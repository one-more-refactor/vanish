//! The one place that decides whether the screen goes dark.
//!
//! Triggers never lock. They report evidence — "the anchor is gone", "the
//! headset is back" — and this module turns evidence into a decision. Keeping
//! that in one place is what makes the grace window, the cooldown and the
//! pause switch apply to every trigger without each one reimplementing them.
//!
//! The decision itself lives in [`Core`], a plain struct with no I/O and no
//! clock of its own: every method takes `now` and returns the [`Action`]s to
//! perform. That is what lets the tests at the bottom of this file walk through
//! "anchor pulled during the cooldown" or "headset back while the anchor is
//! still on the floor" without a USB stick, a headset or a real clock.
//!
//! ## The model
//!
//! Each source is either *away* or not. Anchor and beacon are **states**: they
//! report `Away` when you leave and `Back` when you return, and the arbiter
//! remembers which ones are currently away (an *episode*). The webhook is a
//! **pulse**: it only ever says "lock", never "back".
//!
//! ```text
//!   idle  --Away-->  counting down  --deadline-->  locked (cooldown)
//!     ^                    |
//!     +-------Back---------+
//! ```
//!
//! Rules, each of which has a test:
//!
//! * The first source to notice owns the countdown. A second source must not
//!   shorten or restart it.
//! * `Back` cancels only a countdown started by the SAME source. Your headset
//!   coming back says nothing about a USB stick that is still on the floor —
//!   and if that stick is still out, its own countdown starts next.
//! * Every countdown starts fresh with its owner's grace window.
//! * One away episode causes at most one lock. After the lock, an anchor that
//!   is still out does not lock you again while you type your password.
//! * Evidence that arrives while vanish cannot act on it — during the cooldown
//!   or a pause — is not thrown away. If the anchor is still out when the
//!   cooldown or the pause ends, the countdown starts then. (A pulse is the
//!   exception: a webhook that fires during a cooldown or pause is dropped,
//!   because it cannot say whether it still holds.)
//! * Disarmed never locks on its own. `vanish lock` always does: it is a
//!   person asking.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tracing::{info, warn};

use crate::config::Config;
use crate::{lock, notch};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
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

    /// A pulse says "lock" and never says "back". Its evidence is used once.
    fn is_pulse(self) -> bool {
        matches!(self, Source::Webhook)
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

/// What the caller of [`Core`] has to go and do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Lock the session. `source` is `None` when a person asked for it.
    Lock { source: Option<Source>, detail: String },
    /// A countdown of this many seconds just started (notchd shows it).
    CountdownStarted(u64),
    /// The countdown on screen is no longer happening.
    CountdownCancelled,
}

#[derive(Debug, Clone)]
struct Pending {
    source: Source,
    detail: String,
    deadline: Instant,
}

#[derive(Debug, Clone)]
struct Episode {
    detail: String,
    grace: Option<u64>,
    since: Instant,
    /// A lock already happened during this episode.
    consumed: bool,
}

/// Why an away signal cannot be acted on right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Disarmed,
    Paused,
    Cooldown,
}

/// The decision, without the I/O.
#[derive(Debug)]
pub struct Core {
    armed: bool,
    grace_secs: u64,
    cooldown: Duration,
    away: BTreeMap<Source, Episode>,
    pending: Option<Pending>,
    paused_until: Option<Instant>,
    last_lock: Option<Instant>,
    notes: HashMap<Source, String>,
}

impl Core {
    pub fn new(armed: bool, grace_secs: u64, cooldown_secs: u64) -> Core {
        Core {
            armed,
            grace_secs,
            cooldown: Duration::from_secs(cooldown_secs),
            away: BTreeMap::new(),
            pending: None,
            paused_until: None,
            last_lock: None,
            notes: HashMap::new(),
        }
    }

    pub fn from_config(cfg: &Config) -> Core {
        Core::new(cfg.general.armed, cfg.general.grace_secs, cfg.general.cooldown_secs)
    }

    pub fn note(&mut self, source: Source, text: String) {
        self.notes.insert(source, text);
    }

    pub fn away(&mut self, now: Instant, source: Source, detail: String, grace: Option<u64>) -> Vec<Action> {
        self.notes.insert(source, detail.clone());

        if source.is_pulse() {
            if let Some(b) = self.blocked(now) {
                info!(source = source.as_str(), %detail, reason = ?b, "away, but not acting on it");
                return Vec::new();
            }
            // Each pulse is its own episode.
            self.away.insert(source, Episode { detail, grace, since: now, consumed: false });
        } else {
            match self.away.get_mut(&source) {
                // Still the same absence: keep when it started and whether it
                // already caused a lock. Re-reporting must not re-arm it.
                Some(ep) => ep.detail = detail,
                None => {
                    if let Some(b) = self.blocked(now) {
                        info!(source = source.as_str(), %detail, reason = ?b, "away, deferred");
                    }
                    self.away.insert(source, Episode { detail, grace, since: now, consumed: false });
                }
            }
        }
        self.evaluate(now)
    }

