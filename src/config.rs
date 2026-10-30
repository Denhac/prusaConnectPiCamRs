use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

/// Prusa Connect accepts at most one snapshot per camera this often.
pub const MIN_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_connect_url")]
    pub connect_url: String,
    /// Prusa's camera signaling server, used for live view.
    #[serde(default = "default_signaling_url")]
    pub signaling_url: String,
    /// Holds each camera's fingerprint.
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    #[serde(rename = "camera", default)]
    pub cameras: Vec<CameraConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraConfig {
    /// Names the fingerprint file and the systemd credential that holds the token.
    pub id: String,
    /// Camera name shown in Prusa Connect, set on startup when present.
    pub name: Option<String>,
    /// Seconds between uploads.
    #[serde(default = "default_interval")]
    pub interval: u64,
    /// `host:port` that must accept a TCP connection before each upload, such as the
    /// printer's web interface. Prusa Connect discards snapshots while the printer is offline.
    pub printer_check: Option<String>,
    /// File holding the Prusa Connect token. Defaults to the systemd credential named after `id`.
    pub token_file: Option<PathBuf>,
    /// Offer live view in Prusa Connect. Needs a "Web camera" token and the rpicam source;
    /// snapshots pause while someone watches.
    #[serde(default)]
    pub live: bool,
    pub source: SourceConfig,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SourceConfig {
    Rpicam(RpicamConfig),
    File(FileConfig),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RpicamConfig {
    /// Program to run; `libcamera-vid` on Raspberry Pi OS releases before Bookworm.
    pub command: String,
    /// Camera index from `prusa-connect-camera list-cameras`.
    pub index: u32,
    /// Output size. The default suits Camera Module v2 (3280x2464), scaled from its full
    /// sensor; sizes the camera can only produce by cropping lose part of the view.
    pub width: u32,
    pub height: u32,
    /// Frames per second to stream. Uploads use the latest frame, so this only needs to be
    /// high enough for exposure and white balance to keep up with lighting changes.
    pub framerate: f32,
    /// JPEG quality, 1-100.
    pub quality: u8,
    /// 0 or 180; the camera pipeline can't rotate by 90 degrees.
    pub rotation: u16,
    pub hflip: bool,
    pub vflip: bool,
    /// Fixes the focus of an autofocus camera (Camera Module 3), in dioptres (1 / metres).
    pub lens_position: Option<f32>,
    /// Sensor mode for live view, as rpicam-vid's `--mode WIDTH:HEIGHT`. The default is Camera
    /// Module v2's 2x2-binned mode, which sees the whole sensor area at half its full resolution
    /// (3280x2464), so live view shows the whole view at every size; left to itself,
    /// rpicam-vid picks a cropped mode for small sizes. Empty lets it choose.
    pub live_mode: String,
    /// Extra rpicam-vid options. Don't repeat options this program already sets.
    pub extra_args: Vec<String>,
}

impl Default for RpicamConfig {
    fn default() -> Self {
        Self {
            command: "rpicam-vid".into(),
            index: 0,
            width: 2048,
            height: 1536,
            framerate: 1.0,
            quality: 50,
            rotation: 0,
            hflip: false,
            vflip: false,
            lens_position: None,
            live_mode: "1640:1232".into(),
            extra_args: Vec::new(),
        }
    }
}

/// Serves the same JPEG for every upload, for testing away from a Pi.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub path: PathBuf,
}

fn default_connect_url() -> String {
    "https://connect.prusa3d.com".into()
}

fn default_signaling_url() -> String {
    "https://camera-signaling.prusa3d.com".into()
}

fn default_state_dir() -> PathBuf {
    "/var/lib/prusa-connect-camera".into()
}

fn default_interval() -> u64 {
    15
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.cameras.is_empty(),
            "the config has no [[camera]] sections"
        );
        let mut ids = HashSet::new();
        for camera in &self.cameras {
            let id = &camera.id;
            ensure!(
                !id.is_empty()
                    && id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "camera id {id:?} may only contain letters, digits, '-' and '_'"
            );
            ensure!(ids.insert(id), "camera id {id:?} is used more than once");
            ensure!(
                camera.interval >= MIN_INTERVAL.as_secs(),
                "camera {id}: interval must be at least {} seconds",
                MIN_INTERVAL.as_secs()
            );
            if let Some(addr) = &camera.printer_check {
                ensure!(
                    addr.rsplit_once(':')
                        .is_some_and(|(_, port)| port.parse::<u16>().is_ok()),
                    "camera {id}: printer_check must be host:port, e.g. \"192.168.1.25:80\""
                );
            }
            if let SourceConfig::Rpicam(rpicam) = &camera.source {
                ensure!(
                    matches!(rpicam.rotation, 0 | 180),
                    "camera {id}: rotation must be 0 or 180"
                );
                ensure!(
                    (1..=100).contains(&rpicam.quality),
                    "camera {id}: quality must be 1-100"
                );
                ensure!(
                    rpicam.framerate > 0.0,
                    "camera {id}: framerate must be above 0"
                );
            } else {
                ensure!(
                    !camera.live,
                    "camera {id}: live view needs the rpicam source"
                );
            }
        }
        Ok(())
    }

    /// Picks a camera by id, or the only camera when `id` is `None`.
    pub fn camera(&self, id: Option<&str>) -> Result<&CameraConfig> {
        match (id, self.cameras.as_slice()) {
            (Some(id), cameras) => cameras
                .iter()
                .find(|c| c.id == id)
                .with_context(|| format!("no camera with id {id:?} in the config")),
            (None, [only]) => Ok(only),
            (None, cameras) => {
                let ids: Vec<_> = cameras.iter().map(|c| c.id.as_str()).collect();
                bail!(
                    "several cameras are configured; choose one with --camera ({})",
                    ids.join(", ")
                )
            }
        }
    }
}

