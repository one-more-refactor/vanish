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
    let len = content_length(head);
    if len == 0 || len > MAX_BODY {
        return None;
    }
    // `read_head` reads in chunks, so some or all of the body may already have
    // come off the socket with the headers. Start from that and read only what
    // is still missing — a body split across packets used to be cut off at the
    // packet boundary and then fail to parse.
    let mut body: Vec<u8> = head
        .find("\r\n\r\n")
        .map(|i| head.as_bytes()[i + 4..].to_vec())
        .unwrap_or_default();
    body.truncate(len);
    if body.len() < len {
        let mut rest = vec![0u8; len - body.len()];
        tokio::time::timeout(std::time::Duration::from_secs(2), sock.read_exact(&mut rest))
            .await
            .ok()?
            .ok()?;
        body.extend_from_slice(&rest);
    }
    reason_of(&String::from_utf8_lossy(&body))
}

fn content_length(head: &str) -> usize {
    let headers = head.split("\r\n\r\n").next().unwrap_or_default();
    headers
        .lines()
        .skip(1)
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case("content-length").then(|| v.trim().parse().ok())?
        })
        .unwrap_or(0)
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
    fn accepts_the_alternative_token_header() {
        let h = "POST /lock HTTP/1.1\r\nX-Vanish-Token: 0123456789abcdef0123456789abcdef\r\n\r\n";
        assert!(validate(h, &cfg()));
    }

    #[test]
    fn token_must_match_exactly() {
        let c = cfg();
        // A prefix of the right token, and the right token plus one byte.
        assert!(!validate(&head("POST", "/lock", "Bearer 0123456789abcdef"), &c));
        assert!(!validate(&head("POST", "/lock", "Bearer 0123456789abcdef0123456789abcdef0"), &c));
        // Case matters: it is a secret, not a word.
        assert!(!validate(&head("POST", "/lock", "Bearer 0123456789ABCDEF0123456789ABCDEF"), &c));
        // Basic auth carrying the token is not bearer auth.
        assert!(!validate(&head("POST", "/lock", "Basic 0123456789abcdef0123456789abcdef"), &c));
    }

    #[test]
    fn constant_time_eq_is_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn content_length_is_read_from_the_headers_only() {
        assert_eq!(content_length("POST / HTTP/1.1\r\nContent-Length: 12\r\n\r\n"), 12);
        assert_eq!(content_length("POST / HTTP/1.1\r\ncontent-length:7\r\n\r\n"), 7);
        assert_eq!(content_length("POST / HTTP/1.1\r\n\r\n"), 0);
        // A body line that looks like a header is still the body.
        assert_eq!(content_length("POST / HTTP/1.1\r\n\r\nContent-Length: 99"), 0);
    }

    #[test]
    fn reasons_are_sanitised_and_capped() {
        assert_eq!(reason_of(r#"{"reason":"a\nb\u0007c"}"#).as_deref(), Some("abc"));
        let long = format!(r#"{{"reason":"{}"}}"#, "x".repeat(500));
        assert_eq!(reason_of(&long).map(|r| r.len()), Some(120));
        assert_eq!(reason_of(r#"{"reason":42}"#), None);
    }

    // ── Over a real socket ──────────────────────────────────────────────

    use std::time::Duration;
    use tokio::io::AsyncReadExt as _;

    /// Send `parts` (with a pause between them, so they arrive as separate
    /// reads), return the raw response and whatever reached the arbiter.
    async fn roundtrip(parts: Vec<String>) -> (String, Option<Signal>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = mpsc::channel(4);
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let _ = serve(sock, &cfg(), &tx).await;
        });
        let mut c = TcpStream::connect(addr).await.unwrap();
        for p in parts {
            let _ = c.write_all(p.as_bytes()).await;
            let _ = c.flush().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let mut resp = String::new();
        let _ = c.read_to_string(&mut resp).await;
        server.await.unwrap();
        (resp, rx.try_recv().ok())
    }

    fn request(auth: &str, body: &str) -> String {
        format!(
            "POST /lock HTTP/1.1\r\nHost: x\r\nAuthorization: {auth}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    fn detail_of(sig: Option<Signal>) -> Option<String> {
        match sig {
            Some(Signal::Away { source: Source::Webhook, detail, grace: Some(0) }) => Some(detail),
            _ => None,
        }
    }

    #[tokio::test]
    async fn a_correct_post_locks_with_its_reason() {
        let (resp, sig) =
            roundtrip(vec![request("Bearer 0123456789abcdef0123456789abcdef", r#"{"reason":"no face"}"#)]).await;
        assert!(resp.starts_with("HTTP/1.1 204"), "{resp}");
        assert_eq!(detail_of(sig).as_deref(), Some("webhook: no face"));
    }

    #[tokio::test]
    async fn a_body_in_a_later_packet_is_read_whole() {
        let body = r#"{"reason":"motion sensor, hallway"}"#;
        let full = request("Bearer 0123456789abcdef0123456789abcdef", body);
        let split = full.len() - body.len() + 5; // headers plus a sliver of the body
        let (resp, sig) = roundtrip(vec![full[..split].to_string(), full[split..].to_string()]).await;
        assert!(resp.starts_with("HTTP/1.1 204"), "{resp}");
        assert_eq!(detail_of(sig).as_deref(), Some("webhook: motion sensor, hallway"));
    }

    #[tokio::test]
    async fn no_body_is_fine() {
        let (resp, sig) = roundtrip(vec![request("Bearer 0123456789abcdef0123456789abcdef", "")]).await;
        assert!(resp.starts_with("HTTP/1.1 204"), "{resp}");
        assert_eq!(detail_of(sig).as_deref(), Some("webhook"));
    }

    #[tokio::test]
    async fn every_failure_is_the_same_404_and_locks_nothing() {
        let wrong_token = request("Bearer ffffffffffffffffffffffffffffffff", "");
        let get = "GET /lock HTTP/1.1\r\nAuthorization: Bearer 0123456789abcdef0123456789abcdef\r\n\r\n".to_string();
        let wrong_path = "POST /unlock HTTP/1.1\r\nAuthorization: Bearer 0123456789abcdef0123456789abcdef\r\n\r\n".to_string();
        let mut seen = Vec::new();
        for req in [wrong_token, get, wrong_path, "garbage\r\n\r\n".to_string()] {
            let (resp, sig) = roundtrip(vec![req]).await;
            assert!(sig.is_none());
            seen.push(resp);
        }
        assert!(seen.iter().all(|r| r.as_bytes() == NOT_FOUND), "{seen:?}");
    }

    #[test]
    fn reads_a_reason() {
        assert_eq!(reason_of(r#"{"reason":"no face for 30s"}"#).as_deref(), Some("no face for 30s"));
        assert_eq!(reason_of("not json"), None);
    }
}
