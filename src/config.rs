//! Configuration.
//!
//! One TOML file at `$XDG_CONFIG_HOME/vanish/config.toml`. Every field has a
//! default that is safe on a machine nobody has tuned yet: the beacon needs an
//! address before it does anything, the anchor needs a vendor/product, and the
//! webhook needs a token. Absent any of those, that trigger stays quiet rather
//! than guessing.

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: General,
    pub anchor: Anchor,
    pub beacon: Beacon,
    pub webhook: Webhook,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    /// Master switch. `false` observes and logs but never locks — the same
    /// thing `--dry-run` does, kept in the file so it survives a restart.
    pub armed: bool,
    /// Seconds between "you appear to be gone" and the lock. The window exists
    /// to be cancelled: a headset that reconnects or an anchor that goes back
    /// in during the countdown calls the whole thing off.
    pub grace_secs: u64,
    /// Refuse to lock again this soon after the last lock. Without it, a
    /// flapping bluetooth link re-locks the session every few seconds while
    /// you are typing your password into it.
    pub cooldown_secs: u64,
    /// Shell command to lock with. Empty means logind — see `lock.rs` for why
    /// that is the better default here.
    pub lock_command: String,
    /// Tell notchd to run its countdown during the grace window. Silently a
    /// no-op if notchd is not running.
    pub notch: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Anchor {
    pub enabled: bool,
    /// From `lsusb`, e.g. "1050". `vanish learn` fills these in for you.
    pub vendor_id: String,
    pub product_id: String,
    /// Optional: pin to one physical device when you own two of the same model.
    pub serial: String,
    /// Backstop poll interval, for a netlink event that never arrives. An
    /// anchor that is already absent at startup does NOT fire: this locks a
    /// screen rather than cutting power, and a lock at login is just rude.
    pub poll_ms: u64,
    /// Override the global grace. A pulled cable is a deliberate act, so the
    /// default is much shorter than the beacon's.
    pub grace_secs: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Beacon {
    pub enabled: bool,
    /// Classic bluetooth address of the headset, from `bluetoothctl devices`.
    pub address: String,
    /// Only treat the beacon as evidence while it has recently been ON YOUR
    /// HEAD. See the module comment in `triggers/beacon.rs` — this is the
    /// single setting that separates "walked away" from "put them in the case".
    pub require_in_ear: bool,
    /// How long an in-ear reading stays valid. Long enough to cover a bud
    /// taken out for a conversation, short enough that yesterday does not count.
    pub in_ear_memory_secs: u64,
    /// Ask bluez to keep scanning. Needed for the proximity advertisement that
    /// carries in-ear state; harmless if something else (notchd) already does.
    pub own_discovery: bool,
    /// Lock when smoothed RSSI stays below this for `rssi_hold_secs`.
    /// 0 disables the distance path entirely — see `vanish rssi` to pick one.
    pub away_rssi: i32,
    pub rssi_hold_secs: u64,
    /// Lock when no advertisement has been heard at all for this long while
    /// the beacon is armed. 0 disables. Slower than the link drop in practice,
    /// so it is a backstop, not the main path.
    pub silence_secs: u64,
    pub grace_secs: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Webhook {
    pub enabled: bool,
    /// Loopback by default. Anything else puts a lock button on your network.
    pub bind: String,
    /// 32 hex bytes. `vanish gen-token` prints one. Empty = the listener
    /// refuses to start, because an unauthenticated one is a denial-of-service
    /// against your own session.
    pub token: String,
    pub path: String,
    pub grace_secs: Option<u64>,
}

impl Default for General {
    fn default() -> Self {
        Self {
            armed: true,
            grace_secs: 20,
            cooldown_secs: 60,
            lock_command: String::new(),
            notch: true,
        }
    }
}

impl Default for Anchor {
    fn default() -> Self {
        Self {
            enabled: false,
            vendor_id: String::new(),
            product_id: String::new(),
            serial: String::new(),
            poll_ms: 2000,
            grace_secs: Some(3),
        }
    }
}

impl Default for Beacon {
    fn default() -> Self {
        Self {
            enabled: false,
            address: String::new(),
            require_in_ear: true,
            in_ear_memory_secs: 600,
            own_discovery: true,
            away_rssi: 0,
            rssi_hold_secs: 15,
            silence_secs: 0,
            grace_secs: None,
        }
    }
}

impl Default for Webhook {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: "127.0.0.1:9911".to_string(),
            token: String::new(),
            path: "/lock".to_string(),
            grace_secs: Some(0),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: General::default(),
            anchor: Anchor::default(),
            beacon: Beacon::default(),
            webhook: Webhook::default(),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        config_home().join("vanish/config.toml")
    }

    /// Read the file, or fall back to defaults if it is not there. A file that
    /// exists but does not parse is an error: silently running on defaults
    /// because of a typo is how a security tool ends up switched off without
    /// anyone noticing.
    pub fn load() -> anyhow::Result<Config> {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(s) => {
                let cfg: Config = toml::from_str(&s)
                    .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
        }
    }
}

pub fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default())
                .join(".config")
        })
}

pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_inert() {
        let c = Config::default();
        assert!(!c.anchor.enabled && !c.beacon.enabled && !c.webhook.enabled);
    }

    #[test]
    fn example_config_parses() {
        let s = include_str!("../vanish.toml.example");
        let c: Config = toml::from_str(s).expect("example config must parse");
        // The example ships armed, because a lock is not a destructive act and
        // a tool that needs a second step to do anything gets abandoned.
        assert!(c.general.armed);
    }
}
