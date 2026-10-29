//! Prusa Connect binds a camera's token to the fingerprint sent with its first upload, so
//! each camera's fingerprint is generated once and kept in the state directory.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

pub fn load(state_dir: &Path, camera_id: &str) -> Result<Option<String>> {
    let path = path(state_dir, camera_id);
    match fs::read_to_string(&path) {
        Ok(text) => {
            let value = text.trim().to_owned();
            validate(&value).with_context(|| format!("fingerprint in {}", path.display()))?;
            Ok(Some(value))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn load_or_create(state_dir: &Path, camera_id: &str) -> Result<String> {
    if let Some(value) = load(state_dir, camera_id)? {
        return Ok(value);
    }
    let value = uuid::Uuid::new_v4().to_string();
    write(state_dir, camera_id, &value)?;
    log::info!(
        "[{camera_id}] generated fingerprint {value}; Prusa Connect ties it to the token on the first upload"
    );
    Ok(value)
}

pub fn import(state_dir: &Path, camera_id: &str, value: &str, force: bool) -> Result<()> {
    validate(value)?;
    if let Some(existing) = load(state_dir, camera_id)? {
        if existing == value {
            return Ok(());
        }
        if !force {
            bail!(
                "camera {camera_id} already has fingerprint {existing}; pass --force to replace it \
                 (uploads will fail until the camera is deleted and added again in Prusa Connect)"
            );
        }
    }
    write(state_dir, camera_id, value)
}

/// Prusa Connect accepts 16 to 40 letters and digits; UUIDs with hyphens work too.
fn validate(value: &str) -> Result<()> {
    ensure!(
        (16..=40).contains(&value.len())
            && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "a fingerprint must be 16-40 letters, digits or '-', got {value:?}"
    );
    Ok(())
}

fn path(state_dir: &Path, camera_id: &str) -> PathBuf {
    state_dir.join(format!("{camera_id}.fingerprint"))
}

/// Writes through a temporary file so a crash can't leave a half-written fingerprint.
fn write(state_dir: &Path, camera_id: &str, value: &str) -> Result<()> {
    fs::create_dir_all(state_dir).with_context(|| format!("creating {}", state_dir.display()))?;
    let path = path(state_dir, camera_id);
    let tmp = path.with_extension("fingerprint.tmp");
    fs::write(&tmp, format!("{value}\n")).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pcc-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn generated_once_then_reused() {
        let dir = temp_dir("reuse");
        assert_eq!(load(&dir, "cam0").unwrap(), None);
        let first = load_or_create(&dir, "cam0").unwrap();
        assert_eq!(load_or_create(&dir, "cam0").unwrap(), first);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn import_refuses_to_replace_without_force() {
        let dir = temp_dir("import");
        let a = "5702853b-a3e9-47e0-996e-4d0d4699dbbc";
        let b = "fingerprint-myprinter-camera-1";
        import(&dir, "cam0", a, false).unwrap();
        import(&dir, "cam0", a, false).unwrap();
        assert!(import(&dir, "cam0", b, false).is_err());
        import(&dir, "cam0", b, true).unwrap();
        assert_eq!(load(&dir, "cam0").unwrap().as_deref(), Some(b));
        assert!(import(&dir, "cam0", "too-short", true).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
