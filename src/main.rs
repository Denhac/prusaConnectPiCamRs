mod config;
mod hub;
mod net;
mod outputs;
mod runner;
mod secret;
mod source;
mod status;
mod uploader;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::outputs::connect_cloud::fingerprint;

#[derive(Parser)]
#[command(
    version,
    about = "Sends Raspberry Pi camera pictures to Prusa Connect and self-hosted prusa-connect servers"
)]
struct Cli {
    /// Config file to use.
    #[arg(
        short,
        long,
        global = true,
        default_value = "/etc/prusa-connect-camera/config.toml"
    )]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Upload snapshots from every configured camera until stopped.
    Run,
    /// Capture one frame, send it to each of the camera's outputs, and report what each answered.
    Test {
        /// Camera id from the config. Optional when only one camera is configured.
        #[arg(long)]
        camera: Option<String>,
    },
    /// Capture one frame and save it locally without uploading.
    Snapshot {
        /// Camera id from the config. Optional when only one camera is configured.
        #[arg(long)]
        camera: Option<String>,
        /// Where to write the JPEG.
        #[arg(short, long, default_value = "snapshot.jpg")]
        output: PathBuf,
    },
    /// List the cameras rpicam-vid can see, with their indexes and sensor modes.
    ListCameras,
    /// Show a camera's Prusa Connect fingerprint, or import one it is already registered with.
    Fingerprint {
        #[command(subcommand)]
        action: FingerprintAction,
    },
    /// Show what each output of the running service is doing. Exits 0 when all are uploading
    /// or paused, 2 when a server refused credentials, 3 when an output keeps failing for
    /// another reason, 4 while one is still starting, and 1 when there is no status to read.
    Status {
        /// Wait up to this many seconds for every output to upload, or for a refusal.
        #[arg(long, default_value_t = 0)]
        wait: u64,
        /// The status file the service writes.
        #[arg(long, default_value = status::DEFAULT_PATH)]
        file: PathBuf,
        /// Print nothing; only the exit code tells.
        #[arg(short, long)]
        quiet: bool,
    },
}

#[derive(Subcommand)]
enum FingerprintAction {
    /// Print the fingerprint this camera uploads with.
    Show {
        #[arg(long)]
        camera: Option<String>,
    },
    /// Use an existing fingerprint, e.g. one registered by another uploader.
    Import {
        value: String,
        #[arg(long)]
        camera: Option<String>,
        /// Replace a fingerprint that is already stored. Prusa Connect will reject
        /// uploads until the camera is deleted and added again.
        #[arg(long)]
        force: bool,
    },
}

fn main() -> ExitCode {
    // The WebRTC crates (rtc, rtc_ice, webrtc, …) log failed connectivity checks and refused
    // relay permissions as errors on every connection, though they're part of normal ICE. Live
    // view logs each session's outcome itself, so they're off unless RUST_LOG asks for them.
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,rtc=off,webrtc=off"),
    )
    // ureq-proto logs protocol state at debug and hex-dumps each request, Token header
    // included, at trace. Neither is useful here, whatever RUST_LOG asks for.
    .filter_module("ureq_proto", log::LevelFilter::Info)
    .init();
    let cli = Cli::parse();
    if let Command::Status { wait, file, quiet } = &cli.command {
        return ExitCode::from(show_status(file, Duration::from_secs(*wait), *quiet));
    }
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let load_config = || Config::load(&cli.config);
    match cli.command {
        Command::Run => runner::run(&load_config()?),
        Command::Test { camera } => {
            let config = load_config()?;
            runner::test(&config, config.camera(camera.as_deref())?)
        }
        Command::Snapshot { camera, output } => {
            let config = load_config()?;
            snapshot(config.camera(camera.as_deref())?, &output)
        }
        Command::ListCameras => source::rpicam::list_cameras(),
        Command::Status { .. } => unreachable!("handled in main"),
        Command::Fingerprint { action } => {
            let config = load_config()?;
            match action {
                FingerprintAction::Show { camera } => {
                    let camera = config.camera(camera.as_deref())?;
                    match fingerprint::load(&config.state_dir, &camera.id)? {
                        Some(value) => println!("{value}"),
                        None => {
                            println!("No fingerprint yet; one is generated on the first upload.")
                        }
                    }
                    Ok(())
                }
                FingerprintAction::Import {
                    value,
                    camera,
                    force,
                } => {
                    let camera = config.camera(camera.as_deref())?;
                    fingerprint::import(&config.state_dir, &camera.id, &value, force)?;
                    println!("Camera {} will upload with fingerprint {value}.", camera.id);
                    Ok(())
                }
            }
        }
    }
}

/// Prints the service's status file, after waiting up to `wait` for every output to upload or for
/// a refusal, and returns the exit code that sums it up.
fn show_status(file: &Path, wait: Duration, quiet: bool) -> u8 {
    let deadline = Instant::now() + wait;
    let (report, code) = loop {
        let (report, code) = match status::read(file) {
            Ok(report) => {
                let code = report.verdict();
                (Some(report), code)
            }
            Err(e) => {
                if !quiet && Instant::now() >= deadline {
                    eprintln!("{e:#}; is the service running?");
                }
                (None, status::exit::UNKNOWN)
            }
        };
        let settled = matches!(code, status::exit::OK | status::exit::AUTH);
        if settled || Instant::now() >= deadline {
            break (report, code);
        }
        thread::sleep(Duration::from_secs(1));
    };
    if !quiet && let Some(report) = report {
        for (label, output) in &report.outputs {
            let state = serde_json::to_value(output.state)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            println!("{label:<24} {state:<9} {}", output.message);
        }
    }
    code
}

fn snapshot(camera: &config::CameraConfig, output: &Path) -> Result<()> {
    let source = source::open(&camera.source)?;
    let jpeg = source.capture(source::CAPTURE_TIMEOUT)?;
    fs::write(output, &jpeg).with_context(|| format!("writing {}", output.display()))?;
    println!(
        "Saved a {} byte snapshot to {}.",
        jpeg.len(),
        output.display()
    );
    Ok(())
}
