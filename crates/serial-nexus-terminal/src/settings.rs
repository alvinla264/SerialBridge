//! The human terminal's own settings, stored between launches.
//!
//! The file is small JSON so it stays readable and hand-editable. It belongs
//! to the terminal, not the core: each human's preferences are their own.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File name under the per-user configuration directory.
pub const SETTINGS_FILE: &str = "terminal.json";

/// Preferences the terminal keeps between launches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalSettings {
    /// Echo keys typed in the stream back into the local view.
    pub local_echo: bool,
    /// Keep device output around for scrolling back.
    pub scrollback: bool,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            local_echo: true,
            scrollback: true,
        }
    }
}

/// Why the settings file could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("invalid settings file: {0}")]
    Json(#[from] serde_json::Error),
}

/// One user's settings file.
#[derive(Clone, Debug)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    /// The store for this user, or `None` when the environment names no
    /// configuration directory.
    pub fn from_env() -> Option<Self> {
        default_path().map(|path| Self { path })
    }

    /// A store for an explicit file (tests, embedding).
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the settings file. `Ok(None)` means the file does not exist yet;
    /// an unreadable or invalid file is an error, never silently replaced.
    pub fn load(&self) -> Result<Option<TerminalSettings>, SettingsError> {
        match fs::read_to_string(&self.path) {
            Ok(text) => Ok(Some(serde_json::from_str(&text)?)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Read the settings file, creating it with the defaults when it does not
    /// exist. An unreadable or invalid file is returned as an error and left
    /// untouched, so a hand-edited file is never silently overwritten.
    pub fn load_or_create(&self) -> Result<TerminalSettings, SettingsError> {
        match self.load()? {
            Some(settings) => Ok(settings),
            None => {
                let settings = TerminalSettings::default();
                match self.save_defaults(&settings) {
                    Ok(()) => Ok(settings),
                    // Another launch created the file first: use theirs.
                    Err(SettingsError::Io(error))
                        if error.kind() == io::ErrorKind::AlreadyExists =>
                    {
                        Ok(self.load()?.unwrap_or(settings))
                    }
                    Err(error) => Err(error),
                }
            }
        }
    }

    /// Write the settings, creating their directory. The write goes to a
    /// sibling staging file first, so an interrupted save cannot truncate the
    /// file the terminal loaded. The staging file is created exclusively, so
    /// an existing entry (even a symlink) cannot redirect the write.
    pub fn save(&self, settings: &TerminalSettings) -> Result<(), SettingsError> {
        self.create_directory()?;
        let text = serde_json::to_string_pretty(settings)? + "\n";
        write_atomic(&self.path, &staging_path(&self.path), &text)
    }

    /// Write the defaults only when no file exists yet: a concurrent launch
    /// that created one first wins, and is never replaced.
    fn save_defaults(&self, settings: &TerminalSettings) -> Result<(), SettingsError> {
        self.create_directory()?;
        let text = serde_json::to_string_pretty(settings)? + "\n";
        let temporary = staging_path(&self.path);
        write_staging(&temporary, &text)?;
        let linked = fs::hard_link(&temporary, &self.path);
        let _ = fs::remove_file(&temporary);
        linked?;
        Ok(())
    }

    fn create_directory(&self) -> Result<(), SettingsError> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        Ok(())
    }
}

/// Write `text` to `temporary`, then rename it onto `path`. The staging file
/// is created exclusively, so an existing entry (even a symlink) cannot
/// redirect the write or make it destroy another file.
fn write_atomic(path: &Path, temporary: &Path, text: &str) -> Result<(), SettingsError> {
    write_staging(temporary, text)?;
    if let Err(error) = fs::rename(temporary, path) {
        let _ = fs::remove_file(temporary);
        return Err(error.into());
    }
    Ok(())
}

/// Write `text` to an exclusively created staging file.
fn write_staging(temporary: &Path, text: &str) -> Result<(), SettingsError> {
    let mut staging = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary)?;
    if let Err(error) = staging
        .write_all(text.as_bytes())
        .and_then(|()| staging.sync_all())
    {
        drop(staging);
        let _ = fs::remove_file(temporary);
        return Err(error.into());
    }
    Ok(())
}

/// The staging file next to `path`. The name is unique to this process and
/// moment, so a leftover staging file from a killed run does not block saves.
fn staging_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| SETTINGS_FILE.to_string());
    let moment = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.{}.{moment}.tmp", std::process::id()))
}

/// The settings file for this user, if the environment names a place for it.
pub fn default_path() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    settings_path(xdg, home)
}

