//! The one place that decides whether the screen goes dark.
//!
//! Triggers never lock. They report evidence — "the anchor is gone", "the
//! headset is back" — and this task turns evidence into a decision. Keeping
//! that in one place is what makes the grace window, the cooldown and the
//! pause switch apply to every trigger without each one reimplementing them.
//!
//! The state machine is small on purpose:
//!
//!   idle  --Away-->  counting down  --deadline-->  locked (cooldown)
//!     ^                    |
//!     +-------Back---------+
//!
//! A `Back` cancels only a countdown started by the SAME source. Your headset
//! coming back says nothing about a USB stick that is still on the floor.
//!
//! Triggers report edges, not levels: the anchor says "gone" once. So an
//! absence that arrives while a countdown is already running, or during the
//! cooldown after a lock, is remembered rather than dropped, and gets its own
//! countdown as soon as nothing is in the way. Without that, pulling the anchor
//! during the headset's countdown and then having the headset reconnect, or
//! pulling it within a minute of the last lock, left the screen open for as
//! long as the stick stayed out. A lock answers every absence known at that
//! moment; a webhook is a one-off pulse and is never remembered.

use std::collections::HashMap;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tracing::{info, warn};

use crate::config::Config;
use crate::{lock, notch};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Anchor,
    Beacon,
    Webhook,
}

impl Source {
    /// A pulse has no matching `Back`, so it cannot be remembered as an
    /// ongoing absence.
    fn is_pulse(self) -> bool {
        matches!(self, Source::Webhook)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Source::Anchor => "anchor",
            Source::Beacon => "beacon",
            Source::Webhook => "webhook",
        }
    }
}

#[derive(Debug)]
pub enum Signal {
    /// This source believes you are gone.
    Away {
        source: Source,
        detail: String,
        /// Per-trigger override of the grace window.
        grace: Option<u64>,
    },
    /// This source has changed its mind.
    Back { source: Source, detail: String },
    /// Live one-line description of what a source currently sees, for `status`.
    Note { source: Source, text: String },
    /// Lock now: no countdown, no cooldown. Only ever user-initiated.
    LockNow,
    Pause(Duration),
    Resume,
    Status(oneshot::Sender<Status>),
}

#[derive(Debug, Serialize, serde::Deserialize)]
pub struct Status {
    pub armed: bool,
    pub dry_run: bool,
    pub paused_secs: Option<u64>,
    pub counting_down: Option<Countdown>,
    pub secs_since_lock: Option<u64>,
    pub triggers: HashMap<String, String>,
}

#[derive(Debug, Serialize, serde::Deserialize)]
pub struct Countdown {
    pub source: String,
    pub detail: String,
    pub secs_left: u64,
}

struct Pending {
    source: Source,
    detail: String,
    deadline: Instant,
}

/// An absence that has not been answered by a lock yet.
struct Absent {
    detail: String,
    grace: Option<u64>,
}

