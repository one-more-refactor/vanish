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

pub async fn run(cfg: Config, dry: bool, mut rx: mpsc::Receiver<Signal>) {
    let mut pending: Option<Pending> = None;
    let mut paused_until: Option<Instant> = None;
    let mut last_lock: Option<Instant> = None;
    let mut notes: HashMap<Source, String> = HashMap::new();
    let cooldown = Duration::from_secs(cfg.general.cooldown_secs);

    loop {
        let tick = pending.as_ref().map(|p| p.deadline);
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
            // The countdown ran out.
            let p = pending.take().expect("deadline without a pending countdown");
            info!(source = p.source.as_str(), detail = %p.detail, "locking: countdown elapsed");
            lock::lock(&cfg.general.lock_command, dry).await;
            last_lock = Some(Instant::now());
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
                    continue;
                }

                let secs = grace.unwrap_or(cfg.general.grace_secs);
                if secs == 0 {
                    info!(source = source.as_str(), %detail, "locking: no grace configured");
                    lock::lock(&cfg.general.lock_command, dry).await;
                    last_lock = Some(Instant::now());
                    continue;
                }

                info!(source = source.as_str(), %detail, secs, "away: counting down");
                if cfg.general.notch {
                    notch::lock_soon(secs).await;
                }
                pending = Some(Pending {
                    source,
                    detail,
                    deadline: Instant::now() + Duration::from_secs(secs),
                });
            }

            Signal::Back { source, detail } => {
                notes.insert(source, detail.clone());
                if pending.as_ref().is_some_and(|p| p.source == source) {
                    pending = None;
                    info!(source = source.as_str(), %detail, "back: countdown cancelled");
                    if cfg.general.notch {
                        notch::cancel().await;
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
            }

            Signal::Pause(d) => {
                paused_until = Some(Instant::now() + d);
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