/// The pure form of [`default_path`]: the inputs are explicit so the naming
/// can be tested without touching the environment.
fn settings_path(xdg: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    let directory = match xdg.filter(|dir| !dir.as_os_str().is_empty()) {
        Some(dir) => dir,
        None => home
            .filter(|dir| !dir.as_os_str().is_empty())?
            .join(".config"),
    };
    Some(directory.join("serial-nexus").join(SETTINGS_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn both_off() -> TerminalSettings {
        TerminalSettings {
            local_echo: false,
            scrollback: false,
        }
    }

    #[test]
    fn the_defaults_are_both_on() {
        let settings = TerminalSettings::default();
        assert!(settings.local_echo, "local echo is on by default");
        assert!(settings.scrollback, "scrollback is on by default");
    }

    #[test]
    fn a_missing_file_is_not_an_error_and_is_created_with_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::at(dir.path().join("config/terminal.json"));

        assert_eq!(store.load().expect("missing is fine"), None);
        assert_eq!(
            store.load_or_create().expect("created"),
            TerminalSettings::default()
        );

        // The next launch reads what the first one wrote.
        assert_eq!(
            store.load().expect("load").expect("the file exists"),
            TerminalSettings::default()
        );
    }

    #[test]
    fn settings_round_trip_across_launches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serial-nexus/terminal.json");
        SettingsStore::at(&path)
            .save(&both_off())
            .expect("save both off");

        // A fresh store is a later launch.
        let loaded = SettingsStore::at(&path)
            .load()
            .expect("load")
            .expect("file");
        assert_eq!(loaded, both_off());
    }

    #[test]
    fn unknown_keys_are_ignored_and_missing_fields_stay_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terminal.json");
        std::fs::write(&path, "{\"local_echo\":false,\"future\":true}").unwrap();
        let settings = SettingsStore::at(&path)
            .load()
            .expect("load")
            .expect("file");
        assert!(!settings.local_echo);
        assert!(settings.scrollback, "a missing field keeps its default");
    }

    #[test]
    fn an_invalid_file_is_an_error_and_is_left_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terminal.json");
        let broken = "{\"local_echo\": not a bool";
        std::fs::write(&path, broken).unwrap();

        assert!(SettingsStore::at(&path).load_or_create().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temporary file or replacement is left behind"
        );
    }

    #[test]
    fn an_unreadable_file_is_an_error_and_is_left_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terminal.json");
        std::fs::write(&path, "{\"local_echo\":false}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

        assert!(SettingsStore::at(&path).load_or_create().is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"local_echo\":false}"
        );
    }

    #[test]
    fn save_creates_directories_and_leaves_only_the_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/config/terminal.json");
        SettingsStore::at(&path).save(&both_off()).expect("save");

        assert_eq!(SettingsStore::at(&path).load().unwrap(), Some(both_off()));
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("terminal.json")]);
    }

    #[test]
    fn a_save_that_cannot_reach_its_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("serial-nexus");
        std::fs::write(&blocker, "not a directory").unwrap();
        let store = SettingsStore::at(blocker.join("terminal.json"));

        assert!(store.save(&both_off()).is_err());
        assert_eq!(
            std::fs::read_to_string(&blocker).unwrap(),
            "not a directory"
        );
    }

    #[test]
    fn a_preexisting_staging_entry_cannot_redirect_a_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terminal.json");
        let target = dir.path().join("elsewhere.txt");
        std::fs::write(&target, "sentinel").unwrap();
        let temporary = dir.path().join("staging.tmp");
        std::os::unix::fs::symlink(&target, &temporary).unwrap();

        let result = write_atomic(&path, &temporary, "{}\n");

        assert!(
            result.is_err(),
            "an occupied staging path is a safe failure"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "sentinel");
        assert!(!path.exists(), "nothing was renamed into place");
    }

    #[test]
    fn an_existing_file_is_never_replaced_by_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terminal.json");
        std::fs::write(&path, "{\"local_echo\":false,\"scrollback\":false}").unwrap();

        assert_eq!(
            SettingsStore::at(&path).load_or_create().unwrap(),
            both_off()
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"local_echo\":false,\"scrollback\":false}"
        );
    }

    #[test]
    fn publishing_defaults_refuses_to_replace_a_file_that_appeared_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terminal.json");
        let concurrent = "{\"local_echo\":invalid user edit}";
        std::fs::write(&path, concurrent).unwrap();

        let error = SettingsStore::at(&path)
            .save_defaults(&TerminalSettings::default())
            .expect_err("an existing file is not replaced");

        assert!(
            matches!(&error, SettingsError::Io(error) if error.kind() == io::ErrorKind::AlreadyExists),
            "unexpected error: {error}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), concurrent);
    }

    #[test]
    fn the_path_is_isolated_under_xdg_then_home() {
        assert_eq!(
            settings_path(Some(PathBuf::from("/xdg")), Some(PathBuf::from("/home/a"))),
            Some(PathBuf::from("/xdg/serial-nexus/terminal.json"))
        );
        // An unset or empty XDG_CONFIG_HOME falls back to the home default.
        assert_eq!(
            settings_path(None, Some(PathBuf::from("/home/a"))),
            Some(PathBuf::from("/home/a/.config/serial-nexus/terminal.json"))
        );
        assert_eq!(
            settings_path(Some(PathBuf::new()), Some(PathBuf::from("/home/a"))),
            Some(PathBuf::from("/home/a/.config/serial-nexus/terminal.json"))
        );
        assert_eq!(settings_path(None, None), None);
        assert_eq!(settings_path(Some(PathBuf::new()), None), None);
    }
}