    pub fn back(&mut self, now: Instant, source: Source, detail: String) -> Vec<Action> {
        self.notes.insert(source, detail.clone());
        self.away.remove(&source);
        let mut out = Vec::new();
        if self.pending.as_ref().is_some_and(|p| p.source == source) {
            self.pending = None;
            info!(source = source.as_str(), %detail, "back: countdown cancelled");
            out.push(Action::CountdownCancelled);
        }
        // Somebody else may still be away; their countdown starts now.
        out.extend(self.evaluate(now));
        out
    }

    pub fn lock_now(&mut self, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        if self.pending.take().is_some() {
            out.push(Action::CountdownCancelled);
        }
        out.push(self.do_lock(now, None, "asked to".to_string()));
        out
    }

    pub fn pause(&mut self, now: Instant, d: Duration) -> Vec<Action> {
        self.paused_until = Some(now + d);
        // A pulse cannot say whether it still holds when the pause ends.
        self.away.retain(|s, _| !s.is_pulse());
        let mut out = Vec::new();
        if self.pending.take().is_some() {
            info!("countdown cancelled by pause");
            out.push(Action::CountdownCancelled);
        }
        info!(secs = d.as_secs(), "paused");
        out
    }

    pub fn resume(&mut self, now: Instant) -> Vec<Action> {
        self.paused_until = None;
        info!("resumed");
        self.evaluate(now)
    }

    /// Time passed. Call at (or after) [`Core::next_wakeup`].
    pub fn tick(&mut self, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        let due = self.pending.as_ref().is_some_and(|p| now >= p.deadline);
        if due {
            if let Some(p) = self.pending.take() {
                out.push(self.do_lock(now, Some(p.source), p.detail));
            }
        }
        if self.paused_until.is_some_and(|t| now >= t) {
            self.paused_until = None;
        }
        out.extend(self.evaluate(now));
        out
    }

