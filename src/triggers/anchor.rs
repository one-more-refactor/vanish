//! The kill stick.
//!
//! One USB device is the anchor. Pull it — off a lanyard, off a magnetic
//! breakaway cable, or just out of the port on your way out of the room — and
//! the session locks. It is the deliberate, instant trigger: no proximity
//! guessing, no radio, no ambiguity about what you meant.
//!
//! Two independent detectors, because one path is one point of failure:
//!
//!   1. netlink uevents — the kernel says `remove@` the moment it happens;
//!   2. a sysfs poll every `poll_ms` — catches anything the socket missed
//!      (a dropped datagram, a removal that raced startup).
//!
//! Both feed the same `present` flag, so whichever notices first wins and the
//! other one finds nothing left to report.
//!
//! Startup with the anchor already absent does NOT fire. deadhand's equivalent
//! does, because there the trigger cuts power on a machine that may have been
//! tampered with; here it would mean logging in and being locked out again by
//! a daemon that started before you plugged anything in.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::arbiter::{Signal, Source};
use crate::config::Anchor;
use crate::uevent::{Uevent, UeventSocket};

struct State {
    present: bool,
    /// Kernel devpath of the anchor while it is plugged in — the most precise
    /// way to recognise its removal, since by then sysfs is already gone.
    devpath: Option<String>,
}

pub fn spawn(cfg: Anchor, tx: mpsc::Sender<Signal>) {
    let want = match Want::from_config(&cfg) {
        Some(w) => w,
        None => {
            warn!("anchor enabled but no vendor_id/product_id set; run `vanish learn`");
            return;
        }
    };

    // A pulled anchor is a deliberate act and gets the short window from
    // [anchor].grace_secs. This used to be hardcoded to None, so the config key
    // documented itself in the example file and then did nothing: an anchor
    // pull sat through the full 20-second beacon grace instead of 3.
    let grace = cfg.grace_secs;
    let found = find(&want);
    let state = Arc::new(Mutex::new(State {
        present: found.is_some(),
        devpath: found.clone(),
    }));

    let note = match &found {
        Some(p) => format!("present ({p})"),
        None => "not plugged in — will arm when it appears".to_string(),
    };
    info!(anchor = %want, %note, "anchor watching");
    // try_send, not blocking_send: this runs on the runtime thread that called
    // spawn(), and blocking there deadlocks tokio. The detector threads below
    // are real OS threads and may block all they like.
    let _ = tx.try_send(Signal::Note { source: Source::Anchor, text: note });

    // Detector 1: the kernel's own broadcast.
    {
        let (want, state, tx) = (want.clone(), state.clone(), tx.clone());
        std::thread::Builder::new()
            .name("anchor-uevent".into())
            .spawn(move || match UeventSocket::open() {
                Ok(mut sock) => loop {
                    match sock.recv() {
                        Ok(ev) => {
                            let devpath = state.lock().unwrap().devpath.clone();
                            match classify(&ev, devpath.as_deref(), &want) {
                                Some(Edge::Removed) => {
                                    if !update(&state, &tx, grace, false, "unplugged") {
                                        return;
                                    }
                                }
                                // An `add` carries no serial, so re-check sysfs
                                // rather than trusting the event.
                                Some(Edge::MaybeAdded) => {
                                    if let Some(p) = find(&want) {
                                        state.lock().unwrap().devpath = Some(p.clone());
                                        if !update(&state, &tx, grace, true, "plugged back in") {
                                            return;
                                        }
                                    }
                                }
                                None => {}
                            }
                        }
                        Err(e) => {
                            warn!(%e, "uevent socket read failed; falling back to polling");
                            return;
                        }
                    }
                },
                // Not fatal: the poll below still sees the removal, a second or
                // two later. Say so loudly, because a degraded detector that
                // says nothing is worse than no detector.
                Err(e) => warn!(%e, "cannot open uevent socket; polling only"),
            })
            .expect("spawn anchor-uevent");
    }

    // Detector 2: the backstop.
    {
        let interval = Duration::from_millis(cfg.poll_ms.max(250));
        std::thread::Builder::new()
            .name("anchor-poll".into())
            .spawn(move || loop {
                std::thread::sleep(interval);
                let alive = match find(&want) {
                    Some(p) => {
                        state.lock().unwrap().devpath = Some(p);
                        update(&state, &tx, grace, true, "plugged back in")
                    }
                    None => update(&state, &tx, grace, false, "gone (noticed by poll)"),
                };
                if !alive {
                    return;
                }
            })
            .expect("spawn anchor-poll");
    }
}

