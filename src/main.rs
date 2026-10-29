mod config;
mod connect;
mod fingerprint;
mod runner;
mod source;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::config::Config;

#[derive(Parser)]
#[command(
    version,
    about = "Uploads Raspberry Pi camera snapshots to Prusa Connect"
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
    /// Capture one frame, upload it, and report what Prusa Connect answered.
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
    /// Show a camera's fingerprint, or import one it is already registered with.
    Fingerprint {
        #[command(subcommand)]
        action: FingerprintAction,
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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        // ureq-proto logs protocol state at debug and hex-dumps each request, Token header
        // included, at trace. Neither is useful here, whatever RUST_LOG asks for.
        .filter_module("ureq_proto", log::LevelFilter::Info)
        .init();
    match run(Cli::parse()) {
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
