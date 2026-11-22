//! Tokens and passwords, and where they are read from. A secret never reaches a log: `Debug` is
//! redacted, and nothing here prints its value.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

/// The systemd unit loads every file in /etc/prusa-connect-camera/credentials under this prefix
/// (`LoadCredential=pcc:/etc/prusa-connect-camera/credentials`), so adding an output never means
/// editing the unit.
const DIRECTORY_PREFIX: &str = "pcc_";

/// A token or password. `Debug` is redacted so it can't end up in logs.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
impl Token {
    pub fn for_test(value: &str) -> Self {
        Token(value.into())
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// Reads the secret `name` for `owner` (e.g. "camera cam0"): from `file` when one is given, or
/// else from the systemd credentials, as `pcc_<name>` from the credentials directory or as a
/// credential of its own called `<name>`.
pub fn load(owner: &str, name: &str, file: Option<&Path>) -> Result<Token> {
    let path = match file {
        Some(path) => path.to_owned(),
        None => credential_path(owner, name)?,
    };
    let text = fs::read_to_string(&path)
        .with_context(|| format!("{owner}: reading its token from {}", path.display()))?;
    let token = text.trim();
    ensure!(
        !token.is_empty() && token.bytes().all(|b| b.is_ascii_graphic()),
        "{owner}: the token in {} is empty or has invalid characters",
        path.display()
    );
    Ok(Token(token.to_owned()))
}

fn credential_path(owner: &str, name: &str) -> Result<PathBuf> {
    let Some(dir) = std::env::var_os("CREDENTIALS_DIRECTORY") else {
        bail!(
            "{owner}: no token_file is set and no systemd credentials were passed (put the token \
             in /etc/prusa-connect-camera/credentials/{name})"
        );
    };
    let dir = PathBuf::from(dir);
    let candidates = [
        dir.join(format!("{DIRECTORY_PREFIX}{name}")),
        dir.join(name),
    ];
    for path in &candidates {
        match fs::metadata(path) {
            Ok(_) => return Ok(path.clone()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("{owner}: reading {}", path.display()));
            }
        }
    }
    bail!(
        "{owner}: no credential named {name}; put the token in \
         /etc/prusa-connect-camera/credentials/{name}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_value() {
        let token = Token("pcl_secret".into());
        assert_eq!(format!("{token:?}"), "Token(<redacted>)");
    }

    #[test]
    fn reads_and_checks_a_token_file() {
        let dir = std::env::temp_dir().join(format!("pcc-secret-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good");
        fs::write(&good, "pcl_abc\n").unwrap();
        assert_eq!(load("test", "x", Some(&good)).unwrap().as_str(), "pcl_abc");
        let bad = dir.join("bad");
        fs::write(&bad, "two words\n").unwrap();
        assert!(load("test", "x", Some(&bad)).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
