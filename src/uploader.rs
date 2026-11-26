//! The loop that sends one camera's stills to one output: pacing, backoff, logging and status.
//! It knows nothing about any server. A [`StillTarget`] does the sending, and reports what went
//! wrong as a [`SendError`], so a server's status codes stay inside its own module.

use std::fmt;
use std::net::{TcpStream, ToSocketAddrs};
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::source::{CAPTURE_TIMEOUT, FrameSource, Paused};
use crate::status::{State, Status};

/// The deploy scripts read these from the log, so they're spelled once, here.
pub const UPLOADING: &str = "uploading every";
pub const PAUSING: &str = "pausing uploads";
pub const REJECTED_CREDENTIALS: &str = "rejected the camera's credentials";

/// The longest wait between attempts, and the wait after refused credentials.
pub const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// The longest a target's `send` may take: targets give their requests this timeout, and the
/// watchdog allows for it.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const PRINTER_TIMEOUT: Duration = Duration::from_secs(2);

/// Somewhere a camera's stills are sent.
pub trait StillTarget: Send {
    /// Names the output in logs and the status file, after the camera's id: `cam0/<name>`.
    fn name(&self) -> &str;
    fn interval(&self) -> Duration;
    /// A `host:port` that must accept a connection before each upload, for a server that
    /// discards pictures while the printer is offline. Uploads pause while it doesn't.
    fn printer_check(&self) -> Option<&str> {
        None
    }
    /// Called once before the first upload, e.g. to log which server this is.
    fn start(&self, _label: &str) {}
    /// Sends one JPEG and returns the HTTP status of the answer.
    fn send(&self, jpeg: &[u8]) -> Result<u16, SendError>;
    /// Called when uploads start succeeding, at startup or after failures.
    fn on_uploading(&self, _label: &str) {}
}

/// Why a still wasn't accepted, in the terms the loop acts on.
#[derive(Debug)]
pub enum SendError {
    /// The server refused the credentials. Retrying soon won't help, so the loop waits longest.
    /// The message says so with [`REJECTED_CREDENTIALS`], which the deploy scripts look for.
    Auth(String),
    /// Worth trying again with backoff: the network, the server, or a setting fixed meanwhile.
    Retry(String),
    /// The server refused this picture. The next one goes on schedule.
    Rejected(String),
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendError::Auth(message) | SendError::Retry(message) => f.write_str(message),
            SendError::Rejected(message) => write!(f, "the server refused a picture ({message})"),
        }
    }
}

impl std::error::Error for SendError {}

/// Doubles the wait after each consecutive failure, starting from the normal interval.
pub fn backoff(interval: Duration, failures: u32) -> Duration {
    interval
        .saturating_mul(2u32.saturating_pow(failures.saturating_sub(1)))
        .min(MAX_BACKOFF)
}

pub struct Uploader {
    /// `cam0/local`: prefixes the output's log lines and keys its status.
    pub label: String,
    target: Box<dyn StillTarget>,
    source: Arc<dyn FrameSource>,
    status: Status,
}

#[derive(Default)]
struct Progress {
    failures: u32,
    uploading: bool,
    printer_down: bool,
}

fn reachable(addr: &str) -> bool {
    let Ok(addrs) = addr.to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|a| TcpStream::connect_timeout(&a, PRINTER_TIMEOUT).is_ok())
}

impl Uploader {
    pub fn new(
        camera_id: &str,
        target: Box<dyn StillTarget>,
        source: Arc<dyn FrameSource>,
        status: Status,
    ) -> Self {
        let label = format!("{camera_id}/{}", target.name());
        status.register(&label);
        Self {
            label,
            target,
            source,
            status,
        }
    }

    /// Uploads until the program ends. A panic in one round is logged and retried later, so a
    /// bug in one output doesn't take the others down.
    pub fn run(&self, heartbeat: &Mutex<Instant>) {
        self.target.start(&self.label);
        let mut progress = Progress::default();
        loop {
            *heartbeat.lock().unwrap() = Instant::now();
            let mut started = Instant::now();
            let delay =
                panic::catch_unwind(AssertUnwindSafe(|| self.round(&mut progress, &mut started)))
                    .unwrap_or_else(|_| {
                        log::error!(
                            "[{}] the upload crashed; trying again in {}s",
                            self.label,
                            MAX_BACKOFF.as_secs()
                        );
                        self.status
                            .set(&self.label, State::Retrying, "the upload crashed");
                        MAX_BACKOFF
                    });
            thread::sleep(delay.saturating_sub(started.elapsed()));
        }
    }

