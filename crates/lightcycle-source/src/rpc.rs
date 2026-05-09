//! RPC fallback. Polls java-tron's HTTP `/wallet/getnowblock` for the
//! current head; emits Prometheus metrics for the comparison dashboard
//! kulen-side. Higher latency than P2P; far simpler operationally.
//!
//! ## What this module is and isn't
//!
//! IS: a `HeadPoller` that runs forever, fetching the chain head every
//! `interval`, parsing the JSON response, updating a `tokio::sync::watch`
//! channel that consumers can read at any time, and emitting metrics
//! that match the surface java-tron's own Prometheus exporter
//! exposes (`lightcycle_header_height` mirrors `tron:header_height`,
//! `lightcycle_header_time_seconds` mirrors `tron:header_time / 1000`).
//!
//! ISN'T: a streaming source. The full `Source` trait wants raw
//! protobuf block bytes; HTTP `/wallet/getblockbynum` returns JSON, and
//! reconstructing a wire `Block` from java-tron's TRON-flavored JSON
//! (hex-encoded addresses, decimal-string numbers, optional fields)
//! is more work than vendoring `google/api/annotations.proto` and
//! using the gRPC `Wallet.GetBlockByNum` directly. Deferred until the
//! gRPC vendoring lands or P2P arrives.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::watch;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, warn};

/// What the head poller knows about the chain tip on the upstream node.
/// Hex-string fields preserve the canonical TRON wire encoding so this
/// type is also serializable to JSON for ad-hoc debug surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HeadInfo {
    /// Block number (height).
    pub height: u64,
    /// Hex-encoded `blockID` — 32 bytes, height-prefixed in the high 4 bytes.
    pub block_id_hex: String,
    /// Hex-encoded parent block id.
    pub parent_id_hex: String,
    /// Block timestamp in milliseconds since epoch.
    pub timestamp_ms: i64,
    /// Hex-encoded 21-byte witness (super representative) address.
    pub witness_address_hex: String,
    /// Block protocol version. Tracks java-tron upgrade transitions.
    pub version: i32,
}

/// Long-running task that polls `/wallet/getnowblock` and surfaces the
/// latest head via a `tokio::sync::watch` channel + Prometheus metrics.
#[derive(Debug)]
pub struct HeadPoller {
    rpc_url: String,
    interval: Duration,
    client: Client,
    tx: watch::Sender<Option<HeadInfo>>,
}

impl HeadPoller {
    /// Build a poller. Returns `(poller, receiver)`; spawn the poller's
    /// `run()` on a Tokio task and read the `Receiver` from elsewhere.
    pub fn new(
        rpc_url: String,
        poll_interval: Duration,
    ) -> (Self, watch::Receiver<Option<HeadInfo>>) {
        let (tx, rx) = watch::channel(None);
        // Reqwest's default timeout is unbounded; the rpc poll path
        // should fail fast (we'll retry on the next tick anyway).
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("lightcycle/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client builder cannot fail in this configuration");
        (
            Self {
                rpc_url,
                interval: poll_interval,
                client,
                tx,
            },
            rx,
        )
    }

    /// Run forever. Returns only on a panic from the underlying tokio
    /// runtime; the caller is expected to spawn this on a dedicated
    /// task and treat unexpected return as fatal.
    pub async fn run(self) {
        let mut ticker = interval(self.interval);
        // Skip ticks that piled up while a slow poll was in flight; we
        // only ever want the latest, not a history of polling intent.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;

            match self.poll_once().await {
                Ok(info) => {
                    metrics::gauge!("lightcycle_header_height").set(info.height as f64);
                    metrics::gauge!("lightcycle_header_time_seconds")
                        .set(info.timestamp_ms as f64 / 1000.0);
                    metrics::counter!(
                        "lightcycle_rpc_polls_total",
                        "result" => "success",
                    )
                    .increment(1);
                    debug!(
                        height = info.height,
                        block_id = %&info.block_id_hex[..16.min(info.block_id_hex.len())],
                        "head update"
                    );
                    // Send is best-effort; even if no consumer is currently
                    // attached, the watch channel retains the latest value.
                    let _ = self.tx.send(Some(info));
                }
                Err(e) => {
                    metrics::counter!(
                        "lightcycle_rpc_polls_total",
                        "result" => "error",
                    )
                    .increment(1);
                    warn!(error = %e, "head poll failed");
                }
            }
        }
    }

    async fn poll_once(&self) -> Result<HeadInfo> {
        let start = Instant::now();
        let url = format!("{}/wallet/getnowblock", self.rpc_url.trim_end_matches('/'));

        // POST with empty JSON body — `getnowblock` accepts no parameters
        // but the server's HTTP handler is POST-only.
        let resp = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .context("rpc request failed")?;
        let status = resp.status();
        let body = resp.bytes().await.context("read response body")?;

        let elapsed = start.elapsed();
        metrics::histogram!("lightcycle_rpc_poll_duration_seconds").record(elapsed.as_secs_f64());

        if !status.is_success() {
            anyhow::bail!(
                "rpc returned {}: {}",
                status,
                String::from_utf8_lossy(&body)
            );
        }

        let parsed: NowBlockResp =
            serde_json::from_slice(&body).context("deserialize getnowblock response")?;

        let raw = &parsed.block_header.raw_data;
        Ok(HeadInfo {
            height: u64::try_from(raw.number.unwrap_or(0)).unwrap_or(0),
            block_id_hex: parsed.block_id,
            parent_id_hex: raw.parent_hash.clone().unwrap_or_default(),
            timestamp_ms: raw.timestamp.unwrap_or(0),
            witness_address_hex: raw.witness_address.clone().unwrap_or_default(),
            version: raw.version.unwrap_or(0),
        })
    }
}