pub async fn run(cfg: Config, dry: bool, mut rx: mpsc::Receiver<Signal>) {
    let mut pending: Option<Pending> = None;
    let mut paused_until: Option<Instant> = None;
    let mut last_lock: Option<Instant> = None;
    let mut notes: HashMap<Source, String> = HashMap::new();
    let mut absent: HashMap<Source, Absent> = HashMap::new();
    let cooldown = Duration::from_secs(cfg.general.cooldown_secs);

    loop {
        // Wake for the countdown, or — when an absence is waiting out the
        // cooldown — for the end of the cooldown.
        let tick = match &pending {
            Some(p) => Some(p.deadline),
            None if !absent.is_empty() => last_lock.map(|t| t + cooldown),
            None => None,
        };
        let sig = tokio::select! {
            msg = rx.recv() => match msg {
                Some(m) => Some(m),
                None => return,
            },
            _ = async {
                match tick {
                    Some(t) => tokio::time::sleep_until(t).await,
                    // Nothing pending: park this branch forever and let the
                    // channel drive.
                    None => std::future::pending().await,
                }
            } => None,
        };

        let Some(sig) = sig else {
            match pending.take() {
                // The countdown ran out.
                Some(p) => {
                    info!(source = p.source.as_str(), detail = %p.detail, "locking: countdown elapsed");
                    lock::lock(&cfg.general.lock_command, dry).await;
                    last_lock = Some(Instant::now());
                    absent.clear();
                }
                // The cooldown ran out with an absence still unanswered.
                None => {
                    let cooling = last_lock_blocks(last_lock, cooldown);
                    start_next(&cfg, dry, &mut absent, &mut pending, &mut last_lock, cooling).await;
                }
            }
            continue;
        };

        match sig {
            Signal::Note { source, text } => {
                notes.insert(source, text);
            }

            Signal::Away { source, detail, grace } => {
                notes.insert(source, detail.clone());

                if !cfg.general.armed {
                    info!(source = source.as_str(), %detail, "away, but disarmed");
                    continue;
                }
                if let Some(until) = paused_until {
                    if Instant::now() < until {
                        info!(source = source.as_str(), %detail, "away, but paused");
                        continue;
                    }
                    paused_until = None;
                }
                // Remember it before anything below can set it aside: the
                // trigger will not say it again.
                if !source.is_pulse() {
                    absent.insert(source, Absent { detail: detail.clone(), grace });
                }
                if let Some(t) = last_lock {
                    if t.elapsed() < cooldown {
                        // Almost always a flapping link right after a lock,
                        // while the password is being typed into the screen it
                        // would lock again.
                        info!(source = source.as_str(), %detail, "away, but within cooldown");
                        continue;
                    }
                }
                if pending.is_some() {
                    // Already counting down. The first source to notice owns
                    // the window; a second one must not shorten or restart it.
                    // It stays in `absent` and takes over if the owner's
                    // countdown is cancelled.
                    continue;
                }

                begin(&cfg, dry, source, detail, grace, &mut absent, &mut pending, &mut last_lock).await;
            }

            Signal::Back { source, detail } => {
                notes.insert(source, detail.clone());
                absent.remove(&source);
                if pending.as_ref().is_some_and(|p| p.source == source) {
                    pending = None;
                    info!(source = source.as_str(), %detail, "back: countdown cancelled");
                    if cfg.general.notch {
                        notch::cancel().await;
                    }
                    // Another source may still say you are gone.
                    let paused = paused_until.is_some_and(|t| Instant::now() < t);
                    if !paused {
                        let cooling = last_lock_blocks(last_lock, cooldown);
                        start_next(&cfg, dry, &mut absent, &mut pending, &mut last_lock, cooling).await;
                    }
                }
            }

            Signal::LockNow => {
                if pending.take().is_some() && cfg.general.notch {
                    notch::cancel().await;
                }
                info!("locking: asked to");
                lock::lock(&cfg.general.lock_command, dry).await;
                last_lock = Some(Instant::now());
                absent.clear();
            }

            Signal::Pause(d) => {
                // A pause means "ignore what you have seen so far", too.
                absent.clear();
                // checked: `vanish pause` with an absurd duration must not
                // panic the daemon.
                let now = Instant::now();
                paused_until = Some(now.checked_add(d).unwrap_or(now + Duration::from_secs(365 * 24 * 3600)));
                if pending.take().is_some() {
                    if cfg.general.notch {
                        notch::cancel().await;
                    }
                    info!("countdown cancelled by pause");
                }
                info!(secs = d.as_secs(), "paused");
            }

            Signal::Resume => {
                paused_until = None;
                info!("resumed");
            }

            Signal::Status(reply) => {
                let now = Instant::now();
                let st = Status {
                    armed: cfg.general.armed,
                    dry_run: dry,
                    paused_secs: paused_until
                        .filter(|t| *t > now)
                        .map(|t| (t - now).as_secs()),
                    counting_down: pending.as_ref().map(|p| Countdown {
                        source: p.source.as_str().to_string(),
                        detail: p.detail.clone(),
                        secs_left: p.deadline.saturating_duration_since(now).as_secs(),
                    }),
                    secs_since_lock: last_lock.map(|t| t.elapsed().as_secs()),
                    triggers: notes
                        .iter()
                        .map(|(k, v)| (k.as_str().to_string(), v.clone()))
                        .collect(),
                };
                if reply.send(st).is_err() {
                    warn!("status requester went away");
                }
            }
        }
    }
}

fn last_lock_blocks(last_lock: Option<Instant>, cooldown: Duration) -> bool {
    last_lock.is_some_and(|t| t.elapsed() < cooldown)
}

/// Start a countdown for the remembered absence with the shortest grace, if
/// nothing is in the way.
async fn start_next(
    cfg: &Config,
    dry: bool,
    absent: &mut HashMap<Source, Absent>,
    pending: &mut Option<Pending>,
    last_lock: &mut Option<Instant>,
    in_cooldown: bool,
) {
    if pending.is_some() || in_cooldown || !cfg.general.armed {
        return;
    }
    let Some((&source, a)) = absent
        .iter()
        .min_by_key(|(_, a)| a.grace.unwrap_or(cfg.general.grace_secs))
    else {
        return;
    };
    let (detail, grace) = (a.detail.clone(), a.grace);
    begin(cfg, dry, source, detail, grace, absent, pending, last_lock).await;
}

