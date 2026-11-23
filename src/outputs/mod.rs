//! Where pictures go. Each kind of server is a module of its own, and it alone knows that
//! server's protocol: the rest of the program sees its config and a [`StillTarget`]. A server
//! that changes its API means changing that one module.
//!
//! Prusa Connect (`connect_cloud`) is configured in the camera's own section, for compatibility
//! with configs from before outputs; every other kind is an `[[output]]`.

pub mod api;
pub mod connect_cloud;
pub mod connect_local;
pub mod rtsp;

use anyhow::Result;
use serde::Deserialize;

use crate::uploader::StillTarget;

/// One `[[output]]`, from the config file or a drop-in in `conf.d/`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum OutputConfig {
    /// A self-hosted prusa-connect server.
    ConnectLocal(connect_local::Config),
}

impl OutputConfig {
    /// The output's type, as `type =` spells it.
    pub fn kind(&self) -> &'static str {
        match self {
            OutputConfig::ConnectLocal(_) => "connect-local",
        }
    }

    /// The camera it names, if any.
    pub fn camera(&self) -> Option<&str> {
        match self {
            OutputConfig::ConnectLocal(config) => config.camera.as_deref(),
        }
    }

    /// Tells it apart from the camera's other outputs, in logs and credential names.
    pub fn id(&self) -> &str {
        match self {
            OutputConfig::ConnectLocal(config) => &config.id,
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            OutputConfig::ConnectLocal(config) => config.validate(),
        }
    }

    /// Sets the output up to send pictures from camera `camera_id`, reading its credentials.
    pub fn open(&self, camera_id: &str) -> Result<Box<dyn StillTarget>> {
        Ok(match self {
            OutputConfig::ConnectLocal(config) => {
                Box::new(connect_local::ConnectLocal::open(camera_id, config)?)
            }
        })
    }
}
