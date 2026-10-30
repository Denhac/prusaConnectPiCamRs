//! Client for the Prusa Connect camera API used by "other" cameras: `PUT /c/snapshot`
//! uploads a JPEG and `PUT /c/info` changes camera settings. Both authenticate with the
//! camera's `Token` and `Fingerprint` headers.

use std::fmt;
use std::time::Duration;

use ureq::Agent;

use crate::config::Token;

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Client {
    agent: Agent,
    base_url: String,
    token: Token,
    fingerprint: String,
}

#[derive(Debug)]
pub enum ConnectError {
    /// The token was rejected, or the fingerprint isn't the one the camera registered with.
    Auth {
        status: u16,
        message: String,
    },
    /// Any other response outside 2xx, including redirects, which are never followed.
    Status {
        status: u16,
        message: String,
    },
    Transport(ureq::Error),
}

impl ConnectError {
    pub fn is_auth(&self) -> bool {
        matches!(self, ConnectError::Auth { .. })
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::Auth { status, message } => write!(
                f,
                "Prusa Connect rejected the camera's credentials (HTTP {status}: {message}); check \
                 the token, and that the fingerprint is the one this camera was first registered with"
            ),
            ConnectError::Status { status, message } => {
                write!(f, "Prusa Connect answered HTTP {status}: {message}")
            }
            ConnectError::Transport(e) => write!(f, "request to Prusa Connect failed: {e}"),
        }
    }
}

impl std::error::Error for ConnectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConnectError::Transport(e) => Some(e),
            _ => None,
        }
    }
}

impl Client {
    pub fn new(base_url: &str, token: Token, fingerprint: String) -> Self {
        let agent = Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .http_status_as_error(false)
            // Custom headers survive redirects, so following one could send the token elsewhere.
            .max_redirects(0)
            .max_redirects_will_error(false)
            .user_agent(concat!("prusa-connect-camera/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self {
            agent,
            base_url: base_url.trim_end_matches('/').to_owned(),
            token,
            fingerprint,
        }
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Uploads a JPEG snapshot and returns the HTTP status (200 as of October 2026).
    pub fn upload_snapshot(&self, jpeg: &[u8]) -> Result<u16, ConnectError> {
        // "image/jpg" is what the upstream shell script sends and what has been verified to work.
        self.put("/c/snapshot", "image/jpg", jpeg)
    }

    /// Sets the camera's name. A camera offering live view also declares itself a web camera
    /// with live view, as Prusa's browser webcam does.
    pub fn set_info(&self, name: &str, live: bool) -> Result<u16, ConnectError> {
        let mut body = serde_json::json!({ "config": { "name": name } });
        if live {
            body["type"] = "WEB".into();
            body["features"] = serde_json::json!(crate::live::FEATURES);
        }
        self.put("/c/info", "application/json", body.to_string().as_bytes())
    }

    fn put(&self, path: &str, content_type: &str, body: &[u8]) -> Result<u16, ConnectError> {
        let mut response = self
            .agent
            .put(format!("{}{path}", self.base_url))
            .header("Token", self.token.as_str())
            .header("Fingerprint", &self.fingerprint)
            .content_type(content_type)
            .send(body)
            .map_err(ConnectError::Transport)?;
        let status = response.status().as_u16();
        // Success bodies are never read or logged: /c/info echoes the camera's token back.
        if response.status().is_success() {
            return Ok(status);
        }
        let body = response
            .body_mut()
            .with_config()
            .limit(4096)
            .read_to_string()
            .unwrap_or_default();
        let message = error_message(&body);
        if matches!(status, 401 | 403) {
            Err(ConnectError::Auth { status, message })
        } else {
            Err(ConnectError::Status { status, message })
        }
    }
}

/// Prusa Connect errors are JSON with a readable `message`; anything else is shown as is.
fn error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("message")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| body.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_error_messages() {
        let forbidden =
            r#"{"statusCode":403,"message":"Camera access denied","errorCode":"FORBIDDEN"}"#;
        assert_eq!(error_message(forbidden), "Camera access denied");
        assert_eq!(error_message(" Bad Gateway\n"), "Bad Gateway");
    }
}
