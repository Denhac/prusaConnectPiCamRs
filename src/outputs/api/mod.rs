//! A read-only HTTP API on the Pi, for programs that fetch the cameras' pictures, such as a
//! prusa-connect module that posts them to Slack. Like RTSP, it has no password: anyone on a
//! private network may read it, and clients from the internet are refused.
//!
//! `GET /v1/cameras`, `GET /v1/cameras/{id}`, `GET /v1/cameras/{id}/still` and `GET /v1/status`.
//! The contract is in docs/api.md. A still that can't be had now is a 503 with `Retry-After`, so
//! taking stills from live video later only turns some 503s into 200s.

mod http;

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::timeout;

use crate::hub::Hub;
use crate::net::{is_private, is_tailscale, route_source};
use crate::source::{FrameSource, Paused, Still};
use crate::status::{State, Status};
use crate::supervise::{self, Service};
use http::{Request, Response, http_date, rfc3339};

const LABEL: &str = "api";
/// How old a still may be when the request doesn't say.
const DEFAULT_MAX_AGE: Duration = Duration::from_secs(30);
/// The longest `wait` a request may ask for.
const MAX_WAIT: Duration = Duration::from_secs(30);
/// What `Retry-After` suggests when no still can be had.
const RETRY_AFTER: u64 = 15;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONNECTIONS: usize = 32;

/// `[api]`, in the config file or a drop-in.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The address and port to listen on.
    #[serde(default = "default_listen")]
    pub listen: String,
}

fn default_listen() -> String {
    "0.0.0.0:8555".into()
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        self.listen
            .parse::<SocketAddr>()
            .context("api: listen must be address:port, e.g. \"0.0.0.0:8555\"")?;
        Ok(())
    }
}

/// A camera the API serves.
pub struct Camera {
    pub id: String,
    pub name: Option<String>,
    pub stills: Arc<dyn FrameSource>,
    pub hub: Option<Hub>,
}

/// Starts the API on a runtime of its own. `rtsp` is the address the RTSP server listens on, when
/// it serves live video.
pub fn spawn(
    config: &Config,
    cameras: Vec<Camera>,
    rtsp: Option<SocketAddr>,
    status: Status,
) -> Result<Service> {
    let server = Arc::new(Server {
        listen: config.listen.parse()?,
        cameras,
        rtsp,
        connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        refused: Mutex::default(),
        status,
    });
    server.status.register(LABEL);
    supervise::spawn_runtime(
        "api",
        1,
        supervise::restart_on_panic(LABEL.into(), move || Arc::clone(&server).run()),
    )
}

struct Server {
    listen: SocketAddr,
    cameras: Vec<Camera>,
    rtsp: Option<SocketAddr>,
    connections: Arc<Semaphore>,
    refused: Mutex<HashSet<IpAddr>>,
    status: Status,
}

