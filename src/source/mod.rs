//! Where frames come from. `rpicam` keeps rpicam-vid streaming and hands out its newest
//! frame; `file` serves a fixed JPEG, for testing away from a Pi.

pub mod file;
pub mod jpeg;
pub mod rpicam;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use crate::config::SourceConfig;

/// Long enough for rpicam-vid to start and let exposure settle at a low frame rate.
pub const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);

pub trait FrameSource: Send {
    /// Returns a JPEG captured after this call started, waiting at most `timeout`.
    fn capture(&self, timeout: Duration) -> Result<Arc<[u8]>>;
}

pub fn open(config: &SourceConfig) -> Result<Box<dyn FrameSource>> {
    Ok(match config {
        SourceConfig::Rpicam(config) => Box::new(rpicam::Rpicam::start(config)?),
        SourceConfig::File(config) => Box::new(file::FileSource::new(&config.path)),
    })
}
