//! vanish — the desktop locks itself when you are not there.
//!
//! Three ways of noticing, one decision:
//!
//!   * an **anchor**: a USB stick or breakaway cable. Pull it, the screen locks.
//!   * a **beacon**: the headset you are already wearing. Walk out of range
//!     while wearing it and the screen locks.
//!   * a **webhook**: anything else that can make an HTTP request — a camera
//!     that stopped seeing a face, a sensor, a phone geofence.
//!
//! It is the polite sibling of `deadhand`, which watches the same USB anchor
//! and cuts the power instead. This one only ever locks, which is why it can
//! afford to be wrong occasionally, and why the grace window exists.

mod arbiter;
mod config;
mod control;
mod lock;
mod notch;
mod triggers;
mod uevent;

use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::arbiter::Signal;
use crate::config::Config;
use crate::control::Request;

const USAGE: &str = "\
vanish — presence-based session lock

USAGE:
    vanish [run] [--dry-run]     run the daemon (default)
    vanish status                what it currently believes
    vanish pause [DURATION]      stop locking for a while (default 30m)
    vanish resume                undo a pause
    vanish lock                  lock right now
    vanish learn                 identify a USB anchor by unplugging it
    vanish rssi [ADDRESS]        live beacon readings, for tuning away_rssi
    vanish gen-token             print a webhook token

DURATION is 90s, 30m, 2h or a plain number of seconds.
Config: ~/.config/vanish/config.toml
";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("run");

    match cmd {
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        "gen-token" => {
            println!("{}", gen_token()?);
            Ok(())
        }
        "learn" => learn(),
        "rssi" => {
            init_logging();
            rssi(args.get(1).cloned()).await
        }
        "status" => {
            let v = control::ask(Request::Status).await?;
            print_status(&v);
            Ok(())
        }
        "pause" => {
            let secs = args.get(1).map(|s| parse_duration(s)).transpose()?.unwrap_or(1800);
            control::ask(Request::Pause { secs }).await?;
            println!("paused for {}", human(secs));
            Ok(())
        }
        "resume" => {
            control::ask(Request::Resume).await?;
            println!("resumed");
            Ok(())
        }
        "lock" => {
            control::ask(Request::Lock).await?;
            Ok(())
        }
        "run" | "--dry-run" | "-n" => {
            let dry = args.iter().any(|a| a == "--dry-run" || a == "-n");
            init_logging();
            daemon(dry).await
        }
        other => {
            eprintln!("vanish: unknown command {other:?}\n");
            print!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("VANISH_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .init();
}

async fn daemon(dry: bool) -> Result<()> {
    let cfg = Config::load()?;
    info!(
        armed = cfg.general.armed,
        dry_run = dry,
        grace = cfg.general.grace_secs,
        anchor = cfg.anchor.enabled,
        beacon = cfg.beacon.enabled,
        webhook = cfg.webhook.enabled,
        "vanish starting"
    );
    if !cfg.anchor.enabled && !cfg.beacon.enabled && !cfg.webhook.enabled {
        warn!("every trigger is disabled — vanish will do nothing. See {}", Config::path().display());
    }

    let (tx, rx) = mpsc::channel::<Signal>(64);

    if cfg.anchor.enabled {
        triggers::anchor::spawn(cfg.anchor.clone(), tx.clone());
    }
    if cfg.beacon.enabled {
        let (c, t) = (cfg.beacon.clone(), tx.clone());
        tokio::spawn(async move {
            if let Err(e) = triggers::beacon::run(c, t).await {
                warn!(%e, "beacon stopped");
            }
        });
    }
    if cfg.webhook.enabled {
        let (c, t) = (cfg.webhook.clone(), tx.clone());
        tokio::spawn(async move {
            if let Err(e) = triggers::webhook::run(c, t).await {
                warn!(%e, "webhook stopped");
            }
        });
    }
    {
        let t = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = control::run(t).await {
                warn!(%e, "control socket stopped");
            }
        });
    }

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = arbiter::run(cfg, dry, rx) => {}
        _ = term.recv() => info!("SIGTERM"),
        _ = tokio::signal::ctrl_c() => info!("interrupted"),
    }
    let _ = std::fs::remove_file(control::socket_path());
    Ok(())
}

