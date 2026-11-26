//! A self-hosted prusa-connect server (github.com/prusa3d/prusa-connect, tested with
//! 0.1.0-beta.1). The camera is declared on it as a `generic` camera with no snapshot address,
//! and this output pushes each still with `PUT /api/v1/cameras/{name}/snapshot`, carrying a
//! bearer token whose account holds `operate` on that camera. Everything this relies on is listed
//! in docs/design/compat.md.

use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use ureq::Agent;
use ureq::http::Uri;

use crate::config::{Token, default_interval, valid_id};
use crate::net::is_tailscale;
use crate::uploader::{REJECTED_CREDENTIALS, REQUEST_TIMEOUT, SendError, StillTarget};
/// How much of an error answer is read, and how much of its message is logged.
const ERROR_BODY_LIMIT: u64 = 4096;
const MESSAGE_LIMIT: usize = 300;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The `[[camera]]` whose pictures this sends. Optional when the config has one camera.
    pub camera: Option<String>,
    /// Names this output in logs and its credential, `<camera>-<id>`. Tells several servers apart.
    #[serde(default = "default_id")]
    pub id: String,
    /// The server's address, e.g. "https://connect.example.org". http needs `insecure_http`.
    pub url: String,
    /// The camera's name on that server.
    pub server_camera: String,
    /// Read the token from this file instead of the systemd credential `<camera>-<id>`.
    pub token_file: Option<PathBuf>,
    /// Seconds between uploads.
    #[serde(default = "default_interval")]
    pub interval: u64,
    /// Allow an http:// address, which sends the token unencrypted. Only for a test server.
    #[serde(default)]
    pub insecure_http: bool,
}

fn default_id() -> String {
    "local".into()
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        let id = &self.id;
        ensure!(
            valid_id(id),
            "output {id:?}: an id may only contain letters, digits, '-' and '_'"
        );
        ensure!(
            self.interval >= 1,
            "output {id}: interval must be at least 1 second"
        );
        ensure!(
            valid_server_camera(&self.server_camera),
            "output {id}: server_camera {:?} may only contain letters, digits, '.', '-' and '_'",
            self.server_camera
        );
        base_url(&self.url, self.insecure_http)
            .with_context(|| format!("output {id}: url {:?}", self.url))?;
        Ok(())
    }
}

/// The camera's name goes into the path of a request that carries the token, so nothing in it may
/// change where the request goes.
fn valid_server_camera(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Checks the server's address and returns it without a trailing '/'. The token is sent to
/// whatever it names, so it may hold no user info, query or fragment.
fn base_url(url: &str, insecure_http: bool) -> Result<String> {
    ensure!(!url.contains('#'), "may not contain a fragment");
    let uri: Uri = url.parse().context("isn't a URL")?;
    match uri.scheme_str() {
        Some("https") => {}
        Some("http") => ensure!(
            insecure_http,
            "is http, which would send the token unencrypted; use https, or set \
             insecure_http = true for a test server"
        ),
        _ => bail!("must start with https://"),
    }
    let authority = uri.authority().context("has no host")?;
    ensure!(
        !authority.as_str().contains('@'),
        "may not contain a user name or password"
    );
    ensure!(uri.query().is_none(), "may not contain a query");
    Ok(url.trim_end_matches('/').to_owned())
}

pub struct ConnectLocal {
    name: String,
    base: String,
    server_camera: String,
    token: Token,
    interval: Duration,
    agent: Agent,
}

impl ConnectLocal {
    /// Sets up the output for camera `camera_id`, reading its token.
    pub fn open(camera_id: &str, config: &Config) -> Result<Self> {
        let token = crate::secret::load(
            &format!("camera {camera_id}, output {}", config.id),
            &format!("{camera_id}-{}", config.id),
            config.token_file.as_deref(),
        )?;
        Self::new(config, token)
    }

    fn new(config: &Config, token: Token) -> Result<Self> {
        let agent = Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .http_status_as_error(false)
            // A redirect could carry the token to another server, so none is followed.
            .max_redirects(0)
            .max_redirects_will_error(false)
            .user_agent(concat!("prusa-connect-camera/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Ok(Self {
            name: config.id.clone(),
            base: base_url(&config.url, config.insecure_http)?,
            server_camera: config.server_camera.clone(),
            token,
            interval: Duration::from_secs(config.interval),
            agent,
        })
    }

    fn host_and_port(&self) -> Option<(String, u16)> {
        let uri: Uri = self.base.parse().ok()?;
        let port = uri
            .port_u16()
            .unwrap_or(if uri.scheme_str() == Some("https") {
                443
            } else {
                80
            });
        Some((uri.host()?.to_owned(), port))
    }

    /// Logs the server's version, from its health check, which needs no credential.
    fn log_version(&self, label: &str) {
        let url = format!("{}/api/v1/health", self.base);
        match self.agent.get(&url).call() {
            Ok(mut response) if response.status().is_success() => {
                let body = response
                    .body_mut()
                    .with_config()
                    .limit(ERROR_BODY_LIMIT)
                    .read_to_string()
                    .unwrap_or_default();
                let version = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v.get("version")?.as_str().map(sanitize));
                match version {
                    Some(version) => {
                        log::info!(
                            "[{label}] sending to {}, prusa-connect {version}",
                            self.base
                        )
                    }
                    None => log::info!("[{label}] sending to {}", self.base),
                }
            }
            Ok(response) => log::warn!(
                "[{label}] {} answered its health check with HTTP {}",
                self.base,
                response.status().as_u16()
            ),
            Err(e) => log::warn!("[{label}] can't reach {}: {e}", self.base),
        }
    }
}

