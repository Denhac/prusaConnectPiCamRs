use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

pub use crate::outputs::OutputConfig;
pub use crate::secret::Token;

/// Prusa Connect accepts at most one snapshot per camera this often.
pub const MIN_INTERVAL: Duration = Duration::from_secs(10);

/// The directory beside the config file whose `*.toml` files add outputs, read in name order.
pub const DROP_IN_DIR: &str = "conf.d";

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
    /// Where pictures go besides Prusa Connect, from this file and the drop-ins in `conf.d/`.
    #[serde(rename = "output", default)]
    pub outputs: Vec<OutputConfig>,
    /// Live video over RTSP, for the cameras with video. In this file or one drop-in.
    #[serde(default)]
    pub rtsp: Option<crate::outputs::rtsp::Config>,
}

/// A drop-in file in `conf.d/`. It only adds outputs, so a deploy can add or remove one by
/// writing or deleting a whole file, and an older program, which doesn't read them, is unaffected.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DropIn {
    #[serde(rename = "output", default)]
    outputs: Vec<OutputConfig>,
    #[serde(default)]
    rtsp: Option<crate::outputs::rtsp::Config>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraConfig {
    /// Names the fingerprint file and the systemd credential that holds the token.
    pub id: String,
    /// Camera name shown in Prusa Connect, set on startup when present.
    pub name: Option<String>,
    /// Upload to Prusa Connect (connect.prusa3d.com), configured by the settings below. Off for a
    /// camera that only feeds its other outputs.
    #[serde(default = "default_true")]
    pub cloud: bool,
    /// Seconds between uploads to Prusa Connect.
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

impl RpicamConfig {
    /// The size of the sensor mode live view reads, which gives its streams their shape. Without
    /// a `live_mode`, the snapshot size stands in.
    pub fn live_sensor_size(&self) -> (u32, u32) {
        parse_mode(&self.live_mode).unwrap_or((self.width, self.height))
    }
}

/// Reads the width and height from a `WIDTH:HEIGHT[:…]` sensor mode.
fn parse_mode(mode: &str) -> Option<(u32, u32)> {
    let mut parts = mode.split(':');
    let width = parts.next()?.parse().ok()?;
    let height = parts.next()?.parse().ok()?;
    (width > 0 && height > 0).then_some((width, height))
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

pub fn default_interval() -> u64 {
    15
}

fn default_true() -> bool {
    true
}

/// Whether `id` is fit to name a camera or an output: it ends up in file and credential names.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl Config {
    /// Reads the config file and the drop-ins in the `conf.d` directory beside it.
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut config: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        let dir = path.parent().unwrap_or(Path::new(".")).join(DROP_IN_DIR);
        load_drop_ins(&dir, &mut config)?;
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
                valid_id(id),
                "camera id {id:?} may only contain letters, digits, '-' and '_'"
            );
            ensure!(ids.insert(id), "camera id {id:?} is used more than once");
            ensure!(
                camera.cloud || self.outputs_for(id).next().is_some(),
                "camera {id}: cloud = false and no [[output]] uses it, so it has nowhere to send \
                 pictures"
            );
            ensure!(
                camera.cloud || !camera.live,
                "camera {id}: live = true is live view in Prusa Connect, which needs cloud = true"
            );
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
                ensure!(
                    rpicam.width > 0 && rpicam.height > 0,
                    "camera {id}: width and height must be above 0"
                );
                ensure!(
                    rpicam.live_mode.is_empty() || parse_mode(&rpicam.live_mode).is_some(),
                    "camera {id}: live_mode must be WIDTH:HEIGHT, e.g. \"1640:1232\""
                );
            } else {
                ensure!(
                    !camera.live,
                    "camera {id}: live view needs the rpicam source"
                );
            }
        }
        if let Some(rtsp) = &self.rtsp {
            rtsp.validate()?;
            ensure!(
                self.cameras
                    .iter()
                    .any(|c| matches!(c.source, SourceConfig::Rpicam(_))),
                "rtsp: no camera has video; it needs the rpicam source"
            );
        }
        let mut outputs = HashSet::new();
        for output in &self.outputs {
            let camera = self.camera_of(output)?;
            output.validate()?;
            ensure!(
                outputs.insert((camera, output.id())),
                "camera {camera} has two outputs with id {:?}; give one of them another id",
                output.id()
            );
        }
        Ok(())
    }

    /// The id of the camera whose pictures `output` sends: the one it names, or the only camera.
    fn camera_of<'a>(&'a self, output: &'a OutputConfig) -> Result<&'a str> {
        match (output.camera(), self.cameras.as_slice()) {
            (Some(id), cameras) => {
                ensure!(
                    cameras.iter().any(|c| c.id == id),
                    "an [[output]] names camera {id:?}, which isn't in the config"
                );
                Ok(id)
            }
            (None, [only]) => Ok(&only.id),
            (None, _) => bail!(
                "an [[output]] of type {} has no camera; with several cameras, each output names \
                 one with camera = \"<id>\"",
                output.kind()
            ),
        }
    }

    /// The outputs that send pictures from camera `id`. Call only after validation.
    pub fn outputs_for<'a>(&'a self, id: &'a str) -> impl Iterator<Item = &'a OutputConfig> {
        self.outputs
            .iter()
            .filter(move |output| self.camera_of(output).is_ok_and(|camera| camera == id))
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

    /// The Prusa Connect token: `token_file`, or the systemd credential named after the camera.
    pub fn load_token(&self) -> Result<Token> {
        crate::secret::load(
            &format!("camera {}", self.id),
            &self.id,
            self.token_file.as_deref(),
        )
    }
}

