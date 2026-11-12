//! `<config>/settings.toml`: preferences the app writes. Parsed leniently, key by key, so a hand
//! edit or a file from a newer build never stops an older one from starting.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, value};

use crate::config::Paths;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeepAwake {
    Off,
    #[default]
    PluggedIn,
    Always,
}

impl KeepAwake {
    pub fn as_str(self) -> &'static str {
        match self {
            KeepAwake::Off => "off",
            KeepAwake::PluggedIn => "plugged_in",
            KeepAwake::Always => "always",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    pub keep_awake: KeepAwake,
}

impl Settings {
    pub fn load(paths: &Paths) -> Settings {
        std::fs::read_to_string(paths.settings())
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    fn parse(text: &str) -> Settings {
        let Ok(table) = text.parse::<toml::Table>() else {
            return Settings::default();
        };
        Settings {
            keep_awake: field(&table, "keep_awake"),
        }
    }

    /// Hand-written comments and keys this build doesn't know survive. A file that doesn't parse
    /// is replaced, as `load` already ignored it.
    pub fn save(&self, paths: &Paths) -> Result<()> {
        let file = paths.settings();
        let mut doc = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| text.parse::<DocumentMut>().ok())
            .unwrap_or_default();
        let mut keep_awake = value(self.keep_awake.as_str());
        if let (Some(old), Some(new)) = (
            doc.get("keep_awake").and_then(|item| item.as_value()),
            keep_awake.as_value_mut(),
        ) {
            *new.decor_mut() = old.decor().clone();
        }
        doc["keep_awake"] = keep_awake;

        std::fs::create_dir_all(&paths.config_dir)?;
        let tmp = file.with_extension("toml.tmp");
        std::fs::write(&tmp, doc.to_string())?;
        std::fs::rename(&tmp, &file).with_context(|| format!("writing {}", file.display()))
    }
}

fn field<T: for<'de> Deserialize<'de> + Default>(table: &toml::Table, key: &str) -> T {
    table
        .get(key)
        .and_then(|v| v.clone().try_into().ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(dir: &tempfile::TempDir) -> Paths {
        Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
        }
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::load(&paths(&dir));
        assert_eq!(settings.keep_awake, KeepAwake::PluggedIn);
    }

    #[test]
    fn garbage_gives_defaults() {
        assert_eq!(Settings::parse("not = [toml"), Settings::default());
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let settings = Settings::parse("keep_awake = \"always\"\nfuture = 3\n[later]\nx = 1\n");
        assert_eq!(settings.keep_awake, KeepAwake::Always);
    }

    #[test]
    fn bad_value_gives_that_keys_default() {
        assert_eq!(
            Settings::parse("keep_awake = \"sometimes\"").keep_awake,
            KeepAwake::PluggedIn
        );
        assert_eq!(
            Settings::parse("keep_awake = 1").keep_awake,
            KeepAwake::PluggedIn
        );
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        for mode in [KeepAwake::Off, KeepAwake::Always, KeepAwake::PluggedIn] {
            Settings { keep_awake: mode }.save(&paths).unwrap();
            assert_eq!(Settings::load(&paths).keep_awake, mode);
        }
        assert!(!paths.settings().with_extension("toml.tmp").exists());
    }

    #[test]
    fn save_keeps_comments_and_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(
            paths.settings(),
            "# My settings\nkeep_awake = \"off\"   # off | plugged_in | always\nfuture = 3\n",
        )
        .unwrap();

        Settings {
            keep_awake: KeepAwake::Always,
        }
        .save(&paths)
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(paths.settings()).unwrap(),
            "# My settings\nkeep_awake = \"always\"   # off | plugged_in | always\nfuture = 3\n"
        );
    }

    #[test]
    fn save_replaces_a_file_that_does_not_parse() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(paths.settings(), "not = [toml").unwrap();

        Settings {
            keep_awake: KeepAwake::Off,
        }
        .save(&paths)
        .unwrap();

        assert_eq!(Settings::load(&paths).keep_awake, KeepAwake::Off);
    }
}