impl Server {
    async fn run(self: Arc<Self>) {
        let listener = loop {
            match TcpListener::bind(self.listen).await {
                Ok(listener) => break listener,
                Err(e) => {
                    let message = format!("can't listen on {}: {e}", self.listen);
                    log::warn!("[{LABEL}] {message}; trying again in 30s");
                    self.status.set(LABEL, State::Retrying, message);
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
        };
        log::info!("[{LABEL}] serving http://{}/v1", self.listen);
        let message = format!("listening on {}", self.listen);
        self.status.set(LABEL, State::Ok, message);
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(e) => {
                    log::warn!("[{LABEL}] accepting a connection failed: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            if !is_private(peer.ip()) {
                if self.refused.lock().unwrap().insert(peer.ip()) {
                    log::warn!(
                        "[{LABEL}] refused {}: only private networks may connect",
                        peer.ip()
                    );
                }
                continue;
            }
            let Ok(permit) = Arc::clone(&self.connections).try_acquire_owned() else {
                // Too many at once: close this one rather than queue it.
                continue;
            };
            let server = Arc::clone(&self);
            tokio::spawn(async move {
                server.connection(stream).await;
                drop(permit);
            });
        }
    }

    async fn connection(&self, stream: TcpStream) {
        let local = stream.local_addr().map_or(self.listen.ip(), |a| a.ip());
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let request = match timeout(READ_TIMEOUT, http::read_request(&mut reader)).await {
            Ok(Ok(Some(request))) => request,
            Ok(Err(_)) => {
                let response = Response::problem(400, "bad_request", "Bad request", "");
                let _ = writer.write_all(&response.render(false)).await;
                return;
            }
            _ => return,
        };
        let head_only = request.method == "HEAD";
        let response = self.respond(&request, local).await;
        let _ = timeout(WRITE_TIMEOUT, writer.write_all(&response.render(head_only))).await;
        let _ = writer.shutdown().await;
    }

    async fn respond(&self, request: &Request, local: IpAddr) -> Response {
        if request.method != "GET" && request.method != "HEAD" {
            return Response::problem(405, "method_not_allowed", "Only GET and HEAD", "")
                .header("Allow", "GET, HEAD".into());
        }
        let segments: Vec<&str> = request
            .path
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        match segments.as_slice() {
            ["v1"] => Response::json(200, &index()),
            ["v1", "status"] => Response::json(200, &self.status.report()),
            ["v1", "cameras"] => {
                let cameras: Vec<CameraInfo> = self
                    .cameras
                    .iter()
                    .map(|c| self.describe(c, local))
                    .collect();
                Response::json(200, &serde_json::json!({ "cameras": cameras }))
            }
            ["v1", "cameras", id] => match self.camera(id) {
                Some(camera) => Response::json(200, &self.describe(camera, local)),
                None => not_found(id),
            },
            ["v1", "cameras", id, "still"] => match self.camera(id) {
                Some(camera) => still(camera, request).await,
                None => not_found(id),
            },
            _ => Response::problem(404, "not_found", "No such resource", &request.path),
        }
    }

    fn camera(&self, id: &str) -> Option<&Camera> {
        self.cameras.iter().find(|c| c.id == id)
    }

    fn describe(&self, camera: &Camera, local: IpAddr) -> CameraInfo {
        let (stream, viewers) = camera.hub.as_ref().map_or((None, 0), |hub| hub.stream());
        let latest = camera.stills.latest();
        CameraInfo {
            id: camera.id.clone(),
            name: camera.name.clone().unwrap_or_else(|| camera.id.clone()),
            state: if stream.is_some() { "video" } else { "stills" },
            still: StillInfo {
                available: latest
                    .as_ref()
                    .is_some_and(|s| s.captured.elapsed() <= DEFAULT_MAX_AGE),
                seq: latest.as_ref().map(|s| s.seq),
                captured_at: latest.as_ref().map(|s| rfc3339(s.wall)),
                age_ms: latest
                    .as_ref()
                    .map(|s| s.captured.elapsed().as_millis() as u64),
            },
            video: stream.map(|s| VideoInfo {
                width: s.width,
                height: s.height,
                fps: s.framerate,
                viewers,
            }),
            rtsp: match (self.rtsp, &camera.hub) {
                (Some(listen), Some(_)) => rtsp_ip(listen.ip(), local, route_source)
                    .map(|ip| format!("rtsp://{}/{}", host(ip, listen.port()), camera.id)),
                _ => None,
            },
        }
    }
}

/// The address to give clients for live video:
/// - the RTSP listener's own address, when it is bound to one
/// - otherwise the address the request reached the Pi on, which the client can reach
/// - but never a Tailscale address, since Tailscale is for administering the Pis and video
///   doesn't go over it: then the Pi's LAN address, which `lan` finds
///
/// `None` when the Pi has no address to give.
fn rtsp_ip(
    listen: IpAddr,
    reached: IpAddr,
    lan: impl FnOnce() -> std::io::Result<IpAddr>,
) -> Option<IpAddr> {
    if !listen.is_unspecified() {
        return Some(listen);
    }
    let reached = reached.to_canonical();
    // A listener on [::] takes IPv4 too, but one on 0.0.0.0 takes no IPv6.
    let same_family = listen.is_ipv6() || reached.is_ipv4();
    if same_family && !is_tailscale(reached) {
        return Some(reached);
    }
    lan().ok().filter(|&ip| !is_tailscale(ip))
}

/// `address:port`, with brackets around an IPv6 address.
fn host(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(v4) => format!("{v4}:{port}"),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => format!("{v4}:{port}"),
            None => format!("[{v6}]:{port}"),
        },
    }
}

fn not_found(id: &str) -> Response {
    Response::problem(404, "camera_not_found", "No camera has that id", id)
}

fn index() -> serde_json::Value {
    serde_json::json!({
        "service": "prusa-connect-camera",
        "version": env!("CARGO_PKG_VERSION"),
        "endpoints": [
            "/v1/cameras",
            "/v1/cameras/{id}",
            "/v1/cameras/{id}/still?max_age=SECONDS&wait=SECONDS",
            "/v1/status",
        ],
    })
}

#[derive(Serialize)]
struct CameraInfo {
    id: String,
    name: String,
    /// `stills` or `video`. More states may come; a client treats one it doesn't know like
    /// `stills`.
    state: &'static str,
    still: StillInfo,
    video: Option<VideoInfo>,
    /// Where live video is served, when it is.
    rtsp: Option<String>,
}

#[derive(Serialize)]
struct StillInfo {
    /// Whether `/still` would answer now without `max_age` or `wait`.
    available: bool,
    seq: Option<u64>,
    captured_at: Option<String>,
    age_ms: Option<u64>,
}

#[derive(Serialize)]
struct VideoInfo {
    width: u32,
    height: u32,
    fps: u32,
    viewers: usize,
}

