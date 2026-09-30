//! `serve`'s log: a file a freeze can be read out of afterwards.
//!
//! Three things, each for the question a hang asks:
//!
//! - **every request, twice** — [`requests`] writes a line when one starts and
//!   one when it ends, so a request that hung is a start with no end;
//! - **what is still waiting** — [`Watchdog`] names every request in flight for
//!   more than five seconds, again every five seconds, from a thread of its own;
//! - **whether the runtime still turns** — the same thread watches a tick a
//!   tokio task bumps every second, and says so when it stops: a lock and a
//!   blocked runtime are different answers.
//!
//! The file is `<state dir>/logs/serve.YYYY-MM-DD.log`, rotated daily, seven
//! kept. **The writer never blocks**: lines go to `tracing-appender`'s own
//! thread through a bounded channel and are dropped rather than waited for,
//! because a log added to find a freeze must not be able to cause one. See
//! `RECORD/2026-09-30.a-log-for-serve.completed.md`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;

/// What `LUU_LOG` is when it is not set: luu's own crates at `info`, every
/// dependency at `warn`.
const DEFAULT_FILTER: &str = "warn,luu=info,agent_core=info";

/// How long a request may take before the watchdog names it, and how often it
/// looks.
const SLOW: Duration = Duration::from_secs(5);

/// The log, open. Dropping it flushes what the writer thread still holds, so
/// it lives as long as `serve` does.
pub struct Log {
    _guard: WorkerGuard,
    pub dir: PathBuf,
}

/// Opens the log under `state_dir`. `LUU_LOG` takes a `tracing` filter —
/// `LUU_LOG=debug`, `LUU_LOG=luu=debug,hyper=info` — and one that does not parse
/// is said once and replaced by the default rather than refused: a typo in a
/// log filter is not a reason not to serve.
pub fn open(state_dir: &Path) -> anyhow::Result<Log> {
    let dir = state_dir.join("logs");
    std::fs::create_dir_all(&dir)?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("serve")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)?;
    let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .lossy(true)
        .finish(appender);
    let filter = match std::env::var("LUU_LOG") {
        Ok(asked) => EnvFilter::try_new(&asked).unwrap_or_else(|error| {
            eprintln!(
                "warning: LUU_LOG={asked} is not a filter ({error}); logging {DEFAULT_FILTER}"
            );
            EnvFilter::new(DEFAULT_FILTER)
        }),
        Err(_) => EnvFilter::new(DEFAULT_FILTER),
    };
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(filter)
        .try_init()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(Log { _guard: guard, dir })
}

/// The requests the server is answering right now, by id — what the watchdog
/// reads, and what [`requests`] fills and empties.
#[derive(Default)]
pub struct InFlight {
    next: AtomicU64,
    requests: std::sync::Mutex<BTreeMap<u64, (String, String, Instant)>>,
    /// Bumped by a tokio task every second; see [`Watchdog`].
    tick: AtomicU64,
}

