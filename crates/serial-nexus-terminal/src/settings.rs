//! The human terminal's own settings, persisted between runs.
//!
//! The file is small JSON so it stays readable and hand-editable. It belongs
//! to the terminal, not the core: each human's preferences are their own.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serial_nexus_protocol::DEFAULT_BAUD;

/// Environment variable naming an explicit settings file.
pub const SETTINGS_ENV: &str = "SERIAL_NEXUS_TERMINAL_SETTINGS";

/// Same bound the baud prompt accepts.
pub const MAX_BAUD: u32 = 4_000_000;

/// Preferences the terminal keeps between runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalSettings {
    /// Rate offered when the baud prompt is left empty.
    pub default_baud: u32,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            default_baud: DEFAULT_BAUD,
        }
    }
}

impl TerminalSettings {
    /// Reject values the baud prompt would reject, so a hand-edited or stale
    /// file cannot make the terminal unusable.
    fn sanitise(&mut self) {
        if !(1..=MAX_BAUD).contains(&self.default_baud) {
            self.default_baud = DEFAULT_BAUD;
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

/// The settings file to use, if this machine has a place for one.
pub fn default_path() -> Option<PathBuf> {
    let explicit = std::env::var_os(SETTINGS_ENV).map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    settings_path(explicit, xdg, home)
}

/// The pure form of [`default_path`]: inputs are explicit so the naming can be
/// tested without touching the environment.
fn settings_path(
    explicit: Option<PathBuf>,
    xdg: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(path) = explicit.filter(|path| !path.as_os_str().is_empty()) {
        return Some(path);
    }
    if let Some(dir) = xdg.filter(|dir| !dir.as_os_str().is_empty()) {
        return Some(dir.join("serial-nexus").join("terminal.json"));
    }
    home.filter(|dir| !dir.as_os_str().is_empty()).map(|dir| {
        dir.join(".config")
            .join("serial-nexus")
            .join("terminal.json")
    })
}

/// Load settings from `path`. A missing file means the defaults.
pub fn load(path: &Path) -> Result<TerminalSettings, SettingsError> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let mut settings: TerminalSettings = serde_json::from_str(&text)?;
            settings.sanitise();
            Ok(settings)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(TerminalSettings::default()),
        Err(error) => Err(error.into()),
    }
}

/// Save settings to `path`, creating its directory. The write goes to a
/// sibling temporary file first so an interrupted save cannot truncate the
/// settings the terminal loaded.
pub fn save(path: &Path, settings: &TerminalSettings) -> Result<(), SettingsError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let text = serde_json::to_string_pretty(settings)? + "\n";
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "terminal.json".to_string());
    let temporary = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&temporary, text)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn a_missing_file_loads_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let settings = load(&dir.path().join("terminal.json")).expect("missing file is fine");
        assert_eq!(settings, TerminalSettings::default());
        assert_eq!(settings.default_baud, DEFAULT_BAUD);
    }

    #[test]
    fn save_then_load_round_trips_and_creates_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/config/terminal.json");
        let settings = TerminalSettings {
            default_baud: 19_200,
        };
        save(&path, &settings).expect("save");
        assert_eq!(load(&path).expect("load"), settings);
    }

    #[test]
    fn an_invalid_file_is_an_error_not_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terminal.json");
        std::fs::write(&path, "this is not json").unwrap();
        assert!(load(&path).is_err());
    }

    #[test]
    fn an_out_of_range_rate_falls_back_to_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terminal.json");
        for bad in ["0", "4000001"] {
            std::fs::write(&path, format!("{{\"default_baud\":{bad}}}")).unwrap();
            assert_eq!(
                load(&path).expect("load").default_baud,
                DEFAULT_BAUD,
                "default_baud {bad} should be ignored"
            );
        }
    }

    #[test]
    fn unknown_keys_are_ignored_and_missing_ones_take_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terminal.json");
        std::fs::write(&path, "{\"future\":true}").unwrap();
        assert_eq!(load(&path).expect("load"), TerminalSettings::default());
        std::fs::write(&path, "{\"future\":true,\"default_baud\":9600}").unwrap();
        assert_eq!(load(&path).expect("load").default_baud, 9600);
    }

    #[test]
    fn the_path_prefers_the_environment_then_xdg_then_home() {
        let explicit = Path::new("/tmp/own.json");
        assert_eq!(
            settings_path(
                Some(explicit.to_path_buf()),
                Some(PathBuf::from("/xdg")),
                Some(PathBuf::from("/home/a"))
            ),
            Some(explicit.to_path_buf())
        );
        assert_eq!(
            settings_path(
                None,
                Some(PathBuf::from("/xdg")),
                Some(PathBuf::from("/home/a"))
            ),
            Some(PathBuf::from("/xdg/serial-nexus/terminal.json"))
        );
        assert_eq!(
            settings_path(None, None, Some(PathBuf::from("/home/a"))),
            Some(PathBuf::from("/home/a/.config/serial-nexus/terminal.json"))
        );
        assert_eq!(settings_path(None, None, None), None);
    }
}
