//! Linux USB webcams via FFmpeg's V4L2 input. Capture always uses the configured webcam
//! mode: live-view presets scale the output rather than requesting unsupported device modes.

use super::process::LiveStream;
use crate::config::UsbConfig;

fn input_args(config: &UsbConfig) -> Vec<String> {
    let mut args: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "warning",
        "-nostdin",
        "-f",
        "v4l2",
    ]
    .map(String::from)
    .into();
    args.extend(["-framerate".into(), config.framerate.to_string()]);
    args.extend([
        "-video_size".into(),
        format!("{}x{}", config.width, config.height),
    ]);
    if !config.input_format.is_empty() {
        args.extend(["-input_format".into(), config.input_format.clone()]);
    }
    args.extend(["-i".into(), config.device.clone(), "-an".into()]);
    args
}

pub(super) fn snapshot_args(config: &UsbConfig) -> Vec<String> {
    let mut args = input_args(config);
    if config.input_format == "mjpeg" {
        // The webcam already supplies JPEGs; avoid decoding and encoding on a small Pi.
        // Some webcams omit standard Huffman tables; supply them for standalone JPEGs.
        args.extend(["-c:v", "copy", "-bsf:v", "mjpeg2jpeg"].map(String::from));
    } else {
        // Raw-only webcams need software JPEG encoding. Encode one frame per second.
        args.extend(
            [
                "-vf", "fps=1", "-c:v", "mjpeg", "-q:v", "5", "-threads", "1",
            ]
            .map(String::from),
        );
    }
    args.extend(["-f", "mjpeg", "-flush_packets", "1", "pipe:1"].map(String::from));
    args
}

pub(super) fn live_args(config: &UsbConfig, stream: LiveStream) -> Vec<String> {
    let mut args = input_args(config);
    args.extend(encode_args(config, stream, false));
    args
}

/// A one-packet input queue and single-threaded decode bound encoder read-ahead. Capture
/// happens separately, so a slow encoder gets recent JPEGs instead of blocking the webcam.
pub(super) fn pipe_live_args(config: &UsbConfig, stream: LiveStream) -> Vec<String> {
    let mut args: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "warning",
        "-nostdin",
        "-thread_queue_size",
        "1",
        "-f",
        "image2pipe",
        "-c:v",
        "mjpeg",
        "-probesize",
        "32",
        "-analyzeduration",
        "0",
        "-threads",
        "1",
    ]
    .map(String::from)
    .into();
    args.extend([
        "-framerate".into(),
        stream.framerate.to_string(),
        "-i".into(),
        "pipe:0".into(),
        "-an".into(),
    ]);
    args.extend(encode_args(config, stream, true));
    args
}

