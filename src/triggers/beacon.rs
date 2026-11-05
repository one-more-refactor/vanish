//! The headset as a presence beacon.
//!
//! The idea is that you already wear a radio that goes where you go. If the
//! headset is on your head and the headset leaves, you left.
//!
//! ## Why the link, and not the signal strength
//!
//! bluez only publishes `RSSI` for objects it hears ADVERTISING. A pair of
//! AirPods connected over classic bluetooth does not advertise under the
//! address it is paired as — the advertisement comes from a randomised BLE
//! address that rotates and has no link back to the paired object. So there is
//! no such thing as "the RSSI of my connected headset"; the honest primary
//! signal is `Connected` going false, which the kernel reports when the link
//! supervision timeout expires after you walk out of range.
//!
//! That is a few seconds later than a signal-strength threshold would be, and
//! it is the trade this makes deliberately: late and certain beats early and
//! wrong for something that locks your screen.
//!
//! The distance path is still available (`away_rssi`), matching Apple's
//! proximity advertisement by payload shape the way notchd does. It is off by
//! default because the right threshold is a property of your room, not of the
//! software. `vanish rssi` prints live readings so you can pick one.
//!
//! ## Why in-ear gates everything
//!
//! Without it the trigger is wrong in the most annoying possible way: taking
//! the buds out and dropping them in the case disconnects them, and the
//! session locks while you are sitting right in front of it. So the beacon
//! only counts as evidence while it has recently been WORN. Buds on the desk
//! are not a beacon, they are just bluetooth devices, and vanish ignores them
//! until you put them back in — at which point the anchor and the ordinary
//! idle timeout are what cover you.
//!
//! In-ear state comes from the same unencrypted Apple advertisement, so it
//! works with no pairing and no accessory channel. For a non-Apple headset,
//! there is no in-ear bit to read: set `require_in_ear = false` and accept
//! that "in the case" and "in another room" look the same.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{info, warn};
use zbus::zvariant::OwnedValue;
use zbus::{proxy, Connection};

use crate::arbiter::{Signal, Source};
use crate::config::Beacon;

/// Apple's company ID; every AirPods advertisement carries it.
const APPLE: u16 = 0x004C;
/// The proximity-pairing message: type 0x07, 27 bytes. Anything else from
/// Apple is handoff or nearby-info and says nothing about ears.
const PROXIMITY_TYPE: u8 = 0x07;
const PROXIMITY_LEN: usize = 27;

/// How often the world is re-read. bluez has no "tell me when this device got
/// further away" signal, and a link drop is already seconds late, so polling
/// at this rate costs nothing that matters.
const TICK: Duration = Duration::from_secs(2);
/// Exponential smoothing on RSSI. Raw readings swing 15 dB while you sit still.
const ALPHA: f32 = 0.3;

