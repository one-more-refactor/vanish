//! Inbound trigger for anything that can make an HTTP request.
//!
//! The point of this one is that presence detection nobody has written yet
//! still gets to lock the screen: a camera that stops seeing a face, a
//! motion sensor, a phone geofence, a Home Assistant automation, `curl` in a
//! script. vanish does not care what decided; it cares that something did.
//!
//! Hand-rolled HTTP/1.1, because the whole surface is one route with one
//! header, and a framework here would be more attack surface than protocol.
//!
//! Two rules it will not bend on:
//!   * loopback by default — an unauthenticated port that locks a workstation
//!     is a denial-of-service someone else gets to run;
//!   * one response for every failure. Wrong path, wrong token, wrong method:
//!     all 404, all identical, so the endpoint cannot be found by probing.

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::arbiter::{Signal, Source};
use crate::config::Webhook;

const MAX_HEAD: usize = 8 * 1024;
const MAX_BODY: usize = 1024;
const NOT_FOUND: &[u8] =
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const OK: &[u8] = b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

pub async fn run(cfg: Webhook, tx: mpsc::Sender<Signal>) -> Result<()> {
    if cfg.token.len() < 16 {
        warn!("webhook enabled but token is missing or too short; not listening (see `vanish gen-token`)");
        return Ok(());
    }
    let listener = TcpListener::bind(&cfg.bind).await?;
    info!(bind = %cfg.bind, path = %cfg.path, "webhook listening");

    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!(%e, "accept failed");
                continue;
            }
        };
        let cfg = cfg.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = serve(sock, &cfg, &tx).await {
                warn!(%e, peer = %peer, "webhook request failed");
            }
        });
    }
}

async fn serve(mut sock: TcpStream, cfg: &Webhook, tx: &mpsc::Sender<Signal>) -> Result<()> {
    // A request that never finishes its headers must not hold a task forever.
    let head = match tokio::time::timeout(std::time::Duration::from_secs(5), read_head(&mut sock)).await {
        Ok(Ok(h)) => h,
        _ => {
            let _ = sock.write_all(NOT_FOUND).await;
            return Ok(());
        }
    };

    let ok = validate(&head, cfg);
    if !ok {
        let _ = sock.write_all(NOT_FOUND).await;
        return Ok(());
    }

    let reason = read_reason(&mut sock, &head).await;
    let detail = match reason {
        Some(r) => format!("webhook: {r}"),
        None => "webhook".to_string(),
    };
    let _ = sock.write_all(OK).await;
    let _ = sock.flush().await;

    let _ = tx
        .send(Signal::Away { source: Source::Webhook, detail, grace: cfg.grace_secs })
        .await;
    Ok(())
}

async fn read_head(sock: &mut TcpStream) -> Result<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut byte = [0u8; 512];
    loop {
        let n = sock.read(&mut byte).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&byte[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > MAX_HEAD {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).to_string())
}

/// Method, path and token, all three or nothing.
fn validate(head: &str, cfg: &Webhook) -> bool {
    let mut lines = head.lines();
    let Some(req) = lines.next() else { return false };
    let mut parts = req.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();

    // GET is refused on purpose: a lock is a state change, and a GET endpoint
    // gets fetched by link previews, prefetchers and browser history sync.
    if method != "POST" {
        return false;
    }
    // Query strings are allowed but ignored — a token in a URL ends up in logs.
    let path = path.split('?').next().unwrap_or_default();
    if path != cfg.path {
        return false;
    }

    let mut token = None;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let k = k.trim().to_ascii_lowercase();
        let v = v.trim();
        if k == "authorization" {
            token = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")).map(str::to_string);
        } else if k == "x-vanish-token" {
            token = Some(v.to_string());
        }
    }
    token.is_some_and(|t| constant_time_eq(t.as_bytes(), cfg.token.as_bytes()))
}

/// An optional `{"reason": "..."}` body, purely so the journal says which
/// sensor fired. Anything unparseable is simply no reason.
async fn read_reason(sock: &mut TcpStream, head: &str) -> Option<String> {
    let len: usize = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case("content-length").then(|| v.trim().parse().ok())?
        })
        .unwrap_or(0);
    if len == 0 || len > MAX_BODY {
        return None;
    }
    // Whatever arrived with the head is already gone from the socket, so this
    // only works for bodies that came in a later packet. Small bodies usually
    // ride along with the head; try there first.
    if let Some(idx) = head.find("\r\n\r\n") {
        let inline = &head[idx + 4..];
        if !inline.is_empty() {
            return reason_of(inline);
        }
    }
    let mut body = vec![0u8; len];
    let read = tokio::time::timeout(std::time::Duration::from_secs(2), sock.read_exact(&mut body))
        .await
        .ok()?
        .ok()?;
    let _ = read;
    reason_of(&String::from_utf8_lossy(&body))
}

fn reason_of(s: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(s.trim()).ok()?;
    let r = v.get("reason")?.as_str()?;
    Some(r.chars().filter(|c| !c.is_control()).take(120).collect())
}

/// Not for side-channel resistance over the network — for the habit. Comparing
/// secrets with `==` is the kind of thing that is right until the code moves.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Webhook {
        Webhook {
            enabled: true,
            bind: "127.0.0.1:0".into(),
            token: "0123456789abcdef0123456789abcdef".into(),
            path: "/lock".into(),
            grace_secs: Some(0),
        }
    }

    fn head(method: &str, path: &str, auth: &str) -> String {
        format!("{method} {path} HTTP/1.1\r\nHost: x\r\nAuthorization: {auth}\r\n\r\n")
    }

    #[test]
    fn accepts_a_correct_request() {
        assert!(validate(&head("POST", "/lock", "Bearer 0123456789abcdef0123456789abcdef"), &cfg()));
        assert!(validate(&head("POST", "/lock?src=cam", "Bearer 0123456789abcdef0123456789abcdef"), &cfg()));
    }

    #[test]
    fn refuses_everything_else() {
        let c = cfg();
        assert!(!validate(&head("GET", "/lock", "Bearer 0123456789abcdef0123456789abcdef"), &c));
        assert!(!validate(&head("POST", "/", "Bearer 0123456789abcdef0123456789abcdef"), &c));
        assert!(!validate(&head("POST", "/lock", "Bearer wrong"), &c));
        assert!(!validate(&head("POST", "/lock", ""), &c));
        assert!(!validate("garbage", &c));
    }

    #[test]
    fn reads_a_reason() {
        assert_eq!(reason_of(r#"{"reason":"no face for 30s"}"#).as_deref(), Some("no face for 30s"));
        assert_eq!(reason_of("not json"), None);
    }
}
