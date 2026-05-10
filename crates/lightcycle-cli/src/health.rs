//! Tiny HTTP/1.1 health server: `/healthz` (process up) and `/readyz`
//! (relayer is steady-state). Bound separately from the Prometheus
//! exporter so a single misconfigured scrape can't black-hole the
//! readiness signal.
//!
//! ## Why a hand-rolled server
//!
//! The kubelet readiness probe sends `GET /readyz HTTP/1.0\r\n\r\n` and
//! reads the status line. That's the entire protocol surface we need.
//! Pulling in `hyper` or `axum` for two endpoints would balloon the
//! transitive deps for ~100 lines of value. The handler reads exactly
//! the request line, drops the rest, and writes a fixed response —
//! correct enough for any sane probe and trivial to audit.
//!
//! ## Readiness semantics
//!
//! `/readyz` returns 200 iff the relayer has populated the
//! `solidified_head` watch with at least one chain-reported height.
//! That signal subsumes "have we connected to the upstream RPC,"
//! "have we successfully fetched at least one block," and "is the
//! finality oracle wired" — all three become true together on the
//! first successful tick. Returns 503 with a short body until then.
//!
//! `/healthz` always returns 200 — the only way to reach it is for
//! the listener to be bound, which means the process is alive.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{debug, warn};

use lightcycle_types::BlockHeight;

/// Bind a healthcheck server on `listen` until `shutdown` resolves.
///
/// `head_rx` is the chain-reported solidified-head watch — the relayer
/// broadcasts on every successful `WalletSolidity.GetNowBlock` tick.
/// `/readyz` returns 200 once it sees `Some(_)`, 503 before then.
pub(crate) async fn serve(
    listen: SocketAddr,
    head_rx: watch::Receiver<Option<BlockHeight>>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind health server on {listen}"))?;
    tracing::info!(%listen, "health server listening");
    metrics::counter!("lightcycle_health_server_starts_total").increment(1);

    let head_rx = Arc::new(head_rx);
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                debug!("health server: shutdown received");
                return Ok(());
            }
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _peer)) => {
                        let head_rx = Arc::clone(&head_rx);
                        tokio::spawn(async move {
                            if let Err(e) = handle(stream, head_rx).await {
                                debug!(error = %e, "health request handler error");
                            }
                        });
                    }
                    Err(e) => {
                        warn!(error = %e, "health server accept failed; continuing");
                    }
                }
            }
        }
    }
}

