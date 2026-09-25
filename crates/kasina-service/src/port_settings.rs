//! Shared persistent ThoughtStream selection used by the service, app, and tray.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct PortSettings {
    thoughtstream_port: Option<String>,
}

/// Standard per-user device preference file.
pub fn settings_path() -> Result<PathBuf> {
    let project = directories::ProjectDirs::from("org", "newkasina", "newKasina")
        .context("no user configuration directory")?;
    Ok(project.config_dir().join("service-devices.json"))
}

/// Load an explicit port; a missing file or `None` selects automatic discovery.
pub fn load(path: &Path) -> Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice::<PortSettings>(&bytes)
            .with_context(|| format!("read {}", path.display()))?
            .thoughtstream_port),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// Atomically persist the chosen port.
pub fn save(path: &Path, port: Option<String>) -> Result<()> {
    let parent = path.parent().context("settings path has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".service-devices-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(&PortSettings {
            thoughtstream_port: port,
        })?)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        crate::recording::replace_file(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("save {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_port_survives_reload_and_can_return_to_automatic() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings/service-devices.json");
        assert_eq!(load(&path).unwrap(), None);
        save(&path, Some("/dev/serial/by-id/test".to_owned())).unwrap();
        assert_eq!(
            load(&path).unwrap().as_deref(),
            Some("/dev/serial/by-id/test")
        );
        save(&path, None).unwrap();
        assert_eq!(load(&path).unwrap(), None);
        fs::write(&path, "broken json").unwrap();
        assert!(load(&path).is_err());
    }
}