/// Describe metrics so they appear in the Prometheus output even before
/// they're first observed. Call once at startup. Doing this upfront
/// means the comparison dashboard panels show a value of 0 at startup
/// rather than "N/A" until the first poll.
pub fn describe_metrics() {
    metrics::describe_gauge!(
        "lightcycle_header_height",
        "current chain head height as observed by the RPC head poller"
    );
    metrics::describe_gauge!(
        "lightcycle_header_time_seconds",
        "wall-clock timestamp of the current chain head (seconds since epoch)"
    );
    metrics::describe_counter!(
        "lightcycle_rpc_polls_total",
        "total RPC head polls, labelled by result=success|error"
    );
    metrics::describe_histogram!(
        "lightcycle_rpc_poll_duration_seconds",
        "round-trip duration of a single RPC head poll"
    );
}

// --- JSON shape from java-tron's /wallet/getnowblock --------------
//
// Documented at https://developers.tron.network/reference/walletgetnowblock
// Example response (abridged):
//
//   { "blockID": "0000000004…",
//     "block_header": {
//       "raw_data": {
//         "number": 82500000,
//         "txTrieRoot": "abcd…",
//         "witness_address": "4167e3…",
//         "parentHash": "0000000004…",
//         "version": 34,
//         "timestamp": 1777854558000
//       },
//       "witness_signature": "…"
//     },
//     "transactions": [ … ] }
//
// `Option`-wrapped scalars cover blocks that elide a default field
// (e.g. genesis-adjacent blocks that lack a parent).

#[derive(Debug, Deserialize)]
struct NowBlockResp {
    #[serde(rename = "blockID")]
    block_id: String,
    block_header: BlockHeaderJson,
}

#[derive(Debug, Deserialize)]
struct BlockHeaderJson {
    raw_data: BlockHeaderRawJson,
    #[serde(default)]
    witness_signature: String,
}

#[derive(Debug, Default, Deserialize)]
struct BlockHeaderRawJson {
    #[serde(default)]
    number: Option<i64>,
    #[serde(rename = "parentHash", default)]
    parent_hash: Option<String>,
    #[serde(default)]
    timestamp: Option<i64>,
    #[serde(default)]
    witness_address: Option<String>,
    #[serde(default)]
    version: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The single shape contract we depend on: java-tron returns
    /// `{blockID, block_header: {raw_data: {number, parentHash,
    /// timestamp, witness_address, version}, witness_signature}}`.
    #[test]
    fn parses_canonical_response() {
        let body = br#"{
          "blockID": "0000000004e9414c6f6ee7abcfae16a6a720a6bdb14e9c0ddbf856e05d1fa369",
          "block_header": {
            "raw_data": {
              "number": 82395468,
              "txTrieRoot": "47dc003b50e6ec6cd53a680081a4e282b2ef1a3fb76095e41f78cbb81d92db86",
              "witness_address": "4167e39013be3cdd3814bed152d7439fb5b6791409",
              "parentHash": "0000000004e9414b555d7307c02a9239e5e4772a1b2e79e397638f2158728529",
              "version": 34,
              "timestamp": 1777854558000
            },
            "witness_signature": "deadbeef"
          },
          "transactions": []
        }"#;
        let parsed: NowBlockResp = serde_json::from_slice(body).unwrap();
        assert_eq!(parsed.block_id.len(), 64);
        assert_eq!(parsed.block_header.raw_data.number, Some(82_395_468));
        assert_eq!(
            parsed.block_header.raw_data.timestamp,
            Some(1_777_854_558_000)
        );
        assert_eq!(parsed.block_header.raw_data.version, Some(34));
    }

    /// Pre-genesis-ish blocks may elide some fields; we should still
    /// parse without panicking.
    #[test]
    fn tolerates_minimal_response() {
        let body = br#"{
          "blockID": "00",
          "block_header": {
            "raw_data": {}
          }
        }"#;
        let parsed: NowBlockResp = serde_json::from_slice(body).unwrap();
        assert_eq!(parsed.block_header.raw_data.number, None);
        assert_eq!(parsed.block_header.raw_data.parent_hash, None);
    }
}
