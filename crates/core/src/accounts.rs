use std::path::Path;

use anyhow::{Context, Result, bail};
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

use crate::config::{AccountConfig, ProjectConfig};
use crate::ports::PortMap;

const CREDENTIAL_SERVICE: &str = "tworky";

/// A `login_origins` entry starting with this names a service, whose origin it stands for.
pub const SERVICE_ORIGIN: &str = "service:";

fn credential(project: &str, account: &str) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(
        CREDENTIAL_SERVICE,
        &format!("{project}/{account}"),
    )?)
}

pub fn set_password(project: &str, account: &str, password: &str) -> Result<()> {
    credential(project, account)?
        .set_password(password)
        .with_context(|| format!("storing the password for {account}"))
}

pub fn password(project: &str, account: &str) -> Result<Option<String>> {
    match credential(project, account)?.get_password() {
        Ok(p) => Ok(Some(p)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn has_password(project: &str, account: &str) -> bool {
    matches!(password(project, account), Ok(Some(_)))
}

pub fn delete_password(project: &str, account: &str) -> Result<()> {
    match credential(project, account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry | keyring::Error::NoStorageAccess(_)) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Rewrites `localhost:<base>` / `127.0.0.1:<base>` to the worker's port for any configured base port.
pub fn rewrite_ports(url: &str, base: &PortMap, worker: &PortMap) -> String {
    let mut out = String::with_capacity(url.len());
    let mut rest = url;
    while let Some(host_end) = ["localhost:", "127.0.0.1:"]
        .iter()
        .filter_map(|h| rest.find(h).map(|i| i + h.len()))
        .min()
    {
        out.push_str(&rest[..host_end]);
        rest = &rest[host_end..];
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        let port: Option<u16> = rest[..digits].parse().ok();
        let mapped = port.and_then(|p| {
            base.iter()
                .find(|(_, b)| **b == p)
                .and_then(|(name, _)| worker.get(name))
        });
        match mapped {
            Some(m) => out.push_str(&m.to_string()),
            None => out.push_str(&rest[..digits]),
        }
        rest = &rest[digits..];
    }
    out.push_str(rest);
    out
}

/// An account URL as a worker sees it: templates rendered, then base ports moved to the worker's.
pub fn worker_url(
    url: &str,
    base: &PortMap,
    worker: &PortMap,
    render: &dyn Fn(&str) -> Result<String>,
) -> Result<String> {
    Ok(rewrite_ports(&render(url)?, base, worker))
}

/// Why workers leave the account out, one reason per service it names that the project doesn't
/// have, or that has no port among `ports`.
pub fn unusable_services(
    project: &ProjectConfig,
    ports: &PortMap,
    account: &AccountConfig,
) -> Vec<String> {
    let named = account.start_service.iter().map(String::as_str).chain(
        account
            .login_origins
            .iter()
            .filter_map(|o| o.strip_prefix(SERVICE_ORIGIN)),
    );
    named
        .filter_map(|name| {
            let name = name.trim();
            let why = match project.service(name).map(|s| &s.port) {
                None => "which doesn't exist".to_string(),
                Some(None) => "which has no port".to_string(),
                Some(Some(port)) if !ports.contains_key(port) => {
                    format!("whose port {port} isn't in [ports]")
                }
                Some(Some(_)) => return None,
            };
            Some(format!(
                "account {} names service {name}, {why}",
                account.name
            ))
        })
        .collect()
}

/// Adds the account, or replaces the one called `previous` (moving its password on a rename).
/// Edits the file in place so comments and the layout of other keys survive.
pub fn save_account(
    project: &ProjectConfig,
    previous: Option<&str>,
    account: &AccountConfig,
) -> Result<()> {
    if account.name.trim().is_empty() {
        bail!("an account needs a name");
    }
    let has_url = !account.start_url.trim().is_empty();
    let has_service = account
        .start_service
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty());
    if has_url == has_service {
        bail!("an account needs either a start URL or a start service");
    }
    if let Some(why) = unusable_services(project, &project.ports, account).first() {
        bail!("{why}");
    }
    let clash = project
        .accounts
        .iter()
        .any(|a| a.name.eq_ignore_ascii_case(&account.name) && Some(a.name.as_str()) != previous);
    if clash {
        bail!(
            "{} already has an account called {}",
            project.name,
            account.name
        );
    }
    edit_accounts(&project.file, |tables| {
        let existing = previous.and_then(|name| {
            tables
                .iter_mut()
                .find(|t| t.get("name").and_then(Item::as_str) == Some(name))
        });
        match existing {
            Some(table) => write_account(table, account),
            None => {
                let mut table = Table::new();
                write_account(&mut table, account);
                tables.push(table);
            }
        }
    })?;
    // Credential Manager matches targets ignoring case, so on Windows a case-only rename
    // already names the same credential and moving it would delete it.
    let same_credential = |old: &str| {
        if cfg!(windows) {
            old.eq_ignore_ascii_case(&account.name)
        } else {
            old == account.name
        }
    };
    if let Some(old) = previous.filter(|old| !same_credential(old))
        && let Some(secret) = password(&project.name, old)?
    {
        set_password(&project.name, &account.name, &secret)?;
        delete_password(&project.name, old)?;
    }
    Ok(())
}

/// Allows the account's password to be filled on `origin` too, unless it already is.
pub fn add_login_origin(project: &ProjectConfig, account: &str, origin: &str) -> Result<()> {
    let mut found = false;
    edit_accounts(&project.file, |tables| {
        let Some(table) = tables
            .iter_mut()
            .find(|t| t.get("name").and_then(Item::as_str) == Some(account))
        else {
            return;
        };
        found = true;
        if !table.contains_key("login_origins") {
            table.insert("login_origins", value(Array::new()));
        }
        if let Some(origins) = table["login_origins"].as_array_mut()
            && !origins.iter().any(|o| o.as_str() == Some(origin))
        {
            origins.push(origin);
        }
    })?;
    if !found {
        bail!("{} has no account called {account}", project.name);
    }
    Ok(())
}

/// How an origin seen on a worker is saved in `login_origins`, so every worker reads it back on
/// its own port: a service's port on `localhost` as `service:<name>`, another worker port on
/// `localhost` or `127.0.0.1` as that host with the base port (which `rewrite_ports` maps), and
/// any other origin as it is.
pub fn login_origin_to_save(project: &ProjectConfig, worker: &PortMap, origin: &str) -> String {
    let literal = origin.trim_end_matches('/').to_string();
    let Some((scheme, authority)) = literal.split_once("://") else {
        return literal;
    };
    let Some((host, port)) = authority.rsplit_once(':') else {
        return literal;
    };
    let Ok(port) = port.parse::<u16>() else {
        return literal;
    };
    if !matches!(host, "localhost" | "127.0.0.1") {
        return literal;
    }
    let Some((name, _)) = worker.iter().find(|(_, p)| **p == port) else {
        return literal;
    };
    let service = project
        .services
        .iter()
        .find(|s| s.port.as_deref() == Some(name.as_str()) && s.https == (scheme == "https"));
    if let Some(service) = service.filter(|_| host == "localhost") {
        return format!("{SERVICE_ORIGIN}{}", service.name);
    }
    match project.ports.get(name) {
        Some(base) => format!("{scheme}://{host}:{base}"),
        None => literal,
    }
}

pub fn remove_account(project: &ProjectConfig, name: &str) -> Result<()> {
    edit_accounts(&project.file, |tables| {
        tables.retain(|t| t.get("name").and_then(Item::as_str) != Some(name))
    })?;
    delete_password(&project.name, name)
}

fn write_account(table: &mut Table, account: &AccountConfig) {
    table.insert("name", value(&account.name));
    set_or_remove(
        table,
        "username",
        (!account.username.is_empty()).then(|| value(&account.username)),
    );
    set_or_remove(
        table,
        "start_url",
        (!account.start_url.is_empty()).then(|| value(&account.start_url)),
    );
    set_or_remove(
        table,
        "start_service",
        account
            .start_service
            .as_ref()
            .filter(|s| !s.is_empty())
            .map(value),
    );
    set_or_remove(table, "mobile", account.mobile.then(|| value(true)));
    set_or_remove(
        table,
        "login_origins",
        (!account.login_origins.is_empty())
            .then(|| value(account.login_origins.iter().collect::<Array>())),
    );
    set_or_remove(table, "color", account.color.as_ref().map(value));
}

fn set_or_remove(table: &mut Table, key: &str, item: Option<Item>) {
    match item {
        Some(item) => {
            table.insert(key, item);
        }
        None => {
            table.remove(key);
        }
    }
}

/// Applies `change` to the file's `[[accounts]]` and writes it atomically, refusing a result
/// that no longer parses as a project.
fn edit_accounts(file: &Path, change: impl FnOnce(&mut ArrayOfTables)) -> Result<()> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("{} does not parse", file.display()))?;
    if !doc.contains_key("accounts") {
        doc.insert("accounts", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    let tables = doc["accounts"]
        .as_array_of_tables_mut()
        .with_context(|| format!("accounts in {} is not a list of tables", file.display()))?;
    change(tables);
    if tables.is_empty() {
        doc.remove("accounts");
    }
    let out = doc.to_string();
    toml::from_str::<ProjectConfig>(&out).context("the edited project config")?;
    let tmp = file.with_extension("toml.tmp");
    std::fs::write(&tmp, &out)?;
    std::fs::rename(&tmp, file).with_context(|| format!("writing {}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ports(pairs: &[(&str, u16)]) -> PortMap {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn rewrites_only_known_ports() {
        let base = ports(&[("web", 8081), ("api", 4000)]);
        let worker = ports(&[("web", 8181), ("api", 4100)]);
        assert_eq!(
            rewrite_ports("http://localhost:8081/", &base, &worker),
            "http://localhost:8181/"
        );
        assert_eq!(
            rewrite_ports(
                "http://127.0.0.1:4000/x?next=http://localhost:40000",
                &base,
                &worker
            ),
            "http://127.0.0.1:4100/x?next=http://localhost:40000"
        );
        assert_eq!(
            rewrite_ports("https://example.com", &base, &worker),
            "https://example.com"
        );
    }

    #[test]
    fn login_origins_are_saved_so_each_worker_maps_them() {
        let project: ProjectConfig = toml::from_str(
            r#"name = "acme"
repos = []
[match]
owner = "acme"
[ports]
web = 5173
auth = 8080
[[services]]
name = "app"
port = "web"
run = "npm run dev"
"#,
        )
        .unwrap();
        let worker = ports(&[("web", 5273), ("auth", 8180)]);
        let save = |origin| login_origin_to_save(&project, &worker, origin);
        assert_eq!(save("http://localhost:5273"), "service:app");
        assert_eq!(save("http://127.0.0.1:5273"), "http://127.0.0.1:5173");
        assert_eq!(save("http://[::1]:5273"), "http://[::1]:5273");
        assert_eq!(save("https://localhost:5273"), "https://localhost:5173");
        assert_eq!(save("http://localhost:8180"), "http://localhost:8080");
        assert_eq!(save("http://localhost:9999"), "http://localhost:9999");
        assert_eq!(
            save("https://login.example.com"),
            "https://login.example.com"
        );
        assert_eq!(
            save("https://login.example.com:8180"),
            "https://login.example.com:8180"
        );
    }

    #[test]
    fn worker_url_renders_then_rewrites() {
        let base = ports(&[("web", 8081)]);
        let worker = ports(&[("web", 8181)]);
        let render = |s: &str| Ok(s.replace("{{ ports.api }}", "4100"));
        assert_eq!(
            worker_url(
                "http://localhost:8081/?api={{ ports.api }}",
                &base,
                &worker,
                &render
            )
            .unwrap(),
            "http://localhost:8181/?api=4100"
        );
    }

    const PROJECT: &str = r#"# Acme
name = "acme" # inline comment
repos = []

[match]
owner = "acme"

[ports]
web = 5173

[[services]]
name = "web"
port = "web"
run = "npm run dev"

[[services]]
name = "queue"
run = "npm run queue"
"#;

    fn project(dir: &Path, text: &str) -> ProjectConfig {
        let file = dir.join("acme.toml");
        std::fs::write(&file, text).unwrap();
        let mut p: ProjectConfig = toml::from_str(text).unwrap();
        p.file = file;
        p
    }

    fn reload(p: &ProjectConfig) -> (String, ProjectConfig) {
        let text = std::fs::read_to_string(&p.file).unwrap();
        let mut loaded: ProjectConfig = toml::from_str(&text).unwrap();
        loaded.file = p.file.clone();
        (text, loaded)
    }

    fn tester() -> AccountConfig {
        AccountConfig {
            name: "Tester 1".into(),
            username: "tester@x".into(),
            start_url: "http://localhost:5173/".into(),
            start_service: None,
            mobile: false,
            login_origins: vec!["https://login.example".into()],
            color: None,
        }
    }

    #[test]
    fn save_adds_and_replaces_keeping_comments() {
        let dir = tempfile::tempdir().unwrap();
        let p = project(dir.path(), PROJECT);
        save_account(&p, None, &tester()).unwrap();
        let (text, p) = reload(&p);
        assert!(text.contains("# Acme") && text.contains("# inline comment"));
        assert_eq!(p.accounts, vec![tester()]);

        let mut changed = tester();
        changed.mobile = true;
        changed.login_origins.clear();
        save_account(&p, Some("Tester 1"), &changed).unwrap();
        let (text, p) = reload(&p);
        assert!(!text.contains("login_origins"));
        assert_eq!(p.accounts, vec![changed]);
    }

    #[test]
    fn save_refuses_a_duplicate_name() {
        let dir = tempfile::tempdir().unwrap();
        let p = project(dir.path(), PROJECT);
        save_account(&p, None, &tester()).unwrap();
        let (_, p) = reload(&p);
        assert!(save_account(&p, None, &tester()).is_err());
    }

    #[test]
    fn save_refuses_a_name_differing_only_in_case() {
        let dir = tempfile::tempdir().unwrap();
        let p = project(dir.path(), PROJECT);
        let mut admin = tester();
        admin.name = "Admin".into();
        save_account(&p, None, &admin).unwrap();
        let (_, p) = reload(&p);
        admin.name = "admin".into();
        let err = save_account(&p, None, &admin).unwrap_err().to_string();
        assert!(err.contains("already has an account"), "{err}");
        save_account(&p, Some("Admin"), &admin).expect("renaming it only changes its case");
    }

    #[test]
    fn save_switches_between_a_start_url_and_a_start_service() {
        let dir = tempfile::tempdir().unwrap();
        let p = project(dir.path(), PROJECT);
        save_account(&p, None, &tester()).unwrap();
        let (_, p) = reload(&p);

        let mut by_service = tester();
        by_service.start_url.clear();
        by_service.start_service = Some("web".into());
        save_account(&p, Some("Tester 1"), &by_service).unwrap();
        let (text, p) = reload(&p);
        assert!(!text.contains("start_url"), "{text}");
        assert_eq!(p.accounts, vec![by_service]);

        save_account(&p, Some("Tester 1"), &tester()).unwrap();
        let (text, p) = reload(&p);
        assert!(!text.contains("start_service"), "{text}");
        assert_eq!(p.accounts, vec![tester()]);
    }

    #[test]
    fn save_needs_exactly_one_start_page() {
        let dir = tempfile::tempdir().unwrap();
        let p = project(dir.path(), PROJECT);
        let mut neither = tester();
        neither.start_url = " ".into();
        assert!(save_account(&p, None, &neither).is_err());
        let mut both = tester();
        both.start_service = Some("web".into());
        assert!(save_account(&p, None, &both).is_err());
    }

    #[test]
    fn save_refuses_a_missing_or_portless_service() {
        let dir = tempfile::tempdir().unwrap();
        let p = project(dir.path(), PROJECT);
        let mut account = tester();
        account.start_url.clear();
        account.start_service = Some("nope".into());
        let err = save_account(&p, None, &account).unwrap_err().to_string();
        assert!(
            err.contains("names service nope, which doesn't exist"),
            "{err}"
        );
        account.start_service = Some("queue".into());
        let err = save_account(&p, None, &account).unwrap_err().to_string();
        assert!(
            err.contains("names service queue, which has no port"),
            "{err}"
        );

        let mut by_origin = tester();
        by_origin.login_origins.push("service:nope".into());
        let err = save_account(&p, None, &by_origin).unwrap_err().to_string();
        assert!(err.contains("names service nope"), "{err}");
        assert!(reload(&p).1.accounts.is_empty());
    }

    #[test]
    fn adding_a_login_origin_keeps_comments_and_skips_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let p = project(dir.path(), PROJECT);
        let mut bare = tester();
        bare.login_origins.clear();
        save_account(&p, None, &bare).unwrap();
        let (_, p) = reload(&p);

        add_login_origin(&p, "Tester 1", "service:web").unwrap();
        add_login_origin(&p, "Tester 1", "https://login.example").unwrap();
        add_login_origin(&p, "Tester 1", "service:web").unwrap();
        let (text, p) = reload(&p);
        assert!(text.contains("# Acme") && text.contains("# inline comment"));
        assert_eq!(
            p.accounts[0].login_origins,
            vec!["service:web".to_string(), "https://login.example".into()]
        );
        assert!(add_login_origin(&p, "Nobody", "service:web").is_err());
    }

    #[test]
    fn removing_the_last_account_drops_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let p = project(dir.path(), PROJECT);
        save_account(&p, None, &tester()).unwrap();
        let (_, p) = reload(&p);
        edit_accounts(&p.file, |t| t.clear()).unwrap();
        let (text, p) = reload(&p);
        assert!(!text.contains("accounts"));
        assert!(p.accounts.is_empty());
    }
}