    /// When [`Core::tick`] next has something to do, if ever.
    pub fn next_wakeup(&self) -> Option<Instant> {
        if let Some(p) = &self.pending {
            return Some(p.deadline);
        }
        if !self.armed || !self.away.values().any(|e| !e.consumed) {
            return None;
        }
        // Someone is away but blocked: wake when every block has lifted.
        let pause = self.paused_until;
        let cool = self.last_lock.map(|t| t + self.cooldown);
        match (pause, cool) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    pub fn status(&self, now: Instant, dry_run: bool) -> Status {
        Status {
            armed: self.armed,
            dry_run,
            paused_secs: self.paused_until.filter(|t| *t > now).map(|t| (t - now).as_secs()),
            counting_down: self.pending.as_ref().map(|p| Countdown {
                source: p.source.as_str().to_string(),
                detail: p.detail.clone(),
                secs_left: p.deadline.saturating_duration_since(now).as_secs(),
            }),
            secs_since_lock: self.last_lock.map(|t| now.saturating_duration_since(t).as_secs()),
            triggers: self.notes.iter().map(|(k, v)| (k.as_str().to_string(), v.clone())).collect(),
        }
    }

    fn blocked(&self, now: Instant) -> Option<Block> {
        if !self.armed {
            return Some(Block::Disarmed);
        }
        if self.paused_until.is_some_and(|t| now < t) {
            return Some(Block::Paused);
        }
        if self.last_lock.is_some_and(|t| now < t + self.cooldown) {
            // Almost always a flapping link right after a lock, while the
            // password is being typed into the screen it would lock again.
            return Some(Block::Cooldown);
        }
        None
    }

    /// Start a countdown if somebody is away, nothing is counting and nothing
    /// is in the way.
    fn evaluate(&mut self, now: Instant) -> Vec<Action> {
        if self.pending.is_some() || self.blocked(now).is_some() {
            return Vec::new();
        }
        // The longest-standing absence goes first.
        let next = self
            .away
            .iter()
            .filter(|(_, e)| !e.consumed)
            .min_by_key(|(s, e)| (e.since, **s))
            .map(|(s, e)| (*s, e.detail.clone(), e.grace.unwrap_or(self.grace_secs)));
        let Some((source, detail, secs)) = next else {
            return Vec::new();
        };

        if secs == 0 {
            return vec![self.do_lock(now, Some(source), detail)];
        }
        info!(source = source.as_str(), %detail, secs, "away: counting down");
        self.pending = Some(Pending { source, detail, deadline: now + Duration::from_secs(secs) });
        vec![Action::CountdownStarted(secs)]
    }

    fn do_lock(&mut self, now: Instant, source: Option<Source>, detail: String) -> Action {
        self.last_lock = Some(now);
        // The screen is locked: every absence so far has been answered.
        self.away.retain(|s, _| !s.is_pulse());
        for ep in self.away.values_mut() {
            ep.consumed = true;
        }
        Action::Lock { source, detail }
    }
}

pub async fn run(cfg: Config, dry: bool, mut rx: mpsc::Receiver<Signal>) {
    let mut core = Core::from_config(&cfg);

    loop {
        let wake = core.next_wakeup();
        let sig = tokio::select! {
            msg = rx.recv() => match msg {
                Some(m) => Some(m),
                None => return,
            },
            _ = async {
                match wake {
                    Some(t) => tokio::time::sleep_until(t).await,
                    // Nothing scheduled: park this branch forever and let the
                    // channel drive.
                    None => std::future::pending().await,
                }
            } => None,
        };

        let now = Instant::now();
        let actions = match sig {
            None => core.tick(now),
            Some(Signal::Note { source, text }) => {
                core.note(source, text);
                Vec::new()
            }
            Some(Signal::Away { source, detail, grace }) => core.away(now, source, detail, grace),
            Some(Signal::Back { source, detail }) => core.back(now, source, detail),
            Some(Signal::LockNow) => core.lock_now(now),
            Some(Signal::Pause(d)) => core.pause(now, d),
            Some(Signal::Resume) => core.resume(now),
            Some(Signal::Status(reply)) => {
                if reply.send(core.status(now, dry)).is_err() {
                    warn!("status requester went away");
                }
                Vec::new()
            }
        };

        for a in actions {
            match a {
                Action::Lock { source, detail } => {
                    let source = source.map(Source::as_str).unwrap_or("user");
                    info!(source, %detail, "locking");
                    lock::lock(&cfg.general.lock_command, dry).await;
                }
                Action::CountdownStarted(secs) => {
                    if cfg.general.notch {
                        notch::lock_soon(secs).await;
                    }
                }
                Action::CountdownCancelled => {
                    if cfg.general.notch {
                        notch::cancel().await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRACE: u64 = 20;
    const COOLDOWN: u64 = 60;

    fn core() -> Core {
        Core::new(true, GRACE, COOLDOWN)
    }

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn locks(a: &[Action]) -> usize {
        a.iter().filter(|x| matches!(x, Action::Lock { .. })).count()
    }

    fn away(c: &mut Core, t: Instant, src: Source, grace: Option<u64>) -> Vec<Action> {
        c.away(t, src, format!("{} away", src.as_str()), grace)
    }

    fn back(c: &mut Core, t: Instant, src: Source) -> Vec<Action> {
        c.back(t, src, format!("{} back", src.as_str()))
    }

    #[test]
    fn away_counts_down_then_locks() {
        let t0 = Instant::now();
        let mut c = core();
        assert_eq!(away(&mut c, t0, Source::Beacon, None), vec![Action::CountdownStarted(GRACE)]);
        assert_eq!(c.next_wakeup(), Some(t0 + s(GRACE)));
        assert_eq!(locks(&c.tick(t0 + s(GRACE - 1))), 0, "must not lock before the deadline");
        let a = c.tick(t0 + s(GRACE));
        assert_eq!(locks(&a), 1);
        assert!(matches!(&a[0], Action::Lock { source: Some(Source::Beacon), .. }));
        assert_eq!(c.next_wakeup(), None, "nothing left to do after the lock");
    }

    #[test]
    fn per_source_grace_overrides_the_default() {
        let t0 = Instant::now();
        let mut c = core();
        assert_eq!(away(&mut c, t0, Source::Anchor, Some(3)), vec![Action::CountdownStarted(3)]);
        assert_eq!(locks(&c.tick(t0 + s(3))), 1);
    }

    #[test]
    fn zero_grace_locks_immediately() {
        let t0 = Instant::now();
        let mut c = core();
        assert_eq!(locks(&away(&mut c, t0, Source::Webhook, Some(0))), 1);
    }

    #[test]
    fn back_from_the_same_source_cancels() {
        let t0 = Instant::now();
        let mut c = core();
        away(&mut c, t0, Source::Beacon, None);
        assert_eq!(back(&mut c, t0 + s(5), Source::Beacon), vec![Action::CountdownCancelled]);
        assert_eq!(c.next_wakeup(), None);
        assert_eq!(locks(&c.tick(t0 + s(GRACE + 1))), 0);
    }

    #[test]
    fn back_from_another_source_does_not_cancel() {
        let t0 = Instant::now();
        let mut c = core();
        away(&mut c, t0, Source::Anchor, Some(3));
        assert!(back(&mut c, t0 + s(1), Source::Beacon).is_empty());
        assert_eq!(locks(&c.tick(t0 + s(3))), 1);
    }

    #[test]
    fn a_second_source_does_not_shorten_or_restart_the_window() {
        let t0 = Instant::now();
        let mut c = core();
        away(&mut c, t0, Source::Beacon, None);
        assert!(away(&mut c, t0 + s(5), Source::Anchor, Some(3)).is_empty());
        assert_eq!(c.next_wakeup(), Some(t0 + s(GRACE)));
        // Repeating the same evidence does not restart it either.
        assert!(away(&mut c, t0 + s(10), Source::Beacon, None).is_empty());
        assert_eq!(c.next_wakeup(), Some(t0 + s(GRACE)));
    }

    #[test]
    fn owner_back_hands_the_countdown_to_a_source_still_away() {
        // Headset walked out and came back, but the anchor is still on the
        // floor. Before this was fixed, the screen stayed unlocked.
        let t0 = Instant::now();
        let mut c = core();
        away(&mut c, t0, Source::Beacon, None);
        away(&mut c, t0 + s(2), Source::Anchor, Some(3));
        let a = back(&mut c, t0 + s(10), Source::Beacon);
        assert_eq!(a, vec![Action::CountdownCancelled, Action::CountdownStarted(3)]);
        assert_eq!(locks(&c.tick(t0 + s(13))), 1);
    }

    #[test]
    fn anchor_pulled_during_cooldown_locks_once_it_ends() {
        // Unlock, pull the anchor 30s later, walk off. Before this was fixed
        // the pull was discarded as "within cooldown" and never acted on.
        let t0 = Instant::now();
        let mut c = core();
        assert_eq!(locks(&c.lock_now(t0)), 1);
        assert!(away(&mut c, t0 + s(30), Source::Anchor, Some(3)).is_empty());
        assert_eq!(c.next_wakeup(), Some(t0 + s(COOLDOWN)));
        assert_eq!(c.tick(t0 + s(COOLDOWN)), vec![Action::CountdownStarted(3)]);
        assert_eq!(locks(&c.tick(t0 + s(COOLDOWN + 3))), 1);
    }

    #[test]
    fn plugging_back_in_during_cooldown_forgets_the_pull() {
        let t0 = Instant::now();
        let mut c = core();
        c.lock_now(t0);
        away(&mut c, t0 + s(10), Source::Anchor, Some(3));
        back(&mut c, t0 + s(20), Source::Anchor);
        assert_eq!(c.next_wakeup(), None);
        assert!(c.tick(t0 + s(COOLDOWN)).is_empty());
    }

    #[test]
    fn a_flapping_link_after_a_lock_does_not_relock() {
        let t0 = Instant::now();
        let mut c = core();
        away(&mut c, t0, Source::Beacon, Some(0));
        for i in 1..10u64 {
            assert!(back(&mut c, t0 + s(i * 4), Source::Beacon).is_empty());
            assert!(away(&mut c, t0 + s(i * 4 + 2), Source::Beacon, Some(0)).is_empty());
        }
        // It ends up back before the cooldown runs out.
        back(&mut c, t0 + s(50), Source::Beacon);
        assert!(c.tick(t0 + s(COOLDOWN)).is_empty());
    }

    #[test]
    fn one_absence_locks_at_most_once() {
        // Locked by the anchor; typing the password with the anchor still out
        // must not get locked again when the cooldown ends.
        let t0 = Instant::now();
        let mut c = core();
        away(&mut c, t0, Source::Anchor, Some(3));
        assert_eq!(locks(&c.tick(t0 + s(3))), 1);
        assert_eq!(c.next_wakeup(), None);
        assert!(c.tick(t0 + s(3 + COOLDOWN + 1)).is_empty());
        // Re-reporting the same absence does not re-arm it.
        assert!(away(&mut c, t0 + s(200), Source::Anchor, Some(3)).is_empty());
        // A new pull after plugging back in does.
        back(&mut c, t0 + s(300), Source::Anchor);
        assert_eq!(away(&mut c, t0 + s(400), Source::Anchor, Some(3)), vec![Action::CountdownStarted(3)]);
    }

    #[test]
    fn webhook_is_a_pulse() {
        let t0 = Instant::now();
        let mut c = core();
        assert_eq!(locks(&away(&mut c, t0, Source::Webhook, Some(0))), 1);
        // During the cooldown: dropped, not deferred.
        assert!(away(&mut c, t0 + s(10), Source::Webhook, Some(0)).is_empty());
        assert_eq!(c.next_wakeup(), None);
        assert!(c.tick(t0 + s(COOLDOWN)).is_empty());
        // After it: every post is a fresh decision.
        assert_eq!(locks(&away(&mut c, t0 + s(COOLDOWN + 1), Source::Webhook, Some(0))), 1);
    }

    #[test]
    fn disarmed_never_locks_on_its_own() {
        let t0 = Instant::now();
        let mut c = Core::new(false, GRACE, COOLDOWN);
        assert!(away(&mut c, t0, Source::Anchor, Some(0)).is_empty());
        assert!(away(&mut c, t0, Source::Webhook, Some(0)).is_empty());
        assert_eq!(c.next_wakeup(), None);
        assert!(c.tick(t0 + s(1000)).is_empty());
        // ...but a person asking still gets a lock.
        assert_eq!(locks(&c.lock_now(t0 + s(1001))), 1);
    }

    #[test]
    fn lock_now_cancels_the_countdown_and_locks() {
        let t0 = Instant::now();
        let mut c = core();
        away(&mut c, t0, Source::Beacon, None);
        let a = c.lock_now(t0 + s(1));
        assert_eq!(a[0], Action::CountdownCancelled);
        assert_eq!(locks(&a), 1);
        // The beacon absence was answered by that lock.
        assert!(c.tick(t0 + s(GRACE + COOLDOWN)).is_empty());
    }

    #[test]
    fn pause_cancels_and_holds_then_reconsiders() {
        let t0 = Instant::now();
        let mut c = core();
        away(&mut c, t0, Source::Anchor, Some(3));
        assert_eq!(c.pause(t0 + s(1), s(1800)), vec![Action::CountdownCancelled]);
        assert!(c.tick(t0 + s(4)).is_empty(), "paused: no lock at the old deadline");
        assert!(away(&mut c, t0 + s(5), Source::Beacon, None).is_empty());
        // The anchor is still out when the pause ends: a fresh countdown.
        assert_eq!(c.next_wakeup(), Some(t0 + s(1801)));
        assert_eq!(c.tick(t0 + s(1801)), vec![Action::CountdownStarted(3)]);
    }

    #[test]
    fn pause_drops_webhook_pulses() {
        let t0 = Instant::now();
        let mut c = core();
        away(&mut c, t0, Source::Webhook, Some(10));
        c.pause(t0 + s(1), s(60));
        assert!(away(&mut c, t0 + s(2), Source::Webhook, Some(0)).is_empty());
        assert_eq!(c.next_wakeup(), None);
        assert!(c.tick(t0 + s(61)).is_empty());
    }

    #[test]
    fn resume_reconsiders_immediately() {
        let t0 = Instant::now();
        let mut c = core();
        c.pause(t0, s(1800));
        away(&mut c, t0 + s(1), Source::Anchor, Some(3));
        assert_eq!(c.resume(t0 + s(2)), vec![Action::CountdownStarted(3)]);
    }

    #[test]
    fn pause_and_cooldown_both_have_to_lift() {
        let t0 = Instant::now();
        let mut c = core();
        c.lock_now(t0);
        c.pause(t0 + s(1), s(10));
        away(&mut c, t0 + s(2), Source::Anchor, Some(3));
        assert_eq!(c.next_wakeup(), Some(t0 + s(COOLDOWN)));
        assert!(c.tick(t0 + s(11)).is_empty());
        assert_eq!(c.tick(t0 + s(COOLDOWN)), vec![Action::CountdownStarted(3)]);
    }

    #[test]
    fn status_reports_the_countdown() {
        let t0 = Instant::now();
        let mut c = core();
        c.note(Source::Anchor, "present".into());
        away(&mut c, t0, Source::Beacon, None);
        let st = c.status(t0 + s(5), true);
        assert!(st.armed && st.dry_run);
        let cd = st.counting_down.expect("counting down");
        assert_eq!(cd.source, "beacon");
        assert_eq!(cd.secs_left, GRACE - 5);
        assert_eq!(st.triggers.get("anchor").map(String::as_str), Some("present"));
        assert_eq!(st.triggers.get("beacon").map(String::as_str), Some("beacon away"));
        assert!(st.paused_secs.is_none() && st.secs_since_lock.is_none());
    }
}
