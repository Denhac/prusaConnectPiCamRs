//! Where frames come from. `rpicam` keeps rpicam-vid streaming and hands out its newest
//! frame; `file` serves a fixed JPEG, for testing away from a Pi.

pub mod file;
pub mod h264;
pub mod jpeg;
pub mod rpicam;

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;

use crate::config::SourceConfig;

/// Long enough for rpicam-vid to start and let exposure settle at a low frame rate.
pub const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);

/// A still image, numbered in the order the camera made them.
#[derive(Clone)]
pub struct Still {
    pub jpeg: Arc<[u8]>,
    pub seq: u64,
    pub captured: Instant,
    /// The same moment by the wall clock, for clients that cache by date.
    pub wall: SystemTime,
}

/// Shared by every output of a camera, each calling it on its own schedule.
pub trait FrameSource: Send + Sync {
    /// Returns a JPEG captured after this call started, waiting at most `timeout`. Fails with
    /// [`Paused`] while someone watches live view.
    fn capture(&self, timeout: Duration) -> Result<Arc<[u8]>>;
    /// The newest still, without waiting; none before the first.
    fn latest(&self) -> Option<Still>;
    /// Waits at most `timeout` for a still newer than the one numbered `seq`. Fails with
    /// [`Paused`] while someone watches live view.
    fn newer(&self, seq: u64, timeout: Duration) -> Result<Still>;
}

/// Snapshots are paused because the camera is streaming live view.
#[derive(Debug)]
pub struct Paused;

impl fmt::Display for Paused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("snapshots are paused while someone watches live view")
    }
}

impl std::error::Error for Paused {}

pub fn open(config: &SourceConfig) -> Result<Arc<dyn FrameSource>> {
    Ok(match config {
        SourceConfig::Rpicam(config) => Arc::new(rpicam::Rpicam::start(config)?),
        SourceConfig::File(config) => Arc::new(file::FileSource::new(&config.path)),
    })
}
