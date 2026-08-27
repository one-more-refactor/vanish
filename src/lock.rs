//! Actually locking the session.
//!
//! The default path is logind's `lock-session`, not a locker binary, because
//! this machine already has exactly one locker and a well-understood way of
//! reaching it: `loginctl lock-session` raises systemd's Lock signal, hypridle
//! catches it and runs `pidof hyprlock || hyprlock`. Going through that means
//!
//!   * the idle path, the suspend path and this one all end in the same place;
//!   * `pidof hyprlock ||` makes a second lock a no-op, so vanish never has to
//!     track whether the screen is already locked;
//!   * nothing here has to know which locker is installed.
//!
//! `lock_command` overrides it for setups that run their locker directly.

use std::process::Stdio;

use tokio::process::Command;
use tracing::{info, warn};

/// Run the lock. `dry` logs what would have happened and returns.
pub async fn lock(command: &str, dry: bool) {
    if !command.is_empty() {
        if dry {
            info!(%command, "DRY RUN: would run lock command");
            return;
        }
        info!(%command, "locking (configured command)");
        let _ = Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .status()
            .await;
        return;
    }

    // Ask the seat which session is on screen. Deliberately not a bare
    // `loginctl lock-session`: with no argument it targets the caller's own
    // session, and a user service lives in the manager session, which has no
    // seat and cannot be locked. That call exits 0 and leaves the desktop open
    // — a lock that reports success and does nothing.
    let Some(id) = active_session().await else {
        warn!("no active session on seat0; not locking");
        return;
    };

    if dry {
        info!(session = %id, "DRY RUN: would lock session");
        return;
    }

    info!(session = %id, "locking");
    match Command::new("loginctl")
        .args(["lock-session", &id])
        .stdin(Stdio::null())
        .status()
        .await
    {
        Ok(s) if s.success() => {}
        Ok(s) => warn!(code = ?s.code(), "loginctl lock-session failed"),
        Err(e) => warn!(%e, "could not run loginctl"),
    }
}

async fn active_session() -> Option<String> {
    let out = Command::new("loginctl")
        .args(["show-seat", "seat0", "-p", "ActiveSession", "--value"])
        .output()
        .await
        .ok()?;
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!id.is_empty()).then_some(id)
}
