//! Command arguments and discovery for CSI cameras using rpicam-vid.

use std::process::Command;

use anyhow::{Context, Result, bail};

use super::process::LiveStream;
use crate::config::RpicamConfig;

pub(super) fn snapshot_args(config: &RpicamConfig) -> Vec<String> {
    #[rustfmt::skip]
    let mode = [
        "--codec", "mjpeg",
        "--width", &config.width.to_string(),
        "--height", &config.height.to_string(),
        "--framerate", &config.framerate.to_string(),
        "--quality", &config.quality.to_string(),
    ];
    args(config, &mode)
}

pub(super) fn live_args(config: &RpicamConfig, stream: LiveStream) -> Vec<String> {
    let framerate = stream.framerate.to_string();
    #[rustfmt::skip]
    let mode = [
        "--codec", "h264",
        "--profile", "baseline",
        "--level", "4.1",
        // Repeat the parameter sets with every keyframe, and send one every second, so a viewer
        // can start decoding at any point.
        "--inline",
        "--intra", &framerate,
        "--width", &stream.width.to_string(),
        "--height", &stream.height.to_string(),
        "--framerate", &framerate,
        "--bitrate", &stream.bitrate.to_string(),
    ];
    let mut mode = mode.to_vec();
    if !config.live_mode.is_empty() {
        mode.extend(["--mode", &config.live_mode]);
    }
    args(config, &mode)
}

fn args(config: &RpicamConfig, mode: &[&str]) -> Vec<String> {
    #[rustfmt::skip]
    let mut args: Vec<String> = [
        "--camera", &config.index.to_string(),
        "--timeout", "0",
        "--nopreview",
        // Level 1 prints a line per frame, which would flood the journal.
        "--verbose", "0",
    ]
    .map(String::from)
    .into();
    args.extend(mode.iter().map(|s| s.to_string()));
    #[rustfmt::skip]
    args.extend([
        "--rotation".into(), config.rotation.to_string(),
        // Write each frame out whole instead of leaving its tail in stdio's buffer.
        "--flush".into(),
    ]);
    if config.hflip {
        args.push("--hflip".into());
    }
    if config.vflip {
        args.push("--vflip".into());
    }
    if let Some(position) = config.lens_position {
        args.extend(["--autofocus-mode".into(), "manual".into()]);
        args.extend(["--lens-position".into(), position.to_string()]);
    }
    args.extend(config.extra_args.iter().cloned());
    args.extend(["--output".into(), "-".into()]);
    args
}

pub fn list_cameras() -> Result<()> {
    let status = Command::new("rpicam-vid")
        .arg("--list-cameras")
        .status()
        .context(
            "running rpicam-vid (on Raspberry Pi OS Lite: sudo apt install rpicam-apps-lite)",
        )?;
    if !status.success() {
        bail!("rpicam-vid --list-cameras failed ({status})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> RpicamConfig {
        RpicamConfig {
            index: 1,
            rotation: 180,
            lens_position: Some(3.0),
            extra_args: vec!["--denoise".into(), "cdn_off".into()],
            ..RpicamConfig::default()
        }
    }

    #[test]
    fn builds_snapshot_arguments() {
        let args = snapshot_args(&config()).join(" ");
        assert!(args.starts_with("--camera 1 --timeout 0 "));
        assert!(args.contains(
            "--codec mjpeg --width 2048 --height 1536 --framerate 1 --quality 50 --rotation 180"
        ));
        assert!(
            args.ends_with(
                "--autofocus-mode manual --lens-position 3 --denoise cdn_off --output -"
            )
        );
        assert!(!args.contains("--hflip"));
    }

    #[test]
    fn builds_live_arguments() {
        let stream = LiveStream {
            width: 1280,
            height: 960,
            framerate: 12,
            bitrate: 2_500_000,
        };
        let args = live_args(&config(), stream).join(" ");
        assert!(args.contains(
            "--codec h264 --profile baseline --level 4.1 --inline --intra 12 --width 1280 \
             --height 960 --framerate 12 --bitrate 2500000 --mode 1640:1232 --rotation 180 --flush"
        ));
        assert!(args.ends_with("--output -"));
    }
}
