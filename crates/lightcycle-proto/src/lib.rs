//! Generated protobuf types for the TRON wire format and the Firehose `bstream.v2` protocol.
//!
//! Compiled at build time by `build.rs` from the vendored `.proto` files under `proto/`.
//! Re-vendor with `scripts/sync-protos.sh`; pinned upstream commit SHAs live in
//! `proto/README.md`.
//!
//! ## Module layout
//!
//! - [`tron`] — TRON wire format. The bulk lives under
//!   `tron::protocol::*` (java-tron's monolithic `Tron.proto`); contract
//!   types live under their own packages (`tron::contract::*`); the
//!   wallet RPC stubs live under `tron::api::*` as gRPC client types only.
//!
//! - [`firehose`] — `sf.firehose.v2`'s `Stream` / `Fetch` / `EndpointInfo`
//!   services. Includes both server and client stubs.
//!
//! - [`sf`] — lightcycle-authored payload types ([`sf::tron::type_v1`]).
//!   Chain-specific block shape that ships on Firehose `Response.block`.
//!   Pure message types; no services.
//!
//! The codegen output files are named after the `package` declared in each
//! `.proto`, so the `include_proto!` calls below mirror those packages
//! one-to-one.

// Allow the wide net of warnings that prost/tonic-generated code triggers
// on stable. These are NOT in our hand-written code; the lints we care
// about apply elsewhere via workspace-level config.
#![allow(
    clippy::pedantic,
    clippy::nursery,
    clippy::all,
    missing_debug_implementations,
    unreachable_pub,
    elided_lifetimes_in_paths
)]

pub mod tron {
    //! TRON wire format. Generated from `proto/tron/`.

    /// java-tron's monolithic schema (`core/Tron.proto`, package `protocol`):
    /// blocks, transactions, account state, witness/SR types, common envelopes.
    pub mod protocol {
        tonic::include_proto!("protocol");
    }

    /// Contract types (`core/contract/*.proto`, package `protocol`).
    /// Same package as the monolithic schema upstream — already covered by
    /// `protocol` above; this re-export is here as a place to add helpers.
    pub mod contract {
        // Contract messages live in the same `protocol` package as Tron.proto's
        // top-level types. Re-export the shared module to keep call sites
        // ergonomic: `tron::contract::TransferContract`.
        pub use super::protocol::*;
    }

    /// java-tron's `Wallet` and `WalletSolidity` gRPC client stubs from
    /// `api/api.proto`. Used by `lightcycle-source::rpc::grpc` to fetch
    /// full block bytes via `WalletClient::get_block_by_num`. Also covers
    /// the `WalletExtension` and `Database` services java-tron exposes
    /// on the same channel.
    ///
    /// Usage:
    ///
    /// ```ignore
    /// use lightcycle_proto::tron::api::wallet_client::WalletClient;
    /// let mut client = WalletClient::connect("http://127.0.0.1:50051").await?;
    /// let resp = client.get_block_by_num(NumberMessage { num: 82_500_000 }).await?;
    /// ```
    pub mod api {
        // api.proto's `package protocol;` — same package name as Tron.proto.
        // tonic-build coalesces them into a single generated file, but we
        // alias here so call sites can write `tron::api::*` for clarity.
        // (Reaches into `protocol` because that's where prost lands the
        // service stubs.)
        pub use super::protocol::*;
    }
}

pub mod firehose {
    //! `sf.firehose.v2` streaming protocol.

    /// `Stream`, `Fetch`, `EndpointInfo` services + their request/response
    /// envelopes. Generated with both server and client stubs.
    pub mod v2 {
        tonic::include_proto!("sf.firehose.v2");
    }
}

pub mod sf {
    //! lightcycle-authored streamingfast-namespaced payload types.
    //!
    //! These are NOT vendored from upstream — we own the schema. Lives
    //! under the `sf.*` package because that's the convention Firehose
    //! consumers (Substreams modules in particular) expect for
    //! chain-specific block payloads.

    pub mod tron {
        //! TRON-specific payloads.

        /// `sf.tron.type.v1` — block, transaction, contract, and tx-info
        /// types shipped on Firehose `Response.block`. See the proto
        /// at `proto/sf/tron/type/v1/block.proto` for field-level docs.
        ///
        /// Module is named `type_v1` rather than `type::v1` because
        /// `type` is a Rust keyword. The `r#type` in the include path
        /// is prost-build's filename for the generated Rust file —
        /// it raw-escapes the keyword segment of the proto package
        /// name, so the on-disk file is literally `sf.tron.r#type.v1.rs`.
        pub mod type_v1 {
            tonic::include_proto!("sf.tron.r#type.v1");
        }
    }
}
