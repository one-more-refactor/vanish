//! Courtesy messages to notchd.
//!
//! The notch already knows how to count down to a lock — hypridle uses the
//! same two messages ten seconds before the idle lock. Reusing them means the
//! grace window is visible on screen and looks like every other lock on this
//! machine, instead of the desktop vanishing with no warning.
//!
//! Every failure here is swallowed on purpose. The countdown is a courtesy and
//! must never be something the lock waits on, or depends on being there.

use std::path::PathBuf;

use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use crate::config::runtime_dir;

fn sock() -> PathBuf {
    runtime_dir().join("notch.sock")
}

async fn send(line: &str) {
    let Ok(mut s) = UnixStream::connect(sock()).await else { return };
    let _ = s.write_all(line.as_bytes()).await;
    let _ = s.flush().await;
}

pub async fn lock_soon(seconds: u64) {
    if seconds == 0 {
        return;
    }
    send(&format!("{{\"t\":\"lock_soon\",\"seconds\":{seconds}}}\n")).await;
}

pub async fn cancel() {
    send("{\"t\":\"lock_soon_cancel\"}\n").await;
}
