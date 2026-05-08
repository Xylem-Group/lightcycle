//! Smoke test — confirm the generated types are reachable from a
//! downstream consumer's perspective and that prost's fundamentals
//! (default-construction + encode/decode round-trip) work for at least
//! one type from each vendored proto tree.

use prost::Message;

#[test]
fn tron_block_default_roundtrip() {
    // From java-tron's monolithic core/Tron.proto, package `protocol`.
    let block = lightcycle_proto::tron::protocol::Block::default();
    let encoded = block.encode_to_vec();
    let decoded = lightcycle_proto::tron::protocol::Block::decode(&encoded[..])
        .expect("decode default Block");
    assert_eq!(block, decoded);
}

#[test]
fn tron_transfer_contract_default_roundtrip() {
    // From core/contract/balance_contract.proto. Lives under the same
    // `protocol` package as the monolithic schema.
    let tx = lightcycle_proto::tron::protocol::TransferContract::default();
    let encoded = tx.encode_to_vec();
    let decoded =
        lightcycle_proto::tron::protocol::TransferContract::decode(&encoded[..])
            .expect("decode default TransferContract");
    assert_eq!(tx, decoded);
}

#[test]
fn firehose_v2_request_default_roundtrip() {
    // From streamingfast/proto's sf/firehose/v2/firehose.proto.
    let req = lightcycle_proto::firehose::v2::Request::default();
    let encoded = req.encode_to_vec();
    let decoded = lightcycle_proto::firehose::v2::Request::decode(&encoded[..])
        .expect("decode default firehose Request");
    assert_eq!(req, decoded);
}