#[proxy(interface = "org.bluez.Adapter1", default_service = "org.bluez")]
trait Adapter {
    fn start_discovery(&self) -> zbus::Result<()>;
    fn stop_discovery(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn powered(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn discovering(&self) -> zbus::Result<bool>;
}

/// Everything the beacon believes right now.
#[derive(Default)]
pub struct Reading {
    pub connected: bool,
    pub name: String,
    /// Smoothed RSSI of the strongest Apple proximity advertisement in range.
    pub rssi: Option<f32>,
    pub in_ear: Option<bool>,
    pub last_ad: Option<Instant>,
    pub last_in_ear: Option<Instant>,
}

/// Where we are in the scan cycle, in seconds.
///
/// Deliberately keyed to the wall clock rather than to when this process
/// started. notchd runs the same duty cycle off the same clock, so the two
/// daemons land on the SAME window without talking to each other. Left to
/// their own start times they would drift apart, and since bluez scans
/// whenever any client wants it, two 20% cycles out of phase cost nearly 40%
/// of the airtime instead of 20%.
fn cycle_pos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub async fn run(cfg: Beacon, tx: mpsc::Sender<Signal>) -> Result<()> {
    if cfg.address.is_empty() {
        warn!("beacon enabled but no address set; see `bluetoothctl devices`");
        return Ok(());
    }
    let addr = cfg.address.to_uppercase();
    let conn = Connection::system().await?;

    // Discovery has to be running for any advertisement to reach us, and it
    // does not stay running by itself: a suspend/resume cycle re-initialises
    // the adapter and every client's discovery quietly ends. Asking once at
    // startup — worse, asking only when nobody else is discovering — leaves
    // the beacon permanently blind after the first time the machine sleeps,
    // and blind fails safe, so nothing ever complains. Observed on 2026-08-27:
    // Discovering=false with two daemons that both believed they had asked.
    //
    // So it is re-asserted on every tick. bluez reference-counts discovery per
    // D-Bus client, so this neither fights notchd nor accumulates state.
    let adapter = if cfg.own_discovery {
        match AdapterProxy::builder(&conn).path("/org/bluez/hci0")?.build().await {
            Ok(a) => Some(a),
            Err(e) => {
                warn!(%e, "no adapter at /org/bluez/hci0; in-ear and RSSI unavailable");
                None
            }
        }
    } else {
        None
    };

    let mut r = Reading::default();
    let mut judge = Judge::default();
    let mut last_note = String::new();

    // Only complain about discovery once per outage, not every two seconds.
    let mut discovery_warned = false;

    // Length of one scan/idle cycle, or None when the duty cycle is disabled.
    let cycle = match (cfg.scan_secs, cfg.scan_period_secs) {
        (0, _) | (_, 0) => None,
        (w, p) if w >= p => None,
        (_, p) => Some(p),
    };
    if let Some(p) = cycle {
        info!(scan = cfg.scan_secs, period = p, "beacon scanning on a duty cycle");
    }

    loop {
        tokio::time::sleep(TICK).await;

        // Scan in short windows rather than continuously. Discovery is still
        // re-asserted rather than asked for once — a suspend/resume silently
        // ends it, and blind fails safe, so nothing would ever complain — but
        // it is now also given back between windows so the radio is idle most
        // of the time. See the note on `scan_secs` in the config.
        if let Some(a) = &adapter {
            let want = match cycle {
                Some(len) => cycle_pos() % len < cfg.scan_secs,
                // Duty cycle switched off: hold discovery the way this used to.
                None => true,
            };
            if a.powered().await.unwrap_or(false) {
                let on = a.discovering().await.unwrap_or(false);
                if want && !on {
                    match a.start_discovery().await {
                        Ok(()) => {
                            if discovery_warned {
                                info!("discovery restarted");
                                discovery_warned = false;
                            }
                        }
                        Err(e) => {
                            if !discovery_warned {
                                warn!(%e, "cannot start discovery; the beacon is blind until this clears");
                                discovery_warned = true;
                            }
                        }
                    }
                } else if !want && on {
                    // Only drops OUR reference; notchd's scan, if it holds one,
                    // keeps the adapter discovering and this is a no-op.
                    let _ = a.stop_discovery().await;
                    discovery_warned = false;
                } else {
                    discovery_warned = false;
                }
            }
        }

        if let Err(e) = poll(&conn, &addr, &mut r).await {
            warn!(%e, "bluez read failed");
            continue;
        }

        let name = display_name(&r, &addr);
        let step = judge.step(&cfg, &r, &name, Instant::now());

        let note = summary(&r, step.armed, step.worn);
        if note != last_note {
            last_note = note.clone();
            let _ = tx.send(Signal::Note { source: Source::Beacon, text: note }).await;
        }

        match step.verdict {
            Some(Verdict::Away(detail)) => {
                let _ = tx
                    .send(Signal::Away { source: Source::Beacon, detail, grace: cfg.grace_secs })
                    .await;
            }
            Some(Verdict::Back(detail)) => {
                let _ = tx.send(Signal::Back { source: Source::Beacon, detail }).await;
            }
            None => {}
        }
    }
}

/// Is the headset on somebody's head, as far as we can tell?
///
/// "Recently in an ear" is not enough on its own: the last advertisement we
/// heard may say both buds are OUT, and buds that were just taken out and are
/// about to go in the case are exactly the false lock this gate exists for.
/// The memory only bridges the gaps where nothing was heard at all (between
/// scan windows, or a bud out for a conversation while the other stays in).
pub fn worn(cfg: &Beacon, r: &Reading, now: Instant) -> bool {
    if !cfg.require_in_ear {
        return true;
    }
    r.in_ear != Some(false)
        && r.last_in_ear
            .is_some_and(|t| now.duration_since(t) < Duration::from_secs(cfg.in_ear_memory_secs))
}

/// What the beacon concluded on one tick.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Away(String),
    Back(String),
}

#[derive(Debug)]
pub struct Step {
    /// Connected and worn: the beacon counts as evidence right now.
    pub armed: bool,
    pub worn: bool,
    pub verdict: Option<Verdict>,
}

/// The beacon's decision, kept apart from bluez so it can be tested.
///
/// Edge-triggered: one `Away` when the beacon starts saying "gone", one
/// `Back` when it stops. The arbiter treats the beacon as a state, so a
/// repeated `Away` would mean nothing, and a missing `Back` would leave it
/// believing you are gone forever.
#[derive(Debug, Default)]
pub struct Judge {
    /// Was the beacon trustworthy on the previous tick? Only a fall from armed
    /// to not-armed is news; starting up with the headset in its case is not.
    was_armed: bool,
    /// The link dropped while the beacon was worn, and has not come back.
    dropped: bool,
    below_since: Option<Instant>,
    gone: bool,
}

impl Judge {
    pub fn step(&mut self, cfg: &Beacon, r: &Reading, name: &str, now: Instant) -> Step {
        let worn = worn(cfg, r, now);
        let armed = r.connected && worn;

        // Only a LINK DROP while worn is evidence. Losing the "worn" status
        // while still connected (buds out on the desk, or the in-ear memory
        // running out) just means the beacon stops counting — the person is
        // very likely still sitting right there.
        if self.was_armed && !armed && !r.connected {
            self.dropped = true;
        }
        if r.connected {
            self.dropped = false;
        }

        // Distance, when it has been switched on.
        let far = if cfg.away_rssi != 0 {
            match r.rssi {
                Some(v) if v < cfg.away_rssi as f32 => {
                    let since = *self.below_since.get_or_insert(now);
                    now.duration_since(since) >= Duration::from_secs(cfg.rssi_hold_secs)
                }
                _ => {
                    self.below_since = None;
                    false
                }
            }
        } else {
            false
        };

        // Silence, when it has been switched on. Only meaningful while armed —
        // buds in a case stop advertising too.
        let silent = cfg.silence_secs != 0
            && armed
            && r.last_ad
                .is_some_and(|t| now.duration_since(t) >= Duration::from_secs(cfg.silence_secs));

        let far = armed && far;
        let gone = self.dropped || far || silent;
        let verdict = match (self.gone, gone) {
            (false, true) => Some(Verdict::Away(if self.dropped {
                format!("{name} disconnected")
            } else if far {
                format!(
                    "{name} is far ({} dBm for {}s)",
                    r.rssi.map(|v| v.round() as i32).unwrap_or_default(),
                    cfg.rssi_hold_secs
                )
            } else {
                format!("{name} stopped advertising")
            })),
            (true, false) => Some(Verdict::Back(format!("{name} is back"))),
            _ => None,
        };

        self.gone = gone;
        self.was_armed = armed;
        Step { armed, worn, verdict }
    }
}

fn display_name(r: &Reading, addr: &str) -> String {
    if r.name.is_empty() {
        addr.to_string()
    } else {
        r.name.clone()
    }
}

pub fn summary(r: &Reading, armed: bool, worn: bool) -> String {
    let mut s = String::new();
    s.push_str(if r.connected { "connected" } else { "disconnected" });
    if let Some(v) = r.rssi {
        s.push_str(&format!(", {} dBm", v.round() as i32));
    }
    match r.in_ear {
        Some(true) => s.push_str(", in ear"),
        Some(false) => s.push_str(", out of ear"),
        None => s.push_str(", no advertisement"),
    }
    if !worn {
        s.push_str(", not worn recently");
    }
    s.push_str(if armed { " — armed" } else { " — not a beacon right now" });
    s
}

/// One pass over bluez: the paired device's state, and the loudest Apple
/// proximity advertisement in the room.
pub async fn poll(conn: &Connection, addr: &str, r: &mut Reading) -> Result<()> {
    let om = zbus::fdo::ObjectManagerProxy::builder(conn)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?;
    let objects = om.get_managed_objects().await?;

    let mut best_rssi = i16::MIN;
    let mut best_in_ear: Option<bool> = None;
    let mut connected = false;
    let mut seen = false;

    for (_path, ifaces) in objects {
        let Some(dev) = ifaces.get("org.bluez.Device1") else { continue };

        if prop_str(dev, "Address").eq_ignore_ascii_case(addr) {
            seen = true;
            connected = prop_bool(dev, "Connected");
            let n = prop_str(dev, "Alias");
            r.name = if n.is_empty() { prop_str(dev, "Name") } else { n };
        }

        // The advertisement is on a DIFFERENT object from the paired device —
        // see the module comment. Take the loudest one: yours is reliably the
        // loudest Apple device in your own room, and picking arbitrarily makes
        // the reading flap between two people's earbuds.
        if let Some(md) = dev.get("ManufacturerData") {
            if let Some(ad) = decode_apple(md) {
                let rssi = dev.get("RSSI").and_then(|v| i16::try_from(v).ok()).unwrap_or(i16::MIN + 1);
                if rssi > best_rssi {
                    best_rssi = rssi;
                    best_in_ear = Some(ad);
                }
            }
        }
    }

    if !seen {
        // bluez has never heard of this address. Almost always a typo'd
        // address in the config, which would otherwise look like a headset
        // that is simply never connected.
        r.connected = false;
        return Ok(());
    }
    r.connected = connected;

    if let Some(in_ear) = best_in_ear {
        let now = Instant::now();
        r.last_ad = Some(now);
        r.in_ear = Some(in_ear);
        if in_ear {
            r.last_in_ear = Some(now);
        }
        let v = best_rssi as f32;
        r.rssi = Some(match r.rssi {
            Some(prev) => ALPHA * v + (1.0 - ALPHA) * prev,
            None => v,
        });
    }
    Ok(())
}

/// Is a bud in an ear, according to Apple's proximity advertisement?
///
/// Byte 5 is the status byte; bits 1 and 3 are the two in-ear flags, and which
/// one belongs to which bud depends on the primary bit — which does not matter
/// here, since either ear means the thing is being worn.
fn decode_apple(value: &OwnedValue) -> Option<bool> {
    let map = <HashMap<u16, OwnedValue>>::try_from(value.clone()).ok()?;
    let bytes = <Vec<u8>>::try_from(map.get(&APPLE)?.clone()).ok()?;
    in_ear_from_payload(&bytes)
}

fn in_ear_from_payload(bytes: &[u8]) -> Option<bool> {
    if bytes.len() != PROXIMITY_LEN || bytes[0] != PROXIMITY_TYPE {
        return None;
    }
    let status = bytes[5];
    Some(status & 0x02 != 0 || status & 0x08 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(status: u8) -> Vec<u8> {
        let mut v = vec![0u8; PROXIMITY_LEN];
        v[0] = PROXIMITY_TYPE;
        v[5] = status;
        v
    }

    #[test]
    fn reads_the_in_ear_bits() {
        // 0x22 is what these AirPods actually broadcast while worn — taken
        // from notchd's journal, not from a specification nobody published.
        assert_eq!(in_ear_from_payload(&payload(0x22)), Some(true));
        // The other bud being the primary one moves the flag to bit 3.
        assert_eq!(in_ear_from_payload(&payload(0x08)), Some(true));
        assert_eq!(in_ear_from_payload(&payload(0x00)), Some(false));
    }

    // ── Judge ────────────────────────────────────────────────────────────

    fn cfg() -> Beacon {
        Beacon { enabled: true, address: "AA:BB:CC:DD:EE:FF".into(), ..Beacon::default() }
    }

    /// Connected, last advertisement says in-ear, heard at `t`.
    fn worn_at(t: Instant) -> Reading {
        Reading {
            connected: true,
            name: "buds".into(),
            rssi: Some(-55.0),
            in_ear: Some(true),
            last_ad: Some(t),
            last_in_ear: Some(t),
        }
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn walking_away_while_worn_fires_once_then_comes_back() {
        let (c, t0) = (cfg(), Instant::now());
        let mut j = Judge::default();
        let mut r = worn_at(t0);
        let st = j.step(&c, &r, "buds", t0);
        assert!(st.armed && st.verdict.is_none(), "becoming armed is not news");

        r.connected = false;
        let st = j.step(&c, &r, "buds", t0 + secs(2));
        assert_eq!(st.verdict, Some(Verdict::Away("buds disconnected".into())));
        // Still gone: no repeat.
        assert_eq!(j.step(&c, &r, "buds", t0 + secs(4)).verdict, None);
        assert_eq!(j.step(&c, &r, "buds", t0 + secs(6)).verdict, None);

        r.connected = true;
        let st = j.step(&c, &r, "buds", t0 + secs(8));
        assert_eq!(st.verdict, Some(Verdict::Back("buds is back".into())));
    }

    #[test]
    fn buds_into_the_case_do_not_fire() {
        // The false lock the in-ear gate exists for: both buds out (and the
        // advertisement says so), then the case closes and the link drops.
        let (c, t0) = (cfg(), Instant::now());
        let mut j = Judge::default();
        let mut r = worn_at(t0);
        j.step(&c, &r, "buds", t0);

        r.in_ear = Some(false);
        let st = j.step(&c, &r, "buds", t0 + secs(2));
        assert!(!st.armed && !st.worn);
        assert_eq!(st.verdict, None, "taking them out is not leaving");

        r.connected = false;
        assert_eq!(j.step(&c, &r, "buds", t0 + secs(4)).verdict, None);
    }

    #[test]
    fn in_ear_memory_running_out_while_connected_does_not_fire() {
        // Used to fire "worn beacon went quiet" and lock a person sitting at
        // the desk with the buds connected but not worn for ten minutes.
        let (c, t0) = (cfg(), Instant::now());
        let mut j = Judge::default();
        let r = worn_at(t0);
        j.step(&c, &r, "buds", t0);
        let st = j.step(&c, &r, "buds", t0 + secs(c.in_ear_memory_secs + 1));
        assert!(!st.armed);
        assert_eq!(st.verdict, None);
    }

    #[test]
    fn starting_up_disconnected_or_unworn_is_not_news() {
        let (c, t0) = (cfg(), Instant::now());
        let mut j = Judge::default();
        let r = Reading::default();
        for i in 0..5 {
            assert_eq!(j.step(&c, &r, "buds", t0 + secs(i * 2)).verdict, None);
        }
    }

    #[test]
    fn without_require_in_ear_any_link_drop_counts() {
        let c = Beacon { require_in_ear: false, ..cfg() };
        let t0 = Instant::now();
        let mut j = Judge::default();
        let mut r = Reading { connected: true, ..Reading::default() };
        assert!(j.step(&c, &r, "headset", t0).armed);
        r.connected = false;
        assert_eq!(
            j.step(&c, &r, "headset", t0 + secs(2)).verdict,
            Some(Verdict::Away("headset disconnected".into()))
        );
    }

    #[test]
    fn distance_path_needs_the_signal_to_stay_low() {
        let c = Beacon { away_rssi: -80, rssi_hold_secs: 15, ..cfg() };
        let t0 = Instant::now();
        let mut j = Judge::default();
        let mut r = worn_at(t0);
        j.step(&c, &r, "buds", t0);

        r.rssi = Some(-90.0);
        assert_eq!(j.step(&c, &r, "buds", t0 + secs(2)).verdict, None);
        // A single good reading resets the hold timer.
        r.rssi = Some(-70.0);
        assert_eq!(j.step(&c, &r, "buds", t0 + secs(10)).verdict, None);
        r.rssi = Some(-90.0);
        assert_eq!(j.step(&c, &r, "buds", t0 + secs(12)).verdict, None);
        assert_eq!(j.step(&c, &r, "buds", t0 + secs(20)).verdict, None);
        let st = j.step(&c, &r, "buds", t0 + secs(27));
        assert!(matches!(st.verdict, Some(Verdict::Away(ref d)) if d.contains("is far")));

        r.rssi = Some(-60.0);
        assert!(matches!(j.step(&c, &r, "buds", t0 + secs(29)).verdict, Some(Verdict::Back(_))));
    }

    #[test]
    fn distance_path_is_off_by_default() {
        let (c, t0) = (cfg(), Instant::now());
        let mut j = Judge::default();
        let mut r = worn_at(t0);
        r.rssi = Some(-100.0);
        for i in 0..30 {
            assert_eq!(j.step(&c, &r, "buds", t0 + secs(i)).verdict, None);
        }
    }

    #[test]
    fn silence_backstop() {
        let c = Beacon { silence_secs: 60, ..cfg() };
        let t0 = Instant::now();
        let mut j = Judge::default();
        let r = worn_at(t0);
        assert_eq!(j.step(&c, &r, "buds", t0 + secs(59)).verdict, None);
        let st = j.step(&c, &r, "buds", t0 + secs(60));
        assert_eq!(st.verdict, Some(Verdict::Away("buds stopped advertising".into())));
    }

    #[test]
    fn ignores_apples_other_broadcasts() {
        // Handoff and nearby-info are also company ID 0x004C.
        assert_eq!(in_ear_from_payload(&[0x0c, 0x00, 0x00]), None);
        let mut wrong_len = payload(0x22);
        wrong_len.pop();
        assert_eq!(in_ear_from_payload(&wrong_len), None);
    }
}

fn prop_str(m: &HashMap<String, OwnedValue>, key: &str) -> String {
    m.get(key).and_then(|v| <&str>::try_from(v).ok()).unwrap_or_default().to_string()
}

fn prop_bool(m: &HashMap<String, OwnedValue>, key: &str) -> bool {
    m.get(key).and_then(|v| bool::try_from(v).ok()).unwrap_or(false)
}