/// Lock now if the grace is zero, otherwise start the countdown.
#[allow(clippy::too_many_arguments)]
async fn begin(
    cfg: &Config,
    dry: bool,
    source: Source,
    detail: String,
    grace: Option<u64>,
    absent: &mut HashMap<Source, Absent>,
    pending: &mut Option<Pending>,
    last_lock: &mut Option<Instant>,
) {
    let secs = grace.unwrap_or(cfg.general.grace_secs);
    if secs == 0 {
        info!(source = source.as_str(), %detail, "locking: no grace configured");
        lock::lock(&cfg.general.lock_command, dry).await;
        *last_lock = Some(Instant::now());
        absent.clear();
        return;
    }

    info!(source = source.as_str(), %detail, secs, "away: counting down");
    if cfg.general.notch {
        notch::lock_soon(secs).await;
    }
    *pending = Some(Pending {
        source,
        detail,
        deadline: Instant::now() + Duration::from_secs(secs),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // Dry run with a configured command: a lock only logs, so the tests count
    // locks through `secs_since_lock` and never spawn anything.
    fn spawn() -> mpsc::Sender<Signal> {
        let mut cfg = Config::default();
        cfg.general.grace_secs = 20;
        cfg.general.cooldown_secs = 60;
        cfg.general.lock_command = "true".into();
        cfg.general.notch = false;
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(run(cfg, true, rx));
        tx
    }

    async fn away(tx: &mpsc::Sender<Signal>, source: Source, grace: Option<u64>) {
        tx.send(Signal::Away { source, detail: "gone".into(), grace }).await.unwrap();
    }

    async fn back(tx: &mpsc::Sender<Signal>, source: Source) {
        tx.send(Signal::Back { source, detail: "back".into() }).await.unwrap();
    }

    async fn status(tx: &mpsc::Sender<Signal>) -> Status {
        let (rtx, rrx) = oneshot::channel();
        tx.send(Signal::Status(rtx)).await.unwrap();
        rrx.await.unwrap()
    }

    async fn wait(secs: u64) {
        tokio::time::sleep(Duration::from_secs(secs)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn countdown_locks() {
        let tx = spawn();
        away(&tx, Source::Anchor, Some(3)).await;
        assert!(status(&tx).await.secs_since_lock.is_none());
        wait(4).await;
        assert!(status(&tx).await.secs_since_lock.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn anchor_pulled_during_another_countdown_still_locks() {
        let tx = spawn();
        away(&tx, Source::Beacon, None).await;
        wait(2).await;
        away(&tx, Source::Anchor, Some(3)).await;
        // The headset comes back; the stick is still on the floor.
        back(&tx, Source::Beacon).await;
        let st = status(&tx).await;
        assert_eq!(st.counting_down.map(|c| c.source).as_deref(), Some("anchor"));
        wait(4).await;
        assert!(status(&tx).await.secs_since_lock.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn anchor_pulled_during_cooldown_locks_when_it_ends() {
        let tx = spawn();
        tx.send(Signal::LockNow).await.unwrap();
        wait(10).await;
        away(&tx, Source::Anchor, Some(3)).await;
        assert!(status(&tx).await.counting_down.is_none());
        wait(52).await; // cooldown over at 60s, countdown until 63s
        assert_eq!(status(&tx).await.counting_down.map(|c| c.source).as_deref(), Some("anchor"));
        wait(3).await;
        assert!(status(&tx).await.secs_since_lock.unwrap() < 5);
    }

    #[tokio::test(start_paused = true)]
    async fn back_during_cooldown_forgets_the_absence() {
        let tx = spawn();
        tx.send(Signal::LockNow).await.unwrap();
        away(&tx, Source::Anchor, Some(3)).await;
        back(&tx, Source::Anchor).await;
        wait(120).await;
        let st = status(&tx).await;
        assert!(st.counting_down.is_none());
        assert!(st.secs_since_lock.unwrap() >= 120);
    }

    #[tokio::test(start_paused = true)]
    async fn one_absence_locks_once_and_pulses_are_not_remembered() {
        let tx = spawn();
        away(&tx, Source::Anchor, Some(3)).await;
        wait(10).await;
        // A webhook pulse inside the cooldown is dropped, not queued.
        away(&tx, Source::Webhook, Some(0)).await;
        wait(120).await;
        let st = status(&tx).await;
        assert!(st.counting_down.is_none());
        assert!(st.secs_since_lock.unwrap() >= 120);
    }

    #[tokio::test(start_paused = true)]
    async fn absurd_pause_does_not_panic() {
        let tx = spawn();
        tx.send(Signal::Pause(Duration::from_secs(u64::MAX))).await.unwrap();
        away(&tx, Source::Anchor, Some(0)).await;
        let st = status(&tx).await;
        assert!(st.paused_secs.is_some());
        assert!(st.secs_since_lock.is_none());
    }
}
