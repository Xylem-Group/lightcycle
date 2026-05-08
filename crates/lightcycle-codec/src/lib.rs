//! Block decode, signature verification, and event decoding.
//!
//! Pure CPU. No I/O, no async. The hot path of the relayer.

#![allow(dead_code)]

use lightcycle_types::Result;

/// Decode a raw protobuf block (TRON wire format) into the domain `Block` type
/// and verify the witness signature against the supplied SR set.
pub fn decode_block(_raw: &[u8], _sr_set: &SrSet) -> Result<DecodedBlock> {
    todo!("protobuf decode + secp256k1 sigverify")
}

/// Decode an event log payload using the supplied ABI.
pub fn decode_event(_topics: &[[u8; 32]], _data: &[u8], _abi: &EventAbi) -> Result<DecodedEvent> {
    todo!("event ABI decode")
}

#[derive(Debug, Clone)]
pub struct DecodedBlock;

#[derive(Debug, Clone)]
pub struct DecodedEvent;

#[derive(Debug, Clone)]
pub struct SrSet;

#[derive(Debug, Clone)]
pub struct EventAbi;