/// `GET /v1/cameras/{id}/still`: the newest still no older than `max_age` seconds (default 30),
/// or with `wait`, one captured after the request, waiting up to that many seconds (at most 30).
async fn still(camera: &Camera, request: &Request) -> Response {
    let seconds = |name: &str, default: Duration, max: Duration| -> Result<Duration, Response> {
        match request.query(name) {
            None => Ok(default),
            Some(value) => value
                .parse::<u64>()
                .map(|s| Duration::from_secs(s).min(max))
                .map_err(|_| {
                    let detail = format!("{name} must be a whole number of seconds");
                    Response::problem(400, "bad_request", "Bad query", &detail)
                }),
        }
    };
    let max_age = match seconds("max_age", DEFAULT_MAX_AGE, Duration::from_secs(86_400)) {
        Ok(max_age) => max_age,
        Err(response) => return response,
    };
    let wait = match seconds("wait", Duration::ZERO, MAX_WAIT) {
        Ok(wait) => wait,
        Err(response) => return response,
    };
    let streaming = camera
        .hub
        .as_ref()
        .is_some_and(|hub| hub.stream().0.is_some());
    let still = if wait.is_zero() {
        camera
            .stills
            .latest()
            .filter(|s| s.captured.elapsed() <= max_age)
            .ok_or_else(|| unavailable_reason(camera, streaming, max_age))
    } else {
        let after = camera.stills.latest().map_or(0, |s| s.seq);
        let stills = Arc::clone(&camera.stills);
        match tokio::task::spawn_blocking(move || stills.newer(after, wait)).await {
            Ok(Ok(still)) => Ok(still),
            Ok(Err(e)) if e.is::<Paused>() => Err(unavailable_reason(camera, true, max_age)),
            Ok(Err(e)) => Err(format!("{e:#}")),
            Err(_) => Err("the camera's stills couldn't be read".into()),
        }
    };
    match still {
        Ok(still) => serve(still, request),
        Err(reason) => {
            Response::problem(503, "still_unavailable", "No still available now", &reason)
                .header("Retry-After", RETRY_AFTER.to_string())
        }
    }
}

/// Why no still fits. Prose for people; clients branch on the status and `code`.
fn unavailable_reason(camera: &Camera, streaming: bool, max_age: Duration) -> String {
    if streaming {
        "the camera is streaming live video; stills resume when nobody watches".into()
    } else if camera.stills.latest().is_none() {
        "the camera hasn't made a still yet".into()
    } else {
        format!("the newest still is older than {}s", max_age.as_secs())
    }
}

/// The still as JPEG, or 304 when the client already has it. The ETag is the still's number,
/// which, unlike a date, tells apart several stills in one second.
fn serve(still: Still, request: &Request) -> Response {
    let etag = format!("\"{}\"", still.seq);
    if request
        .header("if-none-match")
        .is_some_and(|tags| tags.split(',').any(|tag| tag.trim() == etag))
    {
        return Response::not_modified().header("ETag", etag);
    }
    Response::new(200, "image/jpeg", still.jpeg.to_vec())
        .header("ETag", etag)
        .header("Last-Modified", http_date(still.wall))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    const ANY: &str = "0.0.0.0";
    const LAN: &str = "10.0.0.36";
    const TAILSCALE: &str = "100.88.1.10";

    fn lan() -> std::io::Result<IpAddr> {
        Ok(ip(LAN))
    }

    fn unused() -> std::io::Result<IpAddr> {
        panic!("the LAN address isn't needed here")
    }

    #[test]
    fn rtsp_address_is_the_listeners_when_bound_to_one() {
        assert_eq!(
            rtsp_ip(ip("10.0.0.40"), ip(LAN), unused),
            Some(ip("10.0.0.40"))
        );
        assert_eq!(
            rtsp_ip(ip("127.0.0.1"), ip(TAILSCALE), unused),
            Some(ip("127.0.0.1"))
        );
    }

    #[test]
    fn rtsp_address_is_the_one_the_client_reached() {
        assert_eq!(rtsp_ip(ip(ANY), ip(LAN), unused), Some(ip(LAN)));
        assert_eq!(
            rtsp_ip(ip(ANY), ip("127.0.0.1"), unused),
            Some(ip("127.0.0.1"))
        );
        assert_eq!(
            rtsp_ip(ip(ANY), ip("::ffff:10.0.0.36"), unused),
            Some(ip(LAN))
        );
        assert_eq!(
            rtsp_ip(ip("::"), ip("fd00::5"), unused),
            Some(ip("fd00::5"))
        );
    }

    #[test]
    fn rtsp_address_is_never_tailscale() {
        assert_eq!(rtsp_ip(ip(ANY), ip(TAILSCALE), lan), Some(ip(LAN)));
        assert_eq!(
            rtsp_ip(ip("::"), ip("fd7a:115c:a1e0::1"), lan),
            Some(ip(LAN))
        );
        // The Pi routes the internet through Tailscale, or has no route: no address to give.
        assert_eq!(rtsp_ip(ip(ANY), ip(TAILSCALE), || Ok(ip(TAILSCALE))), None);
        assert_eq!(
            rtsp_ip(ip(ANY), ip(TAILSCALE), || Err(
                std::io::ErrorKind::NetworkUnreachable.into()
            )),
            None
        );
    }

    #[test]
    fn rtsp_address_on_an_ipv4_listener_is_ipv4() {
        assert_eq!(rtsp_ip(ip(ANY), ip("fd00::5"), lan), Some(ip(LAN)));
    }
}
