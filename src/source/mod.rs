//! Where frames come from. `rpicam` keeps rpicam-vid streaming and hands out its newest
//! frame; `file` serves a fixed JPEG, for testing away from a Pi.

pub mod file;
pub mod h264;
pub mod jpeg;
pub mod rpicam;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use crate::config::SourceConfig;

/// Long enough for rpicam-vid to start and let exposure settle at a low frame rate.
pub const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);

pub trait FrameSource: Send {
    /// Returns a JPEG captured after this call started, waiting at most `timeout`. Fails with
    /// [`Paused`] while someone watches live view.
    fn capture(&self, timeout: Duration) -> Result<Arc<[u8]>>;
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

pub fn open(config: &SourceConfig) -> Result<Box<dyn FrameSource>> {
    Ok(match config {
        SourceConfig::Rpicam(config) => Box::new(rpicam::Rpicam::start(config)?),
        SourceConfig::File(config) => Box::new(file::FileSource::new(&config.path)),
    })
}