fn encode_args(config: &UsbConfig, stream: LiveStream, latest_only: bool) -> Vec<String> {
    use crate::config::UsbEncoder;
    let filter = if latest_only {
        format!("scale={}:{}", stream.width, stream.height)
    } else {
        format!(
            "fps={},scale={}:{}",
            stream.framerate, stream.width, stream.height
        )
    };
    let mut args = vec!["-vf".into(), filter];
    match config.live_encoder {
        UsbEncoder::Software => {
            args.extend(
                [
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-tune",
                    "zerolatency",
                    "-threads",
                    "1",
                    "-profile:v",
                    "baseline",
                    "-level:v",
                    "4.1",
                    "-x264-params",
                    "repeat-headers=1:aud=1:scenecut=0",
                ]
                .map(String::from),
            );
        }
        UsbEncoder::V4l2 => {
            // Numeric baseline profile works with V4L2 builds that lack named profiles.
            args.extend(
                [
                    "-c:v",
                    "h264_v4l2m2m",
                    "-profile:v",
                    "66",
                    "-level:v",
                    "41",
                    "-bf",
                    "0",
                ]
                .map(String::from),
            );
        }
    }
    // Independent pictures allow stale output to be discarded without broken references.
    let gop = if latest_only { 1 } else { stream.framerate };
    args.extend([
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-b:v".into(),
        stream.bitrate.to_string(),
        "-g".into(),
        gop.to_string(),
        "-vsync".into(),
        "0".into(),
        "-f".into(),
        "h264".into(),
        "-flush_packets".into(),
        "1".into(),
        "pipe:1".into(),
    ]);
    args
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_mjpeg_and_keeps_device_paths_as_one_argument() {
        let config = UsbConfig {
            device: "/dev/a camera".into(),
            ..UsbConfig::default()
        };
        let args = snapshot_args(&config);
        assert!(args.windows(2).any(|a| a == ["-i", "/dev/a camera"]));
        assert!(args.windows(2).any(|a| a == ["-c:v", "copy"]));
        assert!(args.windows(2).any(|a| a == ["-bsf:v", "mjpeg2jpeg"]));
        assert!(!args.iter().any(|a| a == "-vf"));
        assert_eq!(args.last().unwrap(), "pipe:1");
    }

    #[test]
    fn encodes_raw_webcam_frames() {
        let config = UsbConfig {
            input_format: "yuyv422".into(),
            ..UsbConfig::default()
        };
        let args = snapshot_args(&config);
        assert!(args.windows(2).any(|a| a == ["-input_format", "yuyv422"]));
        assert!(args.windows(2).any(|a| a == ["-c:v", "mjpeg"]));
        assert!(args.windows(2).any(|a| a == ["-vf", "fps=1"]));
    }

    #[test]
    fn live_presets_scale_output_without_changing_the_capture_mode() {
        let config = UsbConfig {
            width: 1280,
            height: 720,
            ..UsbConfig::default()
        };
        let args = live_args(
            &config,
            LiveStream {
                width: 640,
                height: 360,
                framerate: 10,
                bitrate: 900_000,
            },
        );
        assert!(args.windows(2).any(|a| a == ["-video_size", "1280x720"]));
        assert!(
            args.windows(2)
                .any(|a| a == ["-vf", "fps=10,scale=640:360"])
        );
        assert!(args.windows(2).any(|a| a == ["-g", "10"]));
        assert!(
            args.iter()
                .any(|a| a == "repeat-headers=1:aud=1:scenecut=0")
        );
    }

    /// Exercise the actual output arguments and frame parsers with real encoders, replacing
    /// only the webcam input with a finite test pattern. No camera or network is needed.
    #[test]
    #[ignore = "requires ffmpeg with libx264; run with cargo test ffmpeg_synthetic_streams -- --ignored"]
    fn ffmpeg_synthetic_streams() {
        use crate::source::{h264::AccessUnits, jpeg::Splitter};

        let raw_config = UsbConfig {
            input_format: "yuyv422".into(),
            ..UsbConfig::default()
        };
        let raw = run_ffmpeg(test_input(snapshot_args(&raw_config), false), None);
        assert_eq!(Splitter::default().push(&raw).len(), 2);
        decode("mjpeg", raw.clone());

        let copied = run_ffmpeg(
            test_input(snapshot_args(&UsbConfig::default()), true),
            Some(raw),
        );
        assert_eq!(Splitter::default().push(&copied).len(), 2);
        decode("mjpeg", copied.clone());
        let piped = run_ffmpeg(
            pipe_live_args(
                &UsbConfig::default(),
                LiveStream {
                    width: 640,
                    height: 480,
                    framerate: 12,
                    bitrate: 900_000,
                },
            ),
            Some(copied),
        );
        let piped_units = AccessUnits::default().push(&piped);
        assert_eq!(piped_units.len(), 1);
        decode("h264", piped_units.concat());

        for stream in [
            LiveStream {
                width: 320,
                height: 240,
                framerate: 10,
                bitrate: 900_000,
            },
            LiveStream {
                width: 1920,
                height: 1080,
                framerate: 12,
                bitrate: 6_500_000,
            },
        ] {
            let live = run_ffmpeg(
                test_input(live_args(&UsbConfig::default(), stream), false),
                None,
            );
            let units = AccessUnits::default().push(&live);
            // The last unit waits for the next frame's header in a continuous stream.
            assert_eq!(units.len(), (stream.framerate * 2 - 1) as usize);
            decode("h264", units.concat());
        }
    }

    #[test]
    fn hardware_encoder_uses_baseline_and_independent_frames_without_x264_options() {
        let config = UsbConfig {
            live_encoder: crate::config::UsbEncoder::V4l2,
            ..UsbConfig::default()
        };
        let args = pipe_live_args(
            &config,
            LiveStream {
                width: 1920,
                height: 1080,
                framerate: 12,
                bitrate: 6_500_000,
            },
        );
        assert!(args.windows(2).any(|a| a == ["-c:v", "h264_v4l2m2m"]));
        assert!(args.windows(2).any(|a| a == ["-profile:v", "66"]));
        assert!(
            !args
                .iter()
                .any(|a| a == "-num_output_buffers" || a == "-num_capture_buffers")
        );
        assert!(args.windows(2).any(|a| a == ["-g", "1"]));
        assert!(!args.iter().any(|a| a == "-preset" || a == "-x264-params"));
        assert!(args.windows(2).any(|a| a == ["-vf", "scale=1920:1080"]));
    }

    fn test_input(mut args: Vec<String>, mjpeg: bool) -> Vec<String> {
        let output_start = args.iter().position(|a| a == "-an").unwrap();
        args.splice(
            ..output_start,
            [
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-f",
                if mjpeg { "mjpeg" } else { "lavfi" },
                "-i",
                if mjpeg {
                    "pipe:0"
                } else {
                    "testsrc2=size=640x480:rate=15:duration=2"
                },
            ]
            .map(String::from),
        );
        args
    }

    fn decode(format: &str, bytes: Vec<u8>) {
        run_ffmpeg(
            [
                "-v", "error", "-xerror", "-f", format, "-i", "pipe:0", "-f", "null", "-",
            ]
            .map(String::from)
            .into(),
            Some(bytes),
        );
    }

    fn run_ffmpeg(args: Vec<String>, input: Option<Vec<u8>>) -> Vec<u8> {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let mut child = Command::new("ffmpeg")
            .args(&args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("starting ffmpeg");
        // Feed input while wait_with_output drains stdout, so neither pipe can deadlock.
        let writer = input.map(|bytes| {
            let mut stdin = child.stdin.take().unwrap();
            std::thread::spawn(move || stdin.write_all(&bytes).unwrap())
        });
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "ffmpeg {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if let Some(writer) = writer {
            writer.join().unwrap();
        }
        output.stdout
    }
}