    /// Captures and sends one still, and returns how long to wait before the next. `started` is
    /// set to when the upload began, so pacing doesn't count a capture that waited for the camera.
    fn round(&self, progress: &mut Progress, started: &mut Instant) -> Duration {
        let label = &self.label;
        let interval = self.target.interval();
        if let Some(addr) = self.target.printer_check() {
            let down = !reachable(addr);
            if down != progress.printer_down {
                if down {
                    log::info!("[{label}] printer at {addr} is unreachable; {PAUSING}");
                } else {
                    log::info!("[{label}] printer at {addr} is back; resuming uploads");
                }
                progress.printer_down = down;
            }
            if down {
                let message = format!("the printer is unreachable; {PAUSING}");
                self.status.set(label, State::Paused, message);
                return interval;
            }
        }
        let jpeg = match self.source.capture(CAPTURE_TIMEOUT) {
            Ok(jpeg) => jpeg,
            Err(e) if e.is::<Paused>() => {
                log::debug!("[{label}] {e}");
                self.status.set(label, State::Paused, e.to_string());
                return interval;
            }
            Err(e) => return self.failed(progress, interval, &format!("{e:#}")),
        };
        *started = Instant::now();
        match self.target.send(&jpeg) {
            Ok(status) => {
                if !progress.uploading {
                    log::info!(
                        "[{label}] {UPLOADING} {}s (HTTP {status})",
                        interval.as_secs()
                    );
                    self.target.on_uploading(label);
                }
                log::debug!(
                    "[{label}] uploaded {} bytes in {} ms (HTTP {status})",
                    jpeg.len(),
                    started.elapsed().as_millis()
                );
                progress.uploading = true;
                progress.failures = 0;
                let message = format!("{UPLOADING} {}s", interval.as_secs());
                self.status.set(label, State::Ok, message);
                interval
            }
            Err(e @ SendError::Auth(_)) => {
                progress.uploading = false;
                progress.failures += 1;
                log::warn!("[{label}] {e}; retrying in {}s", MAX_BACKOFF.as_secs());
                self.status.set(label, State::Auth, e.to_string());
                MAX_BACKOFF
            }
            Err(e @ SendError::Rejected(_)) => {
                log::warn!("[{label}] {e}");
                self.status.set(label, State::Retrying, e.to_string());
                interval
            }
            Err(SendError::Retry(message)) => self.failed(progress, interval, &message),
        }
    }

    fn failed(&self, progress: &mut Progress, interval: Duration, message: &str) -> Duration {
        progress.uploading = false;
        progress.failures += 1;
        let delay = backoff(interval, progress.failures);
        log::warn!(
            "[{}] {message}; retrying in {}s",
            self.label,
            delay.as_secs()
        );
        self.status.set(&self.label, State::Retrying, message);
        delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn backoff_doubles_up_to_the_limit() {
        let interval = Duration::from_secs(10);
        let delays: Vec<u64> = (1..=7).map(|n| backoff(interval, n).as_secs()).collect();
        assert_eq!(delays, [10, 20, 40, 80, 160, 300, 300]);
        assert_eq!(backoff(interval, u32::MAX), MAX_BACKOFF);
    }

    struct Fixed(Arc<[u8]>);

    impl FrameSource for Fixed {
        fn capture(&self, _timeout: Duration) -> anyhow::Result<Arc<[u8]>> {
            Ok(Arc::clone(&self.0))
        }
        fn latest(&self) -> Option<crate::source::Still> {
            None
        }
        fn newer(&self, _seq: u64, _timeout: Duration) -> anyhow::Result<crate::source::Still> {
            anyhow::bail!("not used")
        }
    }

    /// Answers each send with the next result from a script, and counts the hooks called.
    #[derive(Default)]
    struct Scripted {
        answers: Mutex<Vec<Result<u16, SendError>>>,
        sent: Arc<AtomicUsize>,
        uploading: Arc<AtomicUsize>,
        printer: Option<String>,
    }

    impl StillTarget for Scripted {
        fn name(&self) -> &str {
            "test"
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(10)
        }
        fn printer_check(&self) -> Option<&str> {
            self.printer.as_deref()
        }
        fn send(&self, _jpeg: &[u8]) -> Result<u16, SendError> {
            self.sent.fetch_add(1, Ordering::SeqCst);
            self.answers.lock().unwrap().remove(0)
        }
        fn on_uploading(&self, _label: &str) {
            self.uploading.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn uploader(target: Scripted, status: &Status) -> Uploader {
        let frames = Arc::new(Fixed(Arc::from(&b"\xff\xd8jpeg"[..])));
        Uploader::new("cam0", Box::new(target), frames, status.clone())
    }

    #[test]
    fn pauses_while_the_printer_is_unreachable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let up = listener.local_addr().unwrap().to_string();
        drop(listener);
        let status = Status::new(None);
        // Nothing listens on the port any more, so the printer counts as off.
        let target = Scripted {
            printer: Some(up),
            ..Scripted::default()
        };
        let sent = Arc::clone(&target.sent);
        let uploader = uploader(target, &status);
        let delay = uploader.round(&mut Progress::default(), &mut Instant::now());
        assert_eq!(delay, Duration::from_secs(10));
        assert_eq!(sent.load(Ordering::SeqCst), 0);
        assert_eq!(status.state("cam0/test"), Some(State::Paused));
    }

    #[test]
    fn paces_backs_off_and_reports() {
        let answers = vec![
            Ok(204),
            Err(SendError::Retry("server down".into())),
            Err(SendError::Retry("server down".into())),
            Err(SendError::Auth("(HTTP 401)".into())),
            Err(SendError::Rejected("HTTP 415".into())),
            Ok(204),
        ];
        let target = Scripted {
            answers: Mutex::new(answers),
            ..Scripted::default()
        };
        let uploading = Arc::clone(&target.uploading);
        let status = Status::new(None);
        let uploader = uploader(target, &status);
        assert_eq!(uploader.label, "cam0/test");
        assert_eq!(status.state("cam0/test"), Some(State::Starting));
        let mut progress = Progress::default();
        let mut round = || uploader.round(&mut progress, &mut Instant::now()).as_secs();
        let delays: Vec<u64> = (0..6).map(|_| round()).collect();
        assert_eq!(delays, [10, 10, 20, 300, 10, 10]);
        assert_eq!(status.state("cam0/test"), Some(State::Ok));
        // Uploads started at the first success, and again once the failures were over.
        assert_eq!(uploading.load(Ordering::SeqCst), 2);
    }
}
