//! gRPC source. Fetches full `Block` protobuf messages from java-tron's
//! `Wallet.GetBlockByNum` RPC. Pairs with the HTTP `HeadPoller` (which
//! covers head metadata only) to round out the RPC fallback story:
//!
//!   - HTTP `/wallet/getnowblock` for cheap head tracking
//!   - gRPC `Wallet.GetBlockByNum` for full block bytes
//!
//! Sits behind the `Source` trait conceptually but doesn't implement
//! it yet (the trait wants a continuous stream; we expose a one-shot
//! `fetch_block` for the `lightcycle inspect` debug subcommand and as
//! the building block for any future polling-streamer that wants real
//! block bytes via RPC). The streaming surface stays P2P's job.

use std::time::Duration;

use anyhow::{Context, Result};
use lightcycle_proto::tron::api::wallet_client::WalletClient;
use lightcycle_proto::tron::protocol::{Block, EmptyMessage, NumberMessage};
use lightcycle_types::{Address, SrSet};
use tonic::transport::Channel;

/// Cheap clonable handle to a connected gRPC client.
#[derive(Debug, Clone)]
pub struct GrpcSource {
    client: WalletClient<Channel>,
}

impl GrpcSource {
    /// Connect to java-tron's gRPC endpoint. URL like `http://127.0.0.1:50051`
    /// (no TLS — the kulen module binds gRPC to host loopback only). Connect
    /// timeout is 5s; per-call timeout 10s.
    pub async fn connect(url: String) -> Result<Self> {
        let channel = tonic::transport::Endpoint::from_shared(url.clone())
            .with_context(|| format!("invalid gRPC endpoint: {url}"))?
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .connect()
            .await
            .with_context(|| format!("gRPC connect failed: {url}"))?;
        Ok(Self {
            client: WalletClient::new(channel),
        })
    }

    /// Fetch the block at `height` as a typed `Block` message.
    ///
    /// **Closed in LiteFullNode mode.** java-tron with `node.lite-fullnode =
    /// true` (our default per ADR 0012) returns gRPC status `UNAVAILABLE`
    /// with the message "this API is closed because this node is a lite
    /// fullnode" for ALL `get_block_by_num` calls, regardless of whether
    /// the height is within the lite node's retained range. If you need
    /// historical block lookup, point at a non-lite full node (e.g.
    /// TronGrid's public endpoint) or land the P2P source per
    /// SCAFFOLD.md priority 3.
    pub async fn fetch_block(&mut self, height: u64) -> Result<Block> {
        let req = NumberMessage {
            num: i64::try_from(height).context("height exceeds i64::MAX (decades from now)")?,
        };
        let resp = self
            .client
            .get_block_by_num(req)
            .await
            .context("get_block_by_num rpc failed (closed in lite-fullnode mode)")?;
        Ok(resp.into_inner())
    }

    /// Fetch the current chain head block. Open in BOTH FullNode AND
    /// LiteFullNode modes (java-tron always exposes the tip). For
    /// `inspect`-style debug use this is the workhorse; historical
    /// `fetch_block` only works against full-history endpoints.
    pub async fn fetch_now_block(&mut self) -> Result<Block> {
        // get_now_block takes an EmptyMessage (a zero-field protobuf
        // message — distinct from the implicit `()` placeholder some
        // gRPC bindings use). prost generates the type but with no
        // fields; default-constructing it is the right call here.
        let resp = self
            .client
            .get_now_block(EmptyMessage {})
            .await
            .context("get_now_block rpc failed")?;
        Ok(resp.into_inner())
    }

    /// Fetch the current active SR set from the upstream node.
    /// `Wallet.ListWitnesses` returns the full configured witness list
    /// (active 27 + standby SRPs); we filter by the `is_jobs` flag —
    /// java-tron sets it `true` for witnesses currently in the
    /// block-producing rotation. This is what `verify_witness_signature`
    /// expects.
    ///
    /// Refreshes are needed across maintenance-period transitions
    /// (every 7,200 blocks ≈ 6 hours). Caller decides cadence; this
    /// method is a single round-trip and skips any caching.
    pub async fn fetch_active_sr_set(&mut self) -> Result<SrSet> {
        let resp = self
            .client
            .list_witnesses(EmptyMessage {})
            .await
            .context("list_witnesses rpc failed")?
            .into_inner();

        let mut members = Vec::new();
        for w in resp.witnesses {
            if !w.is_jobs {
                continue;
            }
            if w.address.len() != 21 {
                anyhow::bail!(
                    "witness address has wrong length: got {} bytes, expected 21",
                    w.address.len()
                );
            }
            let mut a = [0u8; 21];
            a.copy_from_slice(&w.address);
            members.push(Address(a));
        }
        Ok(SrSet::new(members))
    }
}
