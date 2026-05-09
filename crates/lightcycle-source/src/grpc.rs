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
use lightcycle_proto::tron::api::wallet_solidity_client::WalletSolidityClient;
use lightcycle_proto::tron::protocol::{Block, EmptyMessage, NumberMessage, TransactionInfoList};
use lightcycle_types::{Address, BlockHeight, SrSet};
use tonic::transport::Channel;

/// Cheap clonable handle to a connected gRPC client.
///
/// Holds two service stubs that share a single underlying `Channel`:
/// `Wallet` for the live head + tx-info path, and `WalletSolidity` for
/// the chain's authoritative finality reading. We do not compute
/// finality from a confirmation count — finality is read from the
/// chain via `WalletSolidity.GetNowBlock` per ADR-0021's "the chain
/// has a finality machine; lean on it" discipline.
#[derive(Debug, Clone)]
pub struct GrpcSource {
    client: WalletClient<Channel>,
    solidity_client: WalletSolidityClient<Channel>,
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
            client: WalletClient::new(channel.clone()),
            solidity_client: WalletSolidityClient::new(channel),
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

    /// Fetch the chain's current solidified-head height. Reads from
    /// `WalletSolidity.GetNowBlock` — java-tron's solidity service
    /// returns the highest block past SR-supermajority confirmation
    /// (~19 of 27 SRs, ~57s under healthy operation). This is the
    /// chain's own finality machine; we never recompute it.
    ///
    /// Returns `None` only when the upstream node has produced zero
    /// solidified blocks (cold start, or a freshly-initialized chain).
    /// Open in BOTH FullNode and LiteFullNode modes.
    ///
    /// **Per ADR-0021 §2:** the value returned here is the only legal
    /// consistency source for cross-replica or cross-region agreement
    /// in lightcycle. Any caller answering "are these two replicas in
    /// agreement?" must compare against this number, not against a
    /// locally-computed confirmation count.
    pub async fn fetch_solidified_head(&mut self) -> Result<Option<BlockHeight>> {
        let resp = self
            .solidity_client
            .get_now_block(EmptyMessage {})
            .await
            .context("walletsolidity.get_now_block rpc failed")?
            .into_inner();
        let Some(header) = resp.block_header else {
            return Ok(None);
        };
        let Some(raw) = header.raw_data else {
            return Ok(None);
        };
        if raw.number < 0 {
            anyhow::bail!(
                "solidified head returned negative number ({}); chain-level bug",
                raw.number,
            );
        }
        Ok(Some(raw.number as BlockHeight))
    }

    /// Fetch the `TransactionInfo` side channel for every tx in a
    /// block. Returns the prost `TransactionInfoList` message verbatim;
    /// the relayer joins these against the block's transactions by
    /// tx-id and feeds the result through the codec.
    ///
    /// Open in BOTH FullNode and LiteFullNode modes for any block
    /// java-tron currently has in memory (recent head + the lite
    /// node's retained range). Empty list when the block has no txs
    /// or the height is beyond what the node retained.
    pub async fn fetch_transaction_info_by_block_num(
        &mut self,
        height: u64,
    ) -> Result<TransactionInfoList> {
        let req = NumberMessage {
            num: i64::try_from(height).context("height exceeds i64::MAX")?,
        };
        let resp = self
            .client
            .get_transaction_info_by_block_num(req)
            .await
            .context("get_transaction_info_by_block_num rpc failed")?;
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