impl InFlight {
    fn table(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, (String, String, Instant)>> {
        self.requests.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Taken out of the table when the request ends — and when it does not end but
/// is dropped, a client that went away, which is its own line.
struct Tracked {
    inflight: Arc<InFlight>,
    id: u64,
    finished: bool,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        let entry = self.inflight.table().remove(&self.id);
        if !self.finished
            && let Some((method, path, started)) = entry
        {
            tracing::info!(
                target: "luu::http",
                id = self.id,
                %method,
                path,
                ms = started.elapsed().as_millis() as u64,
                "dropped before it answered",
            );
        }
    }
}

/// The middleware: a line when a request starts and one when it ends. The path
/// only, never the query — `?token=` travels there — and never a body, which
/// carries prompts. The embedded page's own files are `debug`, because a page
/// load is thirty of them and none is where a server hangs.
pub async fn requests(
    State(inflight): State<Arc<InFlight>>,
    request: Request,
    next: Next,
) -> Response {
    let id = inflight.next.fetch_add(1, Ordering::Relaxed) + 1;
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let api = path.starts_with("/api/") || path.starts_with("/ws");
    let started = Instant::now();
    if api {
        tracing::info!(target: "luu::http", id, %method, path, "start");
    } else {
        tracing::debug!(target: "luu::http", id, %method, path, "start");
    }
    inflight
        .table()
        .insert(id, (method.clone(), path.clone(), started));
    let mut tracked = Tracked {
        inflight: inflight.clone(),
        id,
        finished: false,
    };
    let response = next.run(request).await;
    tracked.finished = true;
    let status = response.status().as_u16();
    let ms = started.elapsed().as_millis() as u64;
    if api || status >= 400 {
        tracing::info!(target: "luu::http", id, %method, path, status, ms, "end");
    } else {
        tracing::debug!(target: "luu::http", id, %method, path, status, ms, "end");
    }
    response
}

/// Every request in flight for [`SLOW`] or longer, as `(id, method, path,
/// seconds)`. The sockets are not: an open socket is supposed to stay open.
fn overdue(
    table: &BTreeMap<u64, (String, String, Instant)>,
    now: Instant,
) -> Vec<(u64, String, String, u64)> {
    table
        .iter()
        .filter(|(_, (_, path, started))| {
            !path.starts_with("/ws") && now.saturating_duration_since(*started) >= SLOW
        })
        .map(|(id, (method, path, started))| {
            (
                *id,
                method.clone(),
                path.clone(),
                now.saturating_duration_since(*started).as_secs(),
            )
        })
        .collect()
}

/// What the watchdog says about the runtime's tick.
#[derive(Debug, PartialEq, Eq)]
enum Turning {
    /// Not moved since the last look, for this many seconds.
    Stopped(u64),
    /// Moving again, after this many seconds still.
    Again(u64),
}

/// The tick's last value and, while it is not moving, since when.
#[derive(Default)]
struct Stall {
    last: Option<u64>,
    since: Option<Instant>,
}

impl Stall {
    /// One look, `SLOW` after the previous one. Says something only when the
    /// tick has not moved (every look while it stays still) and once when it
    /// moves again.
    fn observe(&mut self, tick: u64, now: Instant) -> Option<Turning> {
        let still = self.last == Some(tick);
        self.last = Some(tick);
        match (still, self.since) {
            (true, None) => {
                let since = now.checked_sub(SLOW).unwrap_or(now);
                self.since = Some(since);
                Some(Turning::Stopped(
                    now.saturating_duration_since(since).as_secs(),
                ))
            }
            (true, Some(since)) => Some(Turning::Stopped(
                now.saturating_duration_since(since).as_secs(),
            )),
            (false, Some(since)) => {
                self.since = None;
                Some(Turning::Again(
                    now.saturating_duration_since(since).as_secs(),
                ))
            }
            (false, None) => None,
        }
    }
}

/// Looks at [`InFlight`] every five seconds from an OS thread of its own —
/// not a tokio task, because what it exists to report includes the runtime
/// itself not turning, and a task on a stuck runtime reports nothing.
pub struct Watchdog;

impl Watchdog {
    pub fn start(inflight: Arc<InFlight>) {
        // The tick: a task that cannot run when the runtime cannot.
        let ticking = inflight.clone();
        tokio::spawn(async move {
            loop {
                ticking.tick.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        std::thread::Builder::new()
            .name("luu-watchdog".into())
            .spawn(move || {
                let mut stall = Stall::default();
                loop {
                    std::thread::sleep(SLOW);
                    match stall.observe(inflight.tick.load(Ordering::Relaxed), Instant::now()) {
                        Some(Turning::Stopped(secs)) => tracing::warn!(
                            target: "luu::watchdog",
                            secs,
                            "the async runtime has not turned: every task is waiting on something that is not a timer",
                        ),
                        Some(Turning::Again(secs)) => tracing::warn!(
                            target: "luu::watchdog",
                            secs,
                            "the async runtime is turning again",
                        ),
                        None => {}
                    }
                    for (id, method, path, secs) in overdue(&inflight.table(), Instant::now()) {
                        tracing::warn!(target: "luu::watchdog", id, %method, path, secs, "still in flight");
                    }
                }
            })
            .expect("spawning the watchdog thread");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_named_once_it_has_waited_five_seconds_and_a_socket_never_is() {
        let now = Instant::now();
        let ago = |secs| now.checked_sub(Duration::from_secs(secs)).unwrap();
        let mut table = BTreeMap::new();
        table.insert(1, ("GET".into(), "/api/settings".into(), ago(12)));
        table.insert(2, ("GET".into(), "/api/engines".into(), ago(1)));
        table.insert(3, ("GET".into(), "/ws".into(), ago(600)));
        assert_eq!(
            overdue(&table, now),
            vec![(1, "GET".to_string(), "/api/settings".to_string(), 12)]
        );
    }

    #[test]
    fn a_tick_that_stops_is_said_every_look_and_its_return_once() {
        let start = Instant::now();
        let at = |looks: u64| start + SLOW * looks as u32;
        let mut stall = Stall::default();
        assert_eq!(
            stall.observe(1, at(1)),
            None,
            "the first look has nothing to compare"
        );
        assert_eq!(stall.observe(6, at(2)), None, "moving");
        assert_eq!(stall.observe(6, at(3)), Some(Turning::Stopped(5)));
        assert_eq!(stall.observe(6, at(4)), Some(Turning::Stopped(10)));
        assert_eq!(stall.observe(7, at(5)), Some(Turning::Again(15)));
        assert_eq!(stall.observe(12, at(6)), None);
    }
}
