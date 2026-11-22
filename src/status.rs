//! What each output is doing, for `prusa-connect-camera status` and the deploy scripts. The
//! service writes it to `$RUNTIME_DIRECTORY/status.json` whenever an output's state changes, so
//! scripts don't have to read the log.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Where the service unit's `RuntimeDirectory=` puts the file.
pub const DEFAULT_PATH: &str = "/run/prusa-connect-camera/status.json";
const FILE_NAME: &str = "status.json";

/// The exit codes of `status`, which the deploy scripts act on.
pub mod exit {
    /// Every output is uploading, or paused for a reason that needs nothing done.
    pub const OK: u8 = 0;
    /// There is no status to read: the service isn't running, or is an older version.
    pub const UNKNOWN: u8 = 1;
    /// A server refused an output's credentials.
    pub const AUTH: u8 = 2;
    /// An output keeps failing for another reason, e.g. its server is down.
    pub const RETRYING: u8 = 3;
    /// An output hasn't finished its first attempt.
    pub const STARTING: u8 = 4;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Starting,
    Ok,
    Paused,
    Auth,
    Retrying,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputStatus {
    pub state: State,
    pub message: String,
    /// When it entered this state, in seconds since the Unix epoch.
    pub since: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Report {
    pub pid: u32,
    /// By label, e.g. `cam0/cloud`.
    pub outputs: BTreeMap<String, OutputStatus>,
}

impl Report {
    /// The exit code `status` answers with: the worst state among the outputs.
    pub fn verdict(&self) -> u8 {
        let any = |state| self.outputs.values().any(|o| o.state == state);
        if self.outputs.is_empty() || any(State::Starting) {
            exit::STARTING
        } else if any(State::Auth) {
            exit::AUTH
        } else if any(State::Retrying) {
            exit::RETRYING
        } else {
            exit::OK
        }
    }
}

/// The outputs' states, shared by every thread that reports one.
#[derive(Clone)]
pub struct Status(Arc<Mutex<Inner>>);

struct Inner {
    path: Option<PathBuf>,
    report: Report,
    write_failed: bool,
}

impl Status {
    /// Keeps the file in the runtime directory systemd gives the service, if it gives one.
    pub fn from_env() -> Self {
        Self::new(
            std::env::var_os("RUNTIME_DIRECTORY").map(|dir| PathBuf::from(dir).join(FILE_NAME)),
        )
    }

    /// Keeps the file at `path`, or only in memory.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self(Arc::new(Mutex::new(Inner {
            path,
            report: Report {
                pid: std::process::id(),
                outputs: BTreeMap::new(),
            },
            write_failed: false,
        })))
    }

    pub fn register(&self, label: &str) {
        self.set(label, State::Starting, "starting");
    }

    /// Records an output's state. The file is rewritten only when something changed.
    pub fn set(&self, label: &str, state: State, message: impl Into<String>) {
        let message = message.into();
        let mut inner = self.0.lock().unwrap();
        let previous = inner.report.outputs.get(label);
        if previous.is_some_and(|p| p.state == state && p.message == message) {
            return;
        }
        let since = match previous {
            Some(p) if p.state == state => p.since,
            _ => now(),
        };
        inner.report.outputs.insert(
            label.to_owned(),
            OutputStatus {
                state,
                message,
                since,
            },
        );
        inner.write();
    }

    #[cfg(test)]
    pub fn state(&self, label: &str) -> Option<State> {
        self.0
            .lock()
            .unwrap()
            .report
            .outputs
            .get(label)
            .map(|o| o.state)
    }
}

impl Inner {
    /// Replaces the file through a temporary one, so a reader never sees half of it.
    fn write(&mut self) {
        let Some(path) = &self.path else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        let result = serde_json::to_vec_pretty(&self.report)
            .map_err(anyhow::Error::from)
            .and_then(|json| Ok(fs::write(&tmp, json)?))
            .and_then(|()| Ok(fs::rename(&tmp, path)?));
        match result {
            Ok(()) => self.write_failed = false,
            Err(e) if !self.write_failed => {
                log::warn!("can't write {}: {e:#}", path.display());
                self.write_failed = true;
            }
            Err(_) => {}
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn read(path: &Path) -> Result<Report> {
    let text = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&text).with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(states: &[State]) -> Report {
        let mut report = Report::default();
        for (i, &state) in states.iter().enumerate() {
            let status = OutputStatus {
                state,
                message: String::new(),
                since: 0,
            };
            report.outputs.insert(format!("cam0/{i}"), status);
        }
        report
    }

    #[test]
    fn the_worst_state_decides() {
        use State::*;
        assert_eq!(report(&[]).verdict(), exit::STARTING);
        assert_eq!(report(&[Ok, Paused]).verdict(), exit::OK);
        assert_eq!(report(&[Ok, Retrying]).verdict(), exit::RETRYING);
        assert_eq!(report(&[Retrying, Auth]).verdict(), exit::AUTH);
        assert_eq!(report(&[Auth, Starting]).verdict(), exit::STARTING);
    }

    #[test]
    fn writes_the_file_when_a_state_changes() {
        let dir = std::env::temp_dir().join(format!("pcc-status-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        let status = Status::new(Some(path.clone()));
        status.register("cam0/cloud");
        assert_eq!(read(&path).unwrap().verdict(), exit::STARTING);
        status.set("cam0/cloud", State::Ok, "uploading every 15s");
        let report = read(&path).unwrap();
        assert_eq!(report.verdict(), exit::OK);
        assert_eq!(report.pid, std::process::id());
        assert_eq!(report.outputs["cam0/cloud"].message, "uploading every 15s");
        fs::remove_dir_all(dir).unwrap();
    }
}