/// Adds the `*.toml` drop-ins in `dir` to `config`, in name order. A missing directory has none.
fn load_drop_ins(dir: &Path, config: &mut Config) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("reading {}", dir.display()))?
            .path();
        if path.extension().is_some_and(|ext| ext == "toml") {
            paths.push(path);
        }
    }
    paths.sort();
    for path in paths {
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let drop_in: DropIn =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.outputs.extend(drop_in.outputs);
        if drop_in.rtsp.is_some() {
            ensure!(
                config.rtsp.is_none(),
                "{}: [rtsp] is set in more than one file",
                path.display()
            );
            config.rtsp = drop_in.rtsp;
        }
    }
    Ok(())
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
    fn finds_the_live_sensor_size() {
        let mut config = RpicamConfig::default();
        assert_eq!(config.live_sensor_size(), (1640, 1232));
        config.live_mode = "2304:1296:10:P".into();
        assert_eq!(config.live_sensor_size(), (2304, 1296));
        config.live_mode = String::new();
        assert_eq!(config.live_sensor_size(), (2048, 1536));
    }

    #[test]
    fn rejects_bad_live_modes() {
        let camera = |mode: &str| {
            parse(&format!(
                "[[camera]]\nid = \"cam0\"\nsource = {{ type = \"rpicam\", live_mode = \"{mode}\" }}"
            ))
        };
        assert!(camera("2304:1296").is_ok());
        assert!(camera("").is_ok());
        assert!(camera("2").is_err());
        assert!(camera("0:1296").is_err());
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

    const ONE_CAMERA: &str = "[[camera]]\nid = \"cam0\"\nsource = { type = \"rpicam\" }\n";
    const LOCAL: &str = "[[output]]\ntype = \"connect-local\"\n\
                         url = \"https://connect.example.org\"\nserver_camera = \"bot\"\n";

    /// Writes a config file and drop-ins to a fresh directory, and loads it.
    fn load_with(main: &str, drop_ins: &[(&str, &str)]) -> Result<Config> {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("pcc-config-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(DROP_IN_DIR)).unwrap();
        fs::write(dir.join("config.toml"), main).unwrap();
        for (name, text) in drop_ins {
            fs::write(dir.join(DROP_IN_DIR).join(name), text).unwrap();
        }
        let result = Config::load(&dir.join("config.toml"));
        fs::remove_dir_all(dir).unwrap();
        result
    }

    #[test]
    fn reads_outputs_from_drop_ins_in_name_order() {
        let second = LOCAL.replace("bot", "other") + "id = \"test\"\n";
        let config = load_with(
            ONE_CAMERA,
            &[
                ("20-test.toml", &second),
                ("10-local.toml", LOCAL),
                ("notes.txt", "ignored"),
            ],
        )
        .unwrap();
        let ids: Vec<_> = config.outputs_for("cam0").map(|o| o.id()).collect();
        assert_eq!(ids, ["local", "test"]);
        assert!(config.cameras[0].cloud);
    }

    #[test]
    fn the_example_drop_in_is_valid() {
        let config = load_with(
            ONE_CAMERA,
            &[("local.toml", include_str!("../config.local.example.toml"))],
        )
        .unwrap();
        assert_eq!(config.outputs_for("cam0").count(), 1);
    }

    #[test]
    fn checks_outputs() {
        // A camera may leave Prusa Connect out, but then needs another output.
        let local_only = ONE_CAMERA.replace("id = \"cam0\"", "id = \"cam0\"\ncloud = false");
        assert!(load_with(&local_only, &[("local.toml", LOCAL)]).is_ok());
        assert!(load_with(&local_only, &[]).is_err());
        // Live view in Prusa Connect needs Prusa Connect.
        let live = local_only.replace("cloud = false", "cloud = false\nlive = true");
        assert!(load_with(&live, &[("local.toml", LOCAL)]).is_err());
        // Two outputs of one camera need different ids.
        assert!(load_with(ONE_CAMERA, &[("a.toml", LOCAL), ("b.toml", LOCAL)]).is_err());
        // With several cameras, an output names its camera, and it must exist.
        let two = format!("{ONE_CAMERA}{}", ONE_CAMERA.replace("cam0", "cam1"));
        assert!(load_with(&two, &[("local.toml", LOCAL)]).is_err());
        let named = format!("{LOCAL}camera = \"cam1\"\n");
        let config = load_with(&two, &[("local.toml", &named)]).unwrap();
        assert_eq!(config.outputs_for("cam1").count(), 1);
        assert_eq!(config.outputs_for("cam0").count(), 0);
        let missing = format!("{LOCAL}camera = \"cam9\"\n");
        assert!(load_with(&two, &[("local.toml", &missing)]).is_err());
        // Drop-ins hold outputs only, and unknown keys and types are errors.
        assert!(load_with(ONE_CAMERA, &[("x.toml", ONE_CAMERA)]).is_err());
        assert!(load_with(ONE_CAMERA, &[("x.toml", &format!("{LOCAL}bogus = 1\n"))]).is_err());
        let unknown = LOCAL.replace("connect-local", "smoke-signals");
        assert!(load_with(ONE_CAMERA, &[("x.toml", &unknown)]).is_err());
        let http = LOCAL.replace("https://", "http://");
        assert!(load_with(ONE_CAMERA, &[("x.toml", &http)]).is_err());
        let insecure = format!("{http}insecure_http = true\n");
        assert!(load_with(ONE_CAMERA, &[("x.toml", &insecure)]).is_ok());
    }
}