/// What one uevent means for the anchor.
#[derive(Debug, PartialEq, Eq)]
enum Edge {
    /// The anchor itself left.
    Removed,
    /// Something of the anchor's model arrived; sysfs decides if it is ours.
    MaybeAdded,
}

fn classify(ev: &Uevent, devpath: Option<&str>, want: &Want) -> Option<Edge> {
    if ev.subsystem != "usb" {
        return None;
    }
    match ev.action.as_str() {
        // Only the exact device we found. Matching on the model here used to
        // let a second device of the same model — or the twin of a pinned
        // serial — lock the session by being unplugged. Interfaces have their
        // own devpaths (`3-2:1.0`) and are ignored; the device's own `remove`
        // follows them.
        "remove" => (devpath == Some(ev.devpath.as_str())).then_some(Edge::Removed),
        "add" | "bind" => ev
            .props
            .get("PRODUCT")
            .is_some_and(|p| want.matches_product(p))
            .then_some(Edge::MaybeAdded),
        _ => None,
    }
}

/// Change the flag and report it — but only on an edge. Both detectors call
/// this constantly; only the transition is news.
fn update(
    state: &Arc<Mutex<State>>,
    tx: &mpsc::Sender<Signal>,
    grace: Option<u64>,
    present: bool,
    why: &str,
) -> bool {
    let mut st = state.lock().unwrap();
    if st.present == present {
        return true;
    }
    st.present = present;
    if !present {
        st.devpath = None;
    }
    drop(st);

    let sig = if present {
        Signal::Back { source: Source::Anchor, detail: why.to_string() }
    } else {
        Signal::Away { source: Source::Anchor, detail: why.to_string(), grace }
    };
    tx.blocking_send_or_log(sig)
}

#[derive(Clone)]
pub struct Want {
    vendor: u16,
    product: u16,
    serial: Option<String>,
}

impl std::fmt::Display for Want {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04x}:{:04x}", self.vendor, self.product)?;
        if let Some(s) = &self.serial {
            write!(f, " serial={s}")?;
        }
        Ok(())
    }
}

impl Want {
    fn from_config(cfg: &Anchor) -> Option<Want> {
        let vendor = u16::from_str_radix(cfg.vendor_id.trim_start_matches("0x"), 16).ok()?;
        let product = u16::from_str_radix(cfg.product_id.trim_start_matches("0x"), 16).ok()?;
        let serial = (!cfg.serial.is_empty()).then(|| cfg.serial.to_lowercase());
        Some(Want { vendor, product, serial })
    }

    /// A uevent's `PRODUCT=` is `vendor/product/bcd` in hex with NO zero
    /// padding, so "1050/402/543" is the YubiKey that lsusb calls 1050:0402.
    /// Comparing the strings would silently never match.
    fn matches_product(&self, raw: &str) -> bool {
        let mut it = raw.split('/');
        let v = it.next().and_then(|s| u16::from_str_radix(s, 16).ok());
        let p = it.next().and_then(|s| u16::from_str_radix(s, 16).ok());
        v == Some(self.vendor) && p == Some(self.product)
    }
}

fn read_trim(path: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(path.join(name)).ok().map(|s| s.trim().to_lowercase())
}

/// The anchor's kernel devpath, if it is plugged in right now.
fn find(want: &Want) -> Option<String> {
    for e in std::fs::read_dir("/sys/bus/usb/devices").ok()?.flatten() {
        let dir: PathBuf = e.path();
        // Only usb_device directories have idVendor; interfaces (1-2:1.0) do
        // not, and matching one would give us a devpath that disappears when
        // a driver unbinds rather than when the device leaves.
        let Some(v) = read_trim(&dir, "idVendor") else { continue };
        let Some(p) = read_trim(&dir, "idProduct") else { continue };
        if u16::from_str_radix(&v, 16).ok() != Some(want.vendor) {
            continue;
        }
        if u16::from_str_radix(&p, 16).ok() != Some(want.product) {
            continue;
        }
        if let Some(s) = &want.serial {
            if read_trim(&dir, "serial").as_ref() != Some(s) {
                continue;
            }
        }
        let canon = std::fs::canonicalize(&dir).unwrap_or(dir);
        let devpath = canon.to_string_lossy().strip_prefix("/sys")?.to_string();
        if !devpath.is_empty() {
            return Some(devpath);
        }
    }
    None
}