/// Identify the anchor the only way that cannot be got wrong: by watching the
/// user unplug it. Reading `lsusb` and picking a line is how people end up
/// with their own keyboard as the anchor.
fn learn() -> Result<()> {
    use crate::uevent::UeventSocket;

    let mut sock = UeventSocket::open()
        .map_err(|e| anyhow::anyhow!("cannot open the uevent socket: {e}"))?;
    eprintln!("Unplug the device you want to use as the anchor, then plug it back in.");
    eprintln!("(Waiting…  Ctrl-C to give up.)\n");

    loop {
        let ev = sock.recv()?;
        if ev.subsystem != "usb" || ev.action != "add" {
            continue;
        }
        // Interfaces produce `add` too; only the device itself has a serial and
        // is the thing that disappears when the plug leaves.
        if ev.props.get("DEVTYPE").map(String::as_str) != Some("usb_device") {
            continue;
        }
        let dir = std::path::PathBuf::from("/sys").join(ev.devpath.trim_start_matches('/'));
        let read = |n: &str| std::fs::read_to_string(dir.join(n)).ok().map(|s| s.trim().to_string());
        let (Some(v), Some(p)) = (read("idVendor"), read("idProduct")) else { continue };
        let name = format!(
            "{} {}",
            read("manufacturer").unwrap_or_default(),
            read("product").unwrap_or_default()
        );

        println!("# {}", name.trim());
        println!("[anchor]");
        println!("enabled = true");
        println!("vendor_id = \"{v}\"");
        println!("product_id = \"{p}\"");
        match read("serial") {
            Some(s) if !s.is_empty() => println!("serial = \"{s}\""),
            _ => println!("# serial = \"\"   # this device does not report one"),
        }
        return Ok(());
    }
}

/// Live beacon telemetry. The numbers `away_rssi` wants are the ones you see
/// here when you stand where "away" means.
async fn rssi(addr: Option<String>) -> Result<()> {
    let addr = match addr {
        Some(a) => a,
        None => {
            let cfg = Config::load()?;
            if cfg.beacon.address.is_empty() {
                anyhow::bail!("no address: pass one, or set beacon.address in the config");
            }
            cfg.beacon.address
        }
    };
    let addr = addr.to_uppercase();
    let conn = zbus::Connection::system().await?;
    let mut r = triggers::beacon::Reading::default();
    eprintln!("Watching {addr}. Walk to where 'away' should start; note the dBm.\n");
    loop {
        if let Err(e) = triggers::beacon::poll(&conn, &addr, &mut r).await {
            eprintln!("bluez: {e}");
        }
        let worn = r.last_in_ear.is_some_and(|t| t.elapsed() < Duration::from_secs(600));
        println!("{}", triggers::beacon::summary(&r, r.connected && worn, worn));
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn print_status(v: &serde_json::Value) {
    let s = |k: &str| v.get(k).cloned().unwrap_or(serde_json::Value::Null);
    let armed = s("armed").as_bool().unwrap_or(false);
    let dry = s("dry_run").as_bool().unwrap_or(false);

    let mut head = if armed { "armed".to_string() } else { "DISARMED".to_string() };
    if dry {
        head.push_str(" (dry run — it will not actually lock)");
    }
    if let Some(p) = s("paused_secs").as_u64() {
        head = format!("paused, {} left", human(p));
    }
    println!("{head}");

    if let Some(c) = v.get("counting_down").and_then(|c| c.as_object()) {
        println!(
            "locking in {}s — {} ({})",
            c.get("secs_left").and_then(|x| x.as_u64()).unwrap_or(0),
            c.get("source").and_then(|x| x.as_str()).unwrap_or("?"),
            c.get("detail").and_then(|x| x.as_str()).unwrap_or("")
        );
    }
    if let Some(t) = s("secs_since_lock").as_u64() {
        println!("last lock {} ago", human(t));
    }
    if let Some(m) = v.get("triggers").and_then(|t| t.as_object()) {
        for (k, val) in m {
            println!("  {k:<8} {}", val.as_str().unwrap_or(""));
        }
    }
}

fn gen_token() -> Result<String> {
    let mut buf = [0u8; 16];
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn parse_duration(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        _ => (s, 1),
    };
    let n: u64 = num.parse().map_err(|_| anyhow::anyhow!("not a duration: {s:?}"))?;
    Ok(n * mult)
}

fn human(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        _ => format!("{}h{}m", secs / 3600, (secs % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90").unwrap(), 90);
        assert_eq!(parse_duration("90s").unwrap(), 90);
        assert_eq!(parse_duration("30m").unwrap(), 1800);
        assert_eq!(parse_duration("2h").unwrap(), 7200);
        assert!(parse_duration("soon").is_err());
    }

    #[test]
    fn tokens_are_hex_and_long_enough() {
        let t = gen_token().unwrap();
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