async fn handle(
    mut stream: tokio::net::TcpStream,
    head_rx: Arc<watch::Receiver<Option<BlockHeight>>>,
) -> Result<()> {
    // Read enough to parse the request line. Bound the read so a
    // malicious slow-loris client can't tie up a task indefinitely.
    let mut buf = [0u8; 1024];
    let n = match tokio::time::timeout(
        std::time::Duration::from_millis(500),
        stream.read(&mut buf),
    )
    .await
    {
        Ok(Ok(n)) if n > 0 => n,
        _ => {
            // Empty or timed-out request; close quietly.
            return Ok(());
        }
    };
    let request = &buf[..n];
    // Match on the request line only. We don't validate the rest.
    let path = parse_path(request);

    let (status_line, body): (&str, &str) = match path {
        Some("/healthz") => ("HTTP/1.1 200 OK", "ok\n"),
        Some("/readyz") => {
            if head_rx.borrow().is_some() {
                ("HTTP/1.1 200 OK", "ready\n")
            } else {
                metrics::counter!("lightcycle_health_readyz_not_ready_total").increment(1);
                (
                    "HTTP/1.1 503 Service Unavailable",
                    "not ready: solidified head not yet observed\n",
                )
            }
        }
        _ => ("HTTP/1.1 404 Not Found", "not found\n"),
    };
    let response = format!(
        "{status_line}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len(),
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await.ok();
    Ok(())
}

/// Extract the URL path from a request like `GET /healthz HTTP/1.1\r\n…`.
/// Returns `None` for requests we can't parse — handler responds 404.
fn parse_path(req: &[u8]) -> Option<&str> {
    let line_end = req.iter().position(|&b| b == b'\r' || b == b'\n')?;
    let line = std::str::from_utf8(&req[..line_end]).ok()?;
    let mut parts = line.splitn(3, ' ');
    let method = parts.next()?;
    let path = parts.next()?;
    // Only GET / HEAD make sense for a probe; anything else gets 404.
    if method != "GET" && method != "HEAD" {
        return None;
    }
    // Strip query string if present.
    let path = path.split('?').next().unwrap_or(path);
    Some(path)
}

/// Describe the health-server metrics so they show up before the first
/// request.
pub(crate) fn describe_metrics() {
    metrics::describe_counter!(
        "lightcycle_health_server_starts_total",
        "Health-server start events (process restarts)."
    );
    metrics::describe_counter!(
        "lightcycle_health_readyz_not_ready_total",
        "Number of /readyz probes that returned 503 (relayer not yet ready)."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_path_extracts_get_path() {
        assert_eq!(
            parse_path(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n"),
            Some("/healthz")
        );
        assert_eq!(
            parse_path(b"GET /readyz?probe=k8s HTTP/1.0\r\n\r\n"),
            Some("/readyz")
        );
        assert_eq!(parse_path(b"HEAD /healthz HTTP/1.1\r\n\r\n"), Some("/healthz"));
    }

    #[test]
    fn parse_path_rejects_non_get_methods() {
        assert_eq!(parse_path(b"POST /healthz HTTP/1.1\r\n\r\n"), None);
        assert_eq!(parse_path(b"DELETE / HTTP/1.1\r\n\r\n"), None);
    }

    #[test]
    fn parse_path_handles_malformed_request() {
        assert_eq!(parse_path(b""), None);
        assert_eq!(parse_path(b"GET\r\n\r\n"), None);
        assert_eq!(parse_path(b"junk"), None);
    }

    #[tokio::test]
    async fn healthz_returns_200() {
        let (_tx, rx) = watch::channel::<Option<BlockHeight>>(None);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Manually accept and handle one connection.
        let (server_done_tx, mut server_done_rx) = tokio::sync::mpsc::channel::<()>(1);
        let rx_for_handler = Arc::new(rx);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle(stream, rx_for_handler).await.unwrap();
            let _ = server_done_tx.send(()).await;
        });

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /healthz HTTP/1.0\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "got: {response}");
        assert!(response.contains("ok"));
        let _ = server_done_rx.recv().await;
    }

    #[tokio::test]
    async fn readyz_returns_503_until_head_seen() {
        let (tx, rx) = watch::channel::<Option<BlockHeight>>(None);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let rx = Arc::new(rx);

        // 1. Before head: /readyz should be 503.
        {
            let rx_for_handler = Arc::clone(&rx);
            let handler = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                handle(stream, rx_for_handler).await.unwrap();
            });
            let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
            client
                .write_all(b"GET /readyz HTTP/1.0\r\n\r\n")
                .await
                .unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();
            let response = String::from_utf8(response).unwrap();
            assert!(
                response.starts_with("HTTP/1.1 503"),
                "expected 503, got: {response}"
            );
            handler.await.unwrap();
        }

        // 2. Send a head, re-bind on the same address (the listener was consumed).
        tx.send(Some(100)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let rx_for_handler = Arc::clone(&rx);
        let handler = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle(stream, rx_for_handler).await.unwrap();
        });
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /readyz HTTP/1.0\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(
            response.starts_with("HTTP/1.1 200 OK"),
            "expected 200 after head broadcast, got: {response}"
        );
        handler.await.unwrap();
    }

    #[tokio::test]
    async fn unknown_path_returns_404() {
        let (_tx, rx) = watch::channel::<Option<BlockHeight>>(None);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let rx_for_handler = Arc::new(rx);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle(stream, rx_for_handler).await.unwrap();
        });
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /unknown HTTP/1.0\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 404"));
    }
}