/// `blocking_send` from a plain thread, with the closed-channel case logged
/// once instead of unwrapped into a panic that would take the detector with it.
trait BlockingSendOrLog {
    fn blocking_send_or_log(&self, sig: Signal) -> bool;
}

impl BlockingSendOrLog for mpsc::Sender<Signal> {
    fn blocking_send_or_log(&self, sig: Signal) -> bool {
        match self.blocking_send(sig) {
            Ok(()) => true,
            Err(_) => {
                warn!("arbiter channel closed; anchor detector stopping");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn want() -> Want {
        Want { vendor: 0x1050, product: 0x0402, serial: None }
    }

    fn ev(action: &str, devpath: &str, product: &str) -> Uevent {
        let mut e = Uevent {
            action: action.into(),
            devpath: devpath.into(),
            subsystem: "usb".into(),
            ..Uevent::default()
        };
        e.props.insert("PRODUCT".into(), product.into());
        e
    }

    const OURS: &str = "/devices/pci0000:00/0000:00:14.0/usb3/3-2";
    const TWIN: &str = "/devices/pci0000:00/0000:00:14.0/usb3/3-4";

    #[test]
    fn removal_of_the_anchor_itself() {
        assert_eq!(classify(&ev("remove", OURS, "1050/402/543"), Some(OURS), &want()), Some(Edge::Removed));
    }

    #[test]
    fn removal_of_an_identical_twin_is_not_ours() {
        assert_eq!(classify(&ev("remove", TWIN, "1050/402/543"), Some(OURS), &want()), None);
    }

    #[test]
    fn removal_of_an_interface_is_not_the_device() {
        let iface = format!("{OURS}/3-2:1.0");
        assert_eq!(classify(&ev("remove", &iface, "1050/402/543"), Some(OURS), &want()), None);
    }

    #[test]
    fn removal_while_already_absent_is_nothing() {
        assert_eq!(classify(&ev("remove", OURS, "1050/402/543"), None, &want()), None);
    }

    #[test]
    fn arrivals_are_matched_by_model_and_checked_later() {
        assert_eq!(classify(&ev("add", TWIN, "1050/402/543"), None, &want()), Some(Edge::MaybeAdded));
        assert_eq!(classify(&ev("bind", OURS, "1050/402/543"), None, &want()), Some(Edge::MaybeAdded));
        assert_eq!(classify(&ev("add", OURS, "46d/c52b/1211"), None, &want()), None);
        assert_eq!(classify(&ev("change", OURS, "1050/402/543"), Some(OURS), &want()), None);
    }

    #[test]
    fn other_subsystems_are_ignored() {
        let mut e = ev("remove", OURS, "1050/402/543");
        e.subsystem = "block".into();
        assert_eq!(classify(&e, Some(OURS), &want()), None);
    }

    #[test]
    fn config_ids_parse_with_or_without_0x() {
        let mut a = Anchor { vendor_id: "0x1050".into(), product_id: "0402".into(), ..Anchor::default() };
        let w = Want::from_config(&a).expect("valid ids");
        assert_eq!((w.vendor, w.product, w.serial.as_deref()), (0x1050, 0x0402, None));
        a.serial = "ABC123".into();
        assert_eq!(Want::from_config(&a).unwrap().serial.as_deref(), Some("abc123"), "serials compare lowercase");
        a.vendor_id = String::new();
        assert!(Want::from_config(&a).is_none(), "no ids, no anchor");
        a.vendor_id = "zzzz".into();
        assert!(Want::from_config(&a).is_none());
    }

    #[test]
    fn product_prop_is_unpadded_hex() {
        // The kernel writes it unpadded; parsing as a number rather than
        // comparing strings is what makes both spellings work.
        assert!(want().matches_product("1050/402/543"));
        assert!(want().matches_product("1050/0402/543"));
        assert!(!want().matches_product("1d6b/2/610"));
        assert!(!want().matches_product("nonsense"));
    }
}