impl CameraConfig {
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval)
    }

    pub fn load_token(&self) -> Result<Token> {
        let path = match &self.token_file {
            Some(path) => path.clone(),
            None => {
                let dir = std::env::var_os("CREDENTIALS_DIRECTORY").with_context(|| {
                    format!(
                        "camera {0}: no token_file is set and no systemd credentials were passed \
                         (LoadCredential={0}:/path/to/token)",
                        self.id
                    )
                })?;
                PathBuf::from(dir).join(&self.id)
            }
        };
        let text = fs::read_to_string(&path).with_context(|| {
            format!("camera {}: reading token from {}", self.id, path.display())
        })?;
        let token = text.trim();
        ensure!(
            !token.is_empty() && token.bytes().all(|b| b.is_ascii_graphic()),
            "camera {}: the token in {} is empty or has invalid characters",
            self.id,
            path.display()
        );
        Ok(Token(token.to_owned()))
    }
}

/// A Prusa Connect camera token. `Debug` is redacted so it can't end up in logs.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config> {
        let config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn rpicam_defaults() {
        let config = parse(
            r#"
            [[camera]]
            id = "cam0"
            source = { type = "rpicam" }
            "#,
        )
        .unwrap();
        assert_eq!(config.connect_url, "https://connect.prusa3d.com");
        let camera = config.camera(None).unwrap();
        assert_eq!(camera.interval(), Duration::from_secs(15));
        let SourceConfig::Rpicam(rpicam) = &camera.source else {
            panic!("expected rpicam")
        };
        assert_eq!(
            (rpicam.width, rpicam.height, rpicam.quality),
            (2048, 1536, 50)
        );
    }

    #[test]
    fn example_config_is_valid() {
        let config = parse(include_str!("../config.example.toml")).unwrap();
        assert_eq!(
            config.camera(None).unwrap().name.as_deref(),
            Some("Printer cam")
        );
    }

    #[test]
    fn rejects_bad_values() {
        let camera = |extra: &str| {
            parse(&format!(
                "[[camera]]\nid = \"cam0\"\n{extra}\n[camera.source]\ntype = \"rpicam\""
            ))
        };
        assert!(camera("interval = 5").is_err());
        assert!(camera("printer_check = \"192.168.1.25\"").is_err());
        assert!(camera("unknown = 1").is_err());
        assert!(
            parse("[[camera]]\nid = \"cam0\"\nsource = { type = \"rpicam\", rotation = 90 }")
                .is_err()
        );
        assert!(
            parse("[[camera]]\nid = \"cam0\"\nsource = { type = \"rpicam\", bogus = 1 }").is_err()
        );
        assert!(parse("[[camera]]\nid = \"../x\"\nsource = { type = \"rpicam\" }").is_err());
    }

    #[test]
    fn picks_camera() {
        let config = parse(
            r#"
            [[camera]]
            id = "a"
            source = { type = "file", path = "a.jpg" }
            [[camera]]
            id = "b"
            source = { type = "file", path = "b.jpg" }
            "#,
        )
        .unwrap();
        assert_eq!(config.camera(Some("b")).unwrap().id, "b");
        assert!(config.camera(None).is_err());
        assert!(config.camera(Some("c")).is_err());
    }
}
