//! Prusa Connect, the cloud service at connect.prusa3d.com: snapshots through its camera API,
//! and live view as a Prusa "web camera". This module is everything the program knows about
//! it, and it knows nothing of the other outputs. docs/design/compat.md lists what it relies on.

mod client;
pub mod fingerprint;
pub mod live;

use std::time::Duration;

use anyhow::Result;

use crate::config::{CameraConfig, Config, SourceConfig};
use crate::hub::Hub;
use crate::uploader::{SendError, StillTarget};
use client::{Client, ConnectError};

/// One camera's snapshots to Prusa Connect.
pub struct Cloud {
    camera_id: String,
    name: Option<String>,
    interval: Duration,
    printer_check: Option<String>,
    client: Client,
    /// Whether the camera also offers live view, which `/c/info` declares.
    live: bool,
}

/// Sets up `camera`'s Prusa Connect output, reading its token and fingerprint, and its live view
/// when the config offers it and the camera has video (`hub`).
pub fn open(
    config: &Config,
    camera: &CameraConfig,
    hub: Option<&Hub>,
) -> Result<(Cloud, Option<live::Camera>)> {
    let token = camera.load_token()?;
    let fingerprint = fingerprint::load_or_create(&config.state_dir, &camera.id)?;
    let live = match (&camera.source, hub) {
        (SourceConfig::Rpicam(rpicam_config), Some(hub)) if camera.live => Some(live::Camera {
            id: camera.id.clone(),
            name: camera.name.clone().unwrap_or_else(|| camera.id.clone()),
            token: token.clone(),
            fingerprint: fingerprint.clone(),
            hardware: live::hardware(),
            resolution: (rpicam_config.width, rpicam_config.height),
            hub: hub.clone(),
        }),
        _ => None,
    };
    let cloud = Cloud {
        camera_id: camera.id.clone(),
        name: camera.name.clone(),
        interval: camera.interval(),
        printer_check: camera.printer_check.clone(),
        client: Client::new(&config.connect_url, token, fingerprint),
        live: live.is_some(),
    };
    Ok((cloud, live))
}

impl StillTarget for Cloud {
    fn name(&self) -> &str {
        "cloud"
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    /// Prusa Connect discards snapshots while the printer is offline, so none are sent then.
    fn printer_check(&self) -> Option<&str> {
        self.printer_check.as_deref()
    }

    fn send(&self, jpeg: &[u8]) -> Result<u16, SendError> {
        self.client.upload_snapshot(jpeg).map_err(|e| match e {
            ConnectError::Auth { .. } => SendError::Auth(e.to_string()),
            ConnectError::Status {
                status: 400 | 413 | 415,
                ..
            } => SendError::Rejected(e.to_string()),
            _ => SendError::Retry(e.to_string()),
        })
    }

    /// Sets the camera's name in Prusa Connect, and declares live view when it's offered. Called
    /// once uploads succeed, so the camera is registered by then.
    fn on_uploading(&self, label: &str) {
        let Some(name) = self
            .name
            .as_deref()
            .or(self.live.then_some(self.camera_id.as_str()))
        else {
            return;
        };
        match self.client.set_info(name, self.live) {
            Ok(_) => log::info!("[{label}] camera name set to {name:?}"),
            Err(e) => log::warn!("[{label}] couldn't update the camera's details: {e}"),
        }
    }
}