impl StillTarget for ConnectLocal {
    fn name(&self) -> &str {
        &self.name
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    fn start(&self, label: &str) {
        if let Some((host, port)) = self.host_and_port()
            && let Ok(addrs) = (host.as_str(), port).to_socket_addrs()
            && let Some(ip) = addrs.map(|a| a.ip()).find(|&ip| is_tailscale(ip))
        {
            log::warn!(
                "[{label}] {host} resolves to the Tailscale address {ip}; pictures should go over \
                 the LAN, so use the server's LAN name or address"
            );
        }
        self.log_version(label);
    }

    fn send(&self, jpeg: &[u8]) -> Result<u16, SendError> {
        let url = format!(
            "{}/api/v1/cameras/{}/snapshot",
            self.base, self.server_camera
        );
        let mut response = self
            .agent
            .put(&url)
            .header("Authorization", &format!("Bearer {}", self.token.as_str()))
            .content_type("image/jpeg")
            .send(jpeg)
            .map_err(|e| SendError::Retry(format!("request to {} failed: {e}", self.base)))?;
        let status = response.status().as_u16();
        if response.status().is_success() {
            return Ok(status);
        }
        let body = response
            .body_mut()
            .with_config()
            .limit(ERROR_BODY_LIMIT)
            .read_to_string()
            .unwrap_or_default();
        Err(classify(status, &body, &self.server_camera))
    }
}

/// Sorts the server's refusal into what the upload loop does about it.
fn classify(status: u16, body: &str, camera: &str) -> SendError {
    let answer = describe(status, body);
    match status {
        401 | 403 => SendError::Auth(format!(
            "the server {REJECTED_CREDENTIALS} ({answer}); check the token: it may have expired \
             or been revoked, or its account lacks operate on camera {camera}"
        )),
        // The server gives one answer for four causes on purpose. One of them is a server that
        // is restarting, so this is retried like any outage.
        404 => SendError::Retry(format!(
            "the server has no camera {camera} that this token may send to ({answer}): it may be \
             restarting, or the camera isn't declared, isn't a generic camera without a snapshot \
             address, or can't be seen by the token's account"
        )),
        400 | 413 | 415 => SendError::Rejected(answer),
        _ => SendError::Retry(format!("the server answered {answer}")),
    }
}

/// "HTTP 401 core.auth/TOKEN_INVALID: The credential is not one this server accepts", from the
/// server's problem document, or from whatever else it sent.
fn describe(status: u16, body: &str) -> String {
    let problem = serde_json::from_str::<serde_json::Value>(body).ok();
    let field = |name| {
        problem
            .as_ref()
            .and_then(|p| p.get(name)?.as_str())
            .map(sanitize)
            .filter(|s| !s.is_empty())
    };
    let message = field("detail")
        .or_else(|| field("title"))
        .unwrap_or_else(|| sanitize(body.trim()));
    match (field("code"), message.is_empty()) {
        (Some(code), false) => format!("HTTP {status} {code}: {message}"),
        (Some(code), true) => format!("HTTP {status} {code}"),
        (None, false) => format!("HTTP {status}: {message}"),
        (None, true) => format!("HTTP {status}"),
    }
}

/// Makes text from the server safe for one log line: no control characters, and not too long.
fn sanitize(text: &str) -> String {
    let mut clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MESSAGE_LIMIT)
        .collect();
    if text.chars().count() > MESSAGE_LIMIT {
        clean.push('…');
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn config(url: &str, insecure_http: bool) -> Config {
        Config {
            camera: None,
            id: "local".into(),
            url: url.into(),
            server_camera: "camera-pi-5".into(),
            token_file: None,
            interval: 15,
            insecure_http,
        }
    }

    #[test]
    fn checks_the_server_address() {
        assert!(
            config("https://connect.example.org", false)
                .validate()
                .is_ok()
        );
        assert!(
            config("https://connect.example.org:8443/", false)
                .validate()
                .is_ok()
        );
        assert!(config("http://10.0.0.36:8080", false).validate().is_err());
        assert!(config("http://10.0.0.36:8080", true).validate().is_ok());
        assert!(
            config("https://user:pw@connect.example.org", false)
                .validate()
                .is_err()
        );
        assert!(
            config("https://connect.example.org/?x=1", false)
                .validate()
                .is_err()
        );
        assert!(
            config("https://connect.example.org/#x", false)
                .validate()
                .is_err()
        );
        assert!(
            config("ftp://connect.example.org", false)
                .validate()
                .is_err()
        );
        assert!(config("connect.example.org", false).validate().is_err());
    }

    #[test]
    fn checks_the_camera_name() {
        let named = |name: &str| Config {
            server_camera: name.into(),
            ..config("https://connect.example.org", false)
        };
        assert!(named("camera-pi-5").validate().is_ok());
        for bad in ["", ".", "..", "a/b", "a?b", "a b", "a%2fb"] {
            assert!(named(bad).validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn sorts_refusals() {
        let problem = |code: &str, title: &str| {
            format!(r#"{{"code":"{code}","title":"{title}","retryable":false,"status":0}}"#)
        };
        let auth = classify(
            401,
            &problem(
                "core.auth/TOKEN_INVALID",
                "The credential is not one this server accepts",
            ),
            "cam",
        );
        assert!(
            matches!(&auth, SendError::Auth(m) if m.contains("HTTP 401 core.auth/TOKEN_INVALID: The credential")),
            "{auth:?}"
        );
        assert!(matches!(classify(403, "", "cam"), SendError::Auth(_)));
        let missing = classify(
            404,
            &problem("core.validation/RESOURCE_NOT_FOUND", "x"),
            "cam",
        );
        assert!(matches!(&missing, SendError::Retry(m) if m.contains("restarting")));
        assert!(matches!(classify(415, "", "cam"), SendError::Rejected(_)));
        assert!(matches!(classify(413, "", "cam"), SendError::Rejected(_)));
        assert!(
            matches!(classify(502, "Bad Gateway", "cam"), SendError::Retry(m) if m == "the server answered HTTP 502: Bad Gateway")
        );
        assert!(matches!(classify(302, "", "cam"), SendError::Retry(_)));
    }

    #[test]
    fn keeps_server_text_to_one_short_line() {
        assert_eq!(sanitize("two\nlines\x1b[31m"), "two lines [31m");
        let long = "x".repeat(1000);
        assert_eq!(sanitize(&long).chars().count(), MESSAGE_LIMIT + 1);
    }

    fn reply(status: &str, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Answers one request with `response` and returns the request as received.
    fn serve_once(response: String) -> (String, thread::JoinHandle<(String, Vec<u8>)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut head = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            reader.get_mut().write_all(response.as_bytes()).unwrap();
            (head, body)
        });
        (url, handle)
    }

    fn target(url: &str) -> ConnectLocal {
        ConnectLocal::new(&config(url, true), Token::for_test("pcl_test")).unwrap()
    }

    #[test]
    fn pushes_a_still_with_the_token() {
        let (url, server) = serve_once(reply("204 No Content", "", ""));
        assert_eq!(target(&url).send(b"\xff\xd8jpeg").unwrap(), 204);
        let (head, body) = server.join().unwrap();
        assert!(
            head.starts_with("PUT /api/v1/cameras/camera-pi-5/snapshot HTTP/1.1\r\n"),
            "{head}"
        );
        let head = head.to_ascii_lowercase();
        assert!(
            head.contains("authorization: bearer pcl_test\r\n"),
            "{head}"
        );
        assert!(head.contains("content-type: image/jpeg\r\n"), "{head}");
        assert_eq!(body, b"\xff\xd8jpeg");
    }

    #[test]
    fn maps_a_refusal_and_follows_no_redirect() {
        let (url, server) = serve_once(reply(
            "401 Unauthorized",
            "Content-Type: application/problem+json\r\n",
            r#"{"code":"core.auth/TOKEN_INVALID","title":"Not accepted"}"#,
        ));
        assert!(matches!(
            target(&url).send(b"\xff\xd8"),
            Err(SendError::Auth(_))
        ));
        server.join().unwrap();

        let (url, server) = serve_once(reply(
            "307 Temporary Redirect",
            "Location: http://192.0.2.1/steal\r\n",
            "",
        ));
        assert!(matches!(
            target(&url).send(b"\xff\xd8"),
            Err(SendError::Retry(_))
        ));
        server.join().unwrap();
    }
}
