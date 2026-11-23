//! Keeps the program's parts running, and notices when one has stopped.
//!
//! - An async part (live view in Prusa Connect, the RTSP server) runs on a runtime and thread of
//!   its own, so one that blocks its runtime holds up only itself, and a heartbeat on that
//!   runtime lets the watchdog notice.
//! - A task that panics is restarted with backoff, so a bug in one output doesn't end the others.
//! - Stop signals arrive here, so every way the program ends goes through one orderly shutdown.

use std::future::Future;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::signal::unix::{SignalKind, signal};

/// How often a runtime's heartbeat beats, and how long it may miss before it counts as stuck.
const BEAT: Duration = Duration::from_secs(5);
const STUCK: Duration = Duration::from_secs(60);
const MAX_RESTART_DELAY: Duration = Duration::from_secs(300);

/// An async part of the program, running on a runtime and thread of its own.
pub struct Service {
    name: String,
    thread: thread::JoinHandle<()>,
    heartbeat: Arc<Mutex<Instant>>,
}

/// Starts `future` on a runtime of its own, with `workers` threads, until it returns.
pub fn spawn_runtime<F>(name: &str, workers: usize, future: F) -> Result<Service>
where
    F: Future<Output = ()> + Send + 'static,
{
    let heartbeat = Arc::new(Mutex::new(Instant::now()));
    let beat = Arc::clone(&heartbeat);
    let thread_name = name.to_owned();
    let thread = thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(workers)
                .thread_name(format!("{thread_name}-rt"))
                .enable_all()
                .build()
                .expect("building a runtime");
            runtime.spawn(async move {
                loop {
                    *beat.lock().unwrap() = Instant::now();
                    tokio::time::sleep(BEAT).await;
                }
            });
            runtime.block_on(future);
        })
        .with_context(|| format!("starting {name}"))?;
    Ok(Service {
        name: name.to_owned(),
        thread,
        heartbeat,
    })
}

impl Service {
    /// Fails if the service has ended, or its runtime has stopped answering.
    pub fn check(&self) -> Result<()> {
        if self.thread.is_finished() {
            bail!("{} stopped", self.name);
        }
        if self.heartbeat.lock().unwrap().elapsed() > STUCK {
            bail!("{} is stuck", self.name);
        }
        Ok(())
    }

    /// Waits up to `timeout` for the service to end.
    pub fn wait(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !self.thread.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Runs the future `make` returns until it ends, starting a new one with backoff whenever it
/// panics. The panic itself is already in the log, from the panic hook.
pub async fn restart_on_panic<F, Fut>(label: String, make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    restart_after(label, make, Duration::from_secs(1)).await;
}

async fn restart_after<F, Fut>(label: String, make: F, first_delay: Duration)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut delay = first_delay;
    loop {
        let started = Instant::now();
        match tokio::spawn(make()).await {
            Err(e) if e.is_panic() => {
                if started.elapsed() > MAX_RESTART_DELAY {
                    delay = first_delay;
                }
                log::error!("[{label}] crashed; restarting it in {delay:?}");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(MAX_RESTART_DELAY);
            }
            _ => return,
        }
    }
}

/// Delivers SIGTERM and SIGINT, which ask the program to stop.
pub fn stop_signals() -> Result<std_mpsc::Receiver<()>> {
    let (stop, stops) = std_mpsc::channel();
    thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("building the signal runtime");
            runtime.block_on(async {
                let (Ok(mut terminate), Ok(mut interrupt)) = (
                    signal(SignalKind::terminate()),
                    signal(SignalKind::interrupt()),
                ) else {
                    log::warn!("can't handle stop signals; stopping won't be orderly");
                    return std::future::pending().await;
                };
                loop {
                    tokio::select! {
                        _ = terminate.recv() => {}
                        _ = interrupt.recv() => {}
                    }
                    if stop.send(()).is_err() {
                        return;
                    }
                }
            });
        })
        .context("starting the signal thread")?;
    Ok(stops)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn restarts_a_task_that_panics_until_it_returns() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&runs);
        let make = move || {
            let counter = Arc::clone(&counter);
            async move {
                if counter.fetch_add(1, Ordering::SeqCst) < 2 {
                    panic!("a bug in one output (this panic is part of the test)");
                }
            }
        };
        runtime.block_on(restart_after("test".into(), make, Duration::from_millis(1)));
        assert_eq!(runs.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn notices_a_service_that_ends() {
        let service = spawn_runtime("test-service", 1, async {}).unwrap();
        service.wait(Duration::from_secs(5));
        assert!(service.check().is_err());
        let running = spawn_runtime("test-service", 1, std::future::pending()).unwrap();
        assert!(running.check().is_ok());
    }
}
