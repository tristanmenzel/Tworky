use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::ports::PortMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Creating,
    Ready,
    Failed,
    Cleaning,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssueRef {
    pub id: String,
    pub number: u64,
    pub repo: String,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrRef {
    pub number: u64,
    pub repo: String,
    pub title: String,
    pub url: String,
    pub head: String,
    pub base: String,
}

/// What a work item is for. Items written before issues and PRs could be items are `Custom`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ItemSource {
    #[default]
    Custom,
    Issue(IssueRef),
    PullRequest(PrRef),
}

/// An entry in the work list, known by a random name. It stays until removed, and can't be removed
/// while it has a worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub project: String,
    pub created_at: u64,
    #[serde(default)]
    pub source: ItemSource,
    /// Made to test a project's setup; removed when its worker is cleaned up.
    #[serde(default)]
    pub scratch: bool,
}

impl Item {
    pub fn issue(&self) -> Option<&IssueRef> {
        match &self.source {
            ItemSource::Issue(i) => Some(i),
            _ => None,
        }
    }

    pub fn pull_request(&self) -> Option<&PrRef> {
        match &self.source {
            ItemSource::PullRequest(p) => Some(p),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemRef {
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
}

/// A worker is for exactly one of `issue` or `item`. A pull request item's worker has `item` and `pr`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerRecord {
    pub id: String,
    pub project: String,
    #[serde(default)]
    pub issue: Option<IssueRef>,
    #[serde(default)]
    pub item: Option<ItemRef>,
    #[serde(default)]
    pub pr: Option<PrRef>,
    pub branch: String,
    pub slot: u16,
    pub ports: PortMap,
    pub dir: PathBuf,
    pub compose_project: String,
    pub status: WorkerStatus,
    pub claude_session: String,
    pub claude_started: bool,
    pub error: Option<String>,
    pub created_at: u64,
    #[serde(default)]
    pub worktrees: Vec<WorktreeRecord>,
    /// Set by terminate, cleared by setup: the stack and the session's dev servers are down.
    #[serde(default)]
    pub stopped: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorktreeRecord {
    pub repo: String,
    pub branch: String,
    pub origin: BranchOrigin,
}

/// How the worktree's branch came to exist locally. Cleanup only deletes branches Tworky made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchOrigin {
    /// Created from the base ref.
    New,
    /// Local branch created to track an existing remote branch.
    Tracking,
    /// A local branch that already existed, or a worktree adopted as-is.
    Existing,
}

impl BranchOrigin {
    pub fn created_by_tworky(self) -> bool {
        matches!(self, Self::New | Self::Tracking)
    }
}

impl WorkerRecord {
    /// `owner/repo#12 Title` for an issue, `item brisk-otter: Title` for an item.
    pub fn subject_label(&self) -> String {
        match (&self.issue, &self.item) {
            (Some(i), _) => format!("{}#{} {}", i.repo, i.number, i.title),
            (None, Some(i)) => match &self.pr {
                Some(pr) => format!("{} PR #{} {}", pr.repo, pr.number, pr.title),
                None => format!("item {}: {}", i.name, i.title),
            },
            (None, None) => self.id.clone(),
        }
    }

    pub fn subject_title(&self) -> &str {
        match (&self.issue, &self.item) {
            (Some(i), _) => &i.title,
            (None, Some(i)) => &i.title,
            (None, None) => &self.id,
        }
    }

    pub fn issue_id(&self) -> Option<&str> {
        self.issue.as_ref().map(|i| i.id.as_str())
    }
}

pub struct State {
    path: PathBuf,
}

impl State {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let state = Self {
            path: path.to_path_buf(),
        };
        state.conn()?.execute_batch(
            "CREATE TABLE IF NOT EXISTS workers (
                id TEXT PRIMARY KEY,
                slot INTEGER NOT NULL UNIQUE,
                data TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS items (
                name TEXT PRIMARY KEY,
                data TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS project_sessions (
                project TEXT PRIMARY KEY,
                data TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;
        state.migrate()?;
        Ok(state)
    }

    fn schema_version(&self) -> Result<u32> {
        let v: Option<String> = self
            .conn()?
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.and_then(|v| v.parse().ok()).unwrap_or(0))
    }

    /// Version 1 made the work list a list of items, so every issue a worker holds becomes one.
    fn migrate(&self) -> Result<()> {
        if self.schema_version()? >= 1 {
            return Ok(());
        }
        let items = self.list_items()?;
        let mut taken: HashSet<String> = items.iter().map(|i| i.name.clone()).collect();
        let listed: HashSet<String> = items
            .iter()
            .filter_map(|i| i.issue().map(|r| r.id.clone()))
            .collect();
        for w in self.list()? {
            let Some(issue) = w.issue.clone().filter(|i| !listed.contains(&i.id)) else {
                continue;
            };
            let name = crate::items::random_name(&taken);
            taken.insert(name.clone());
            self.insert_item(&Item {
                name,
                title: issue.title.clone(),
                description: String::new(),
                project: w.project.clone(),
                created_at: w.created_at,
                source: ItemSource::Issue(issue),
                scratch: false,
            })?;
        }
        self.conn()?.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', '1')",
            [],
        )?;
        Ok(())
    }

    fn conn(&self) -> Result<Connection> {
        let conn = Connection::open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(conn)
    }

    pub fn insert(&self, w: &WorkerRecord) -> Result<()> {
        self.conn()?.execute(
            "INSERT INTO workers (id, slot, data) VALUES (?1, ?2, ?3)",
            params![w.id, w.slot, serde_json::to_string(w)?],
        )?;
        Ok(())
    }

    pub fn save(&self, w: &WorkerRecord) -> Result<()> {
        self.conn()?.execute(
            "UPDATE workers SET slot = ?2, data = ?3 WHERE id = ?1",
            params![w.id, w.slot, serde_json::to_string(w)?],
        )?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<WorkerRecord>> {
        let data: Option<String> = self
            .conn()?
            .query_row("SELECT data FROM workers WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        data.map(|d| serde_json::from_str(&d).map_err(Into::into))
            .transpose()
    }

    pub fn list(&self) -> Result<Vec<WorkerRecord>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT data FROM workers ORDER BY slot")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|d| Ok(serde_json::from_str(&d?)?)).collect()
    }

    pub fn used_slots(&self) -> Result<Vec<u16>> {
        Ok(self.list()?.iter().map(|w| w.slot).collect())
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        self.conn()?
            .execute("DELETE FROM workers WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn insert_item(&self, item: &Item) -> Result<()> {
        self.conn()?.execute(
            "INSERT INTO items (name, data) VALUES (?1, ?2)",
            params![item.name, serde_json::to_string(item)?],
        )?;
        Ok(())
    }

    pub fn get_item(&self, name: &str) -> Result<Option<Item>> {
        let data: Option<String> = self
            .conn()?
            .query_row("SELECT data FROM items WHERE name = ?1", [name], |r| {
                r.get(0)
            })
            .optional()?;
        data.map(|d| serde_json::from_str(&d).map_err(Into::into))
            .transpose()
    }

    pub fn list_items(&self) -> Result<Vec<Item>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT data FROM items")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut items: Vec<Item> = rows
            .map(|d| Ok(serde_json::from_str(&d?)?))
            .collect::<Result<_>>()?;
        items.sort_by_key(|i| std::cmp::Reverse(i.created_at));
        Ok(items)
    }

    pub fn save_item(&self, item: &Item) -> Result<()> {
        self.conn()?.execute(
            "UPDATE items SET data = ?2 WHERE name = ?1",
            params![item.name, serde_json::to_string(item)?],
        )?;
        Ok(())
    }

    pub fn delete_item(&self, name: &str) -> Result<()> {
        self.conn()?
            .execute("DELETE FROM items WHERE name = ?1", [name])?;
        Ok(())
    }

    pub fn item_for_issue(&self, issue_id: &str) -> Result<Option<Item>> {
        Ok(self
            .list_items()?
            .into_iter()
            .find(|i| i.issue().is_some_and(|r| r.id == issue_id)))
    }

    pub fn item_for_pull_request(&self, url: &str) -> Result<Option<Item>> {
        Ok(self
            .list_items()?
            .into_iter()
            .find(|i| i.pull_request().is_some_and(|p| p.url == url)))
    }

    pub fn project_session(&self, project: &str) -> Result<Option<ProjectSession>> {
        let data: Option<String> = self
            .conn()?
            .query_row(
                "SELECT data FROM project_sessions WHERE project = ?1",
                [project],
                |r| r.get(0),
            )
            .optional()?;
        data.map(|d| serde_json::from_str(&d).map_err(Into::into))
            .transpose()
    }

    pub fn save_project_session(&self, project: &str, s: &ProjectSession) -> Result<()> {
        self.conn()?.execute(
            "INSERT OR REPLACE INTO project_sessions (project, data) VALUES (?1, ?2)",
            params![project, serde_json::to_string(s)?],
        )?;
        Ok(())
    }
}

/// The Claude conversation that sets up or adjusts a project's config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectSession {
    pub claude_session: String,
    pub claude_started: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn record(id: &str, slot: u16) -> WorkerRecord {
        WorkerRecord {
            id: id.into(),
            project: "acme".into(),
            issue: Some(IssueRef {
                id: "I".into(),
                number: 1,
                repo: "o/r".into(),
                title: "t".into(),
                url: "u".into(),
            }),
            item: None,
            pr: None,
            branch: "feat/1-t".into(),
            slot,
            ports: [("api".to_string(), 4100)].into(),
            dir: PathBuf::from("w"),
            compose_project: "tw-x".into(),
            status: WorkerStatus::Creating,
            claude_session: "s".into(),
            claude_started: false,
            error: None,
            created_at: 0,
            worktrees: vec![],
            stopped: false,
        }
    }

    #[test]
    fn round_trips_and_enforces_unique_slots() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open(&dir.path().join("s.db")).unwrap();
        state.insert(&record("a", 1)).unwrap();
        assert!(state.insert(&record("b", 1)).is_err());
        state.insert(&record("b", 2)).unwrap();
        let mut a = state.get("a").unwrap().unwrap();
        a.status = WorkerStatus::Ready;
        state.save(&a).unwrap();
        assert_eq!(state.get("a").unwrap().unwrap().status, WorkerStatus::Ready);
        assert_eq!(state.used_slots().unwrap(), vec![1, 2]);
        state.delete("a").unwrap();
        assert!(state.get("a").unwrap().is_none());
    }

    #[test]
    fn loads_records_written_before_items_existed() {
        let mut old = serde_json::to_value(record("a", 1)).unwrap();
        old.as_object_mut().unwrap().remove("item");
        let w: WorkerRecord = serde_json::from_value(old).unwrap();
        assert_eq!(w.issue_id(), Some("I"));
        assert!(w.item.is_none());
        assert_eq!(w.subject_label(), "o/r#1 t");
    }

    #[test]
    fn stores_items_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open(&dir.path().join("s.db")).unwrap();
        let item = |name: &str, at| Item {
            name: name.into(),
            title: "Fix audit".into(),
            description: String::new(),
            project: "acme".into(),
            created_at: at,
            source: ItemSource::Custom,
            scratch: false,
        };
        state.insert_item(&item("old-owl", 1)).unwrap();
        state.insert_item(&item("new-elk", 2)).unwrap();
        assert!(state.insert_item(&item("old-owl", 3)).is_err());
        let names: Vec<_> = state
            .list_items()
            .unwrap()
            .into_iter()
            .map(|i| i.name)
            .collect();
        assert_eq!(names, ["new-elk", "old-owl"]);
        assert_eq!(state.get_item("old-owl").unwrap().unwrap().created_at, 1);
        state.delete_item("old-owl").unwrap();
        assert!(state.get_item("old-owl").unwrap().is_none());
    }

    #[test]
    fn reads_items_written_before_sources_existed() {
        let item: Item = serde_json::from_str(
            r#"{"name":"brisk-otter","title":"t","project":"acme","created_at":1}"#,
        )
        .unwrap();
        assert_eq!(item.source, ItemSource::Custom);
        assert!(!item.scratch);
    }

    fn forget_schema_version(state: &State) {
        state
            .conn()
            .unwrap()
            .execute("DELETE FROM meta", [])
            .unwrap();
    }

    #[test]
    fn migration_lists_each_worker_issue_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let state = State::open(&path).unwrap();
        forget_schema_version(&state);
        state.insert(&record("a", 1)).unwrap();
        let mut item_worker = record("b", 2);
        item_worker.issue = None;
        item_worker.item = Some(ItemRef {
            name: "old-owl".into(),
            title: "t".into(),
            description: String::new(),
        });
        state.insert(&item_worker).unwrap();

        let state = State::open(&path).unwrap();
        let items = state.list_items().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].issue().unwrap().id, "I");
        assert_eq!(items[0].project, "acme");

        forget_schema_version(&state);
        let state = State::open(&path).unwrap();
        assert_eq!(state.list_items().unwrap().len(), 1, "already listed");
    }
}
