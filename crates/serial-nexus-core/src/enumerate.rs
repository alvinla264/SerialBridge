use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// A serial device the system can see but the core may not have open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortInfo {
    pub path: String,
    pub description: Option<String>,
}

/// List serial devices under Linux.
///
/// Sources, unioned by device path:
/// - `/dev/serial/by-id/*` symlinks (stable names make good descriptions);
/// - `/sys/class/tty/*` entries with a backing device, except the phantom
///   `serial8250` ports.
///
/// This avoids depending on libudev, so the core builds without system
/// libraries. Pseudo-terminals (used by tests) are not enumerated; they are
/// opened by explicit path.
pub fn list_ports() -> Vec<PortInfo> {
    let mut ports: BTreeMap<String, PortInfo> = BTreeMap::new();

    if let Ok(entries) = fs::read_dir("/dev/serial/by-id") {
        for entry in entries.flatten() {
            let link = entry.path();
            if let Ok(target) = fs::read_link(&link) {
                let path = if target.is_absolute() {
                    target
                } else {
                    link.parent().unwrap_or(Path::new("/")).join(target)
                };
                let path = normalize(&path);
                let description = entry.file_name().to_string_lossy().into_owned();
                ports.insert(
                    path.clone(),
                    PortInfo {
                        path,
                        description: Some(description),
                    },
                );
            }
        }
    }

    if let Ok(entries) = fs::read_dir("/sys/class/tty") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let base = entry.path();
            if !base.join("device").exists() {
                continue;
            }
            if driver_name(&base).as_deref() == Some("serial8250") {
                continue;
            }
            let path = format!("/dev/{name}");
            if !Path::new(&path).exists() {
                continue;
            }
            ports.entry(path.clone()).or_insert(PortInfo {
                path,
                description: None,
            });
        }
    }

    ports.into_values().collect()
}

fn driver_name(tty: &Path) -> Option<String> {
    let link = fs::read_link(tty.join("device/driver")).ok()?;
    Some(link.file_name()?.to_string_lossy().into_owned())
}

/// Resolve a device path to the same identity the engine uses for open
/// devices, so a `by-id` symlink merges with its `/dev/ttyUSB*` target instead
/// of appearing as a second, closed device.
pub fn normalize(path: &Path) -> String {
    std::fs::canonicalize(path)
        .map(|resolved| resolved.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_resolves_symlinks_to_one_identity() {
        let directory = tempfile::TempDir::new().expect("tempdir");
        let device = directory.path().join("ttyUSB9");
        std::fs::write(&device, b"").expect("device file");
        let alias = directory.path().join("by-id-usb-9");
        std::os::unix::fs::symlink(&device, &alias).expect("symlink");

        assert_eq!(normalize(&device), normalize(&alias));
        assert_eq!(normalize(&device), device.to_string_lossy());
    }
}
