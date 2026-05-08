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
}

pub mod firehose {
    //! `sf.firehose.v2` streaming protocol.

    /// `Stream`, `Fetch`, `EndpointInfo` services + their request/response
    /// envelopes. Generated with both server and client stubs.
    pub mod v2 {
        tonic::include_proto!("sf.firehose.v2");
    }
}
