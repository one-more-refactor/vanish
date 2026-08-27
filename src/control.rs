//! The control socket.
//!
//! A daemon that can lock your screen needs a way to be told "not for the next
//! half hour" that is faster than editing a config file — during a
//! presentation, while something is rendering, when the headset is charging on
//! the other side of the desk. `systemctl --user stop` would do it, but it is
//! also how you forget to turn the thing back on.
//!
//! One line of JSON in, one line of JSON out, on a socket in the runtime dir
//! that only your uid can reach.

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use crate::arbiter::Signal;
use crate::config::runtime_dir;

pub fn socket_path() -> PathBuf {
    runtime_dir().join("vanish.sock")
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    Status,
    Pause { secs: u64 },
    Resume,
    Lock,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response {
    Status(crate::arbiter::Status),
    Ok { ok: bool },
}

pub async fn run(tx: mpsc::Sender<Signal>) -> Result<()> {
    let path = socket_path();
    // A leftover socket from a killed daemon would make bind fail. Removing it
    // is safe: the runtime dir is ours alone, and a live daemon holding this
    // path means our own bind is the mistake, which the next line reports.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;

    loop {
        let (sock, _) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!(%e, "control accept failed");
                continue;
            }
        };
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = serve(sock, tx).await {
                warn!(%e, "control request failed");
            }
        });
    }
}

async fn serve(sock: UnixStream, tx: mpsc::Sender<Signal>) -> Result<()> {
    let mut reader = BufReader::new(sock);
    let mut line = String::new();
    reader.read_line(&mut line).await?;

    let resp = match serde_json::from_str::<Request>(line.trim()) {
        Ok(Request::Status) => {
            let (rtx, rrx) = oneshot::channel();
            tx.send(Signal::Status(rtx)).await?;
            Response::Status(rrx.await?)
        }
        Ok(Request::Pause { secs }) => {
            tx.send(Signal::Pause(std::time::Duration::from_secs(secs))).await?;
            Response::Ok { ok: true }
        }
        Ok(Request::Resume) => {
            tx.send(Signal::Resume).await?;
            Response::Ok { ok: true }
        }
        Ok(Request::Lock) => {
            tx.send(Signal::LockNow).await?;
            Response::Ok { ok: true }
        }
        Err(_) => Response::Ok { ok: false },
    };

    let mut out = serde_json::to_string(&resp)?;
    out.push('\n');
    let mut sock = reader.into_inner();
    sock.write_all(out.as_bytes()).await?;
    sock.flush().await?;
    Ok(())
}

/// Client side, used by every subcommand that is not `run`.
pub async fn ask(req: Request) -> Result<serde_json::Value> {
    let path = socket_path();
    let sock = UnixStream::connect(&path).await.map_err(|e| {
        anyhow::anyhow!("{}: {e} (is the daemon running? `systemctl --user status vanish`)", path.display())
    })?;
    let mut reader = BufReader::new(sock);
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    reader.get_mut().write_all(line.as_bytes()).await?;
    reader.get_mut().flush().await?;
    let mut resp = String::new();
    reader.read_line(&mut resp).await?;
    Ok(serde_json::from_str(resp.trim())?)
}
