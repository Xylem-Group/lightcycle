//! Minimal ABI registry + event decoder for arbitrary contract logs.
//!
//! Sits alongside [`crate::events`] (which decodes the universal
//! TRC-20/721 events without a registry, by topic-0 hash). Where the
//! universal decoder stops, this registry takes over: the operator
//! pre-registers an event signature like
//! `"Swap(address indexed sender, uint256 amount0In, uint256 amount1In,
//!  uint256 amount0Out, uint256 amount1Out, address indexed to)"` and
//! the registry computes its `topic[0]` hash, then decodes any matching
//! log into a structured [`DecodedEvent`].
//!
//! ## Scope of v0.1
//!
//! Supports the **static-type** subset of Solidity event params, which
//! covers the long tail of real-world contract events:
//!
//! - Indexed: `address`, `uint8..=uint256`, `int8..=int256`, `bool`,
//!   `bytes32`, smaller `bytesN` (left-padded), other fixed-size types
//! - Non-indexed: same set, encoded as 32-byte words in `data`
//!
//! Out of scope (intentional v0.1 cut):
//! - Dynamic types (`string`, `bytes`, `T[]`) in non-indexed params —
//!   these use offset+length encoding which adds substantial parser
//!   complexity. The biggest indexer use cases (DEX swaps, lending
//!   accruals, governance votes) are static-only.
//! - Tuple types and nested structs.
//! - Anonymous events (no `topic[0]` to match against).
//!
//! When extension lands, the [`AbiType`] enum grows and the decoder
//! gains a non-static decode path. Existing registered signatures stay
//! source-compatible.
//!
//! ## Why hand-rolled
//!
//! `alloy-sol-types` is the gold standard but pulls a large dep tree.
//! `ethabi` is older and unmaintained. The static-types subset is ~250
//! lines including the parser; building it ourselves keeps the
//! transitive deps of `lightcycle-codec` honest and lets us tailor the
//! error messages to lightcycle's failure modes.

use std::collections::HashMap;
use std::fmt;

use sha3::{Digest, Keccak256};

use lightcycle_types::Address;

use crate::tx_info::Log;

/// Static Solidity types we currently decode. The `usize` bit-width
/// for [`AbiType::Uint`] / [`AbiType::Int`] / [`AbiType::Bytes`] is
/// 1..=32 bytes — we keep the raw 32-byte word in [`AbiValue`] and
/// leave width-aware truncation/sign-extension to the consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbiType {
    Address,
    Bool,
    /// `uintN` for `N` in 8..=256, divisible by 8. Stored as 32-byte
    /// big-endian; top `(32 - N/8)` bytes are zero on a well-formed
    /// emission.
    Uint(u16),
    /// `intN` for `N` in 8..=256, divisible by 8. Stored as 32-byte
    /// big-endian two's complement.
    Int(u16),
    /// `bytesN` for `N` in 1..=32. Stored left-justified in a 32-byte
    /// word; trailing `(32 - N)` bytes are zero on a well-formed
    /// emission.
    Bytes(u8),
}

impl fmt::Display for AbiType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Address => f.write_str("address"),
            Self::Bool => f.write_str("bool"),
            Self::Uint(n) => write!(f, "uint{n}"),
            Self::Int(n) => write!(f, "int{n}"),
            Self::Bytes(n) => write!(f, "bytes{n}"),
        }
    }
}

/// One decoded ABI value. The 32-byte raw word is preserved for
/// numeric types so consumers can convert to their preferred bigint
/// crate without our enum committing to one. Address values are
/// reconstructed in TRON's 21-byte form (network prefix `0x41` +
/// 20-byte hash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbiValue {
    Address(Address),
    Bool(bool),
    /// 32-byte big-endian word for `uintN` / `intN` / `bytes32`. The
    /// type tag stays on the originating [`AbiParam`] alongside this
    /// value, so the consumer knows whether to interpret as unsigned,
    /// two's-complement, or fixed-bytes.
    Word([u8; 32]),
}

/// One declared parameter on an event signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbiParam {
    /// Name as written in the registered signature. May be empty if
    /// the operator omitted parameter names (e.g. registered as
    /// `Swap(address,uint256)`).
    pub name: String,
    pub ty: AbiType,
    pub indexed: bool,
}

/// One registered event signature, with its derived `topic[0]` hash.
/// Cheap to clone; the hash is precomputed on construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventSignature {
    pub name: String,
    pub params: Vec<AbiParam>,
    /// `keccak256(canonical_signature_string)`. Indexed and non-indexed
    /// params are NOT distinguished in the canonical string per Solidity
    /// ABI spec — the order of types is what matters.
    pub topic0: [u8; 32],
}

impl EventSignature {
    /// Parse a Solidity-style event signature into a registry entry.
    ///
    /// Accepted shapes:
    /// - `"Transfer(address,address,uint256)"` (compact, no names)
    /// - `"Transfer(address indexed from, address indexed to, uint256 value)"`
    ///   (full, with names + indexed annotations)
    /// - Mixed: `"Swap(address indexed sender, uint256, uint256)"`
    ///
    /// Whitespace is tolerated. The canonical string for keccak hashing
    /// is rebuilt from the parsed types (no names, no indexed
    /// annotations) so a spelling like `"Transfer(address  ,address,
    /// uint256 )"` produces the same `topic0` as the standard form.
    pub fn parse(s: &str) -> Result<Self, AbiParseError> {
        let s = s.trim();
        let open = s
            .find('(')
            .ok_or_else(|| AbiParseError::Malformed("missing '(' in event signature".into()))?;
        if !s.ends_with(')') {
            return Err(AbiParseError::Malformed(
                "event signature must end with ')'".into(),
            ));
        }
        let name = s[..open].trim().to_string();
        if name.is_empty() {
            return Err(AbiParseError::Malformed("empty event name".into()));
        }
        let body = &s[open + 1..s.len() - 1];
        let params = parse_param_list(body)?;
        let canonical = canonical_signature(&name, &params);
        let topic0_hash = Keccak256::digest(canonical.as_bytes());
        let mut topic0 = [0u8; 32];
        topic0.copy_from_slice(&topic0_hash);
        Ok(Self {
            name,
            params,
            topic0,
        })
    }
}

fn parse_param_list(body: &str) -> Result<Vec<AbiParam>, AbiParseError> {
    let body = body.trim();
    if body.is_empty() {
        return Ok(Vec::new());
    }
    body.split(',')
        .map(|tok| parse_one_param(tok.trim()))
        .collect()
}

fn parse_one_param(s: &str) -> Result<AbiParam, AbiParseError> {
    // Accepted forms:
    //   "address"
    //   "address sender"
    //   "address indexed sender"
    //   "uint256"
    let tokens: Vec<&str> = s.split_whitespace().collect();
    let (ty_tok, indexed, name): (&str, bool, String) = match tokens.as_slice() {
        [ty] => (ty, false, String::new()),
        // The "indexed" branch must precede the bare 2-tuple branch
        // — `[ty, "indexed"]` is a strict subset of `[ty, name]`, and
        // rust matches the first arm that fits.
        [ty, "indexed"] => (ty, true, String::new()),
        [ty, name] => (ty, false, (*name).to_string()),
        [ty, "indexed", name] => (ty, true, (*name).to_string()),
        _ => {
            return Err(AbiParseError::Malformed(format!(
                "unrecognized parameter syntax: '{s}'"
            )));
        }
    };
    let ty = parse_type(ty_tok)?;
    Ok(AbiParam { name, ty, indexed })
}

fn parse_type(s: &str) -> Result<AbiType, AbiParseError> {
    // Reject array types first; otherwise "uint256[]" parses as a
    // bad uint width and falls into UnknownType. Cleaner error.
    if s.contains('[') {
        return Err(AbiParseError::UnsupportedType(format!(
            "array types like '{s}' not supported in v0.1"
        )));
    }
    if s == "address" {
        return Ok(AbiType::Address);
    }
    if s == "bool" {
        return Ok(AbiType::Bool);
    }
    if let Some(rest) = s.strip_prefix("uint") {
        let bits: u16 = if rest.is_empty() {
            256
        } else {
            rest.parse()
                .map_err(|_| AbiParseError::UnknownType(s.into()))?
        };
        if bits == 0 || bits > 256 || !bits.is_multiple_of(8) {
            return Err(AbiParseError::UnknownType(s.into()));
        }
        return Ok(AbiType::Uint(bits));
    }
    if let Some(rest) = s.strip_prefix("int") {
        let bits: u16 = if rest.is_empty() {
            256
        } else {
            rest.parse()
                .map_err(|_| AbiParseError::UnknownType(s.into()))?
        };
        if bits == 0 || bits > 256 || !bits.is_multiple_of(8) {
            return Err(AbiParseError::UnknownType(s.into()));
        }
        return Ok(AbiType::Int(bits));
    }
    if let Some(rest) = s.strip_prefix("bytes") {
        // bytes32, bytes16, etc — fixed-size only in v0.1.
        // Bare "bytes" (dynamic) is not yet supported; fail clearly.
        if rest.is_empty() {
            return Err(AbiParseError::UnsupportedType(
                "dynamic 'bytes' (non-fixed) not supported in v0.1; use bytesN".into(),
            ));
        }
        let n: u8 = rest
            .parse()
            .map_err(|_| AbiParseError::UnknownType(s.into()))?;
        if n == 0 || n > 32 {
            return Err(AbiParseError::UnknownType(s.into()));
        }
        return Ok(AbiType::Bytes(n));
    }
    if s == "string" {
        return Err(AbiParseError::UnsupportedType(
            "'string' is dynamic; not supported in v0.1".into(),
        ));
    }
    Err(AbiParseError::UnknownType(s.into()))
}

fn canonical_signature(name: &str, params: &[AbiParam]) -> String {
    let mut s = String::with_capacity(name.len() + 2 + params.len() * 8);
    s.push_str(name);
    s.push('(');
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        // canonical form ALWAYS uses `uint256`/`int256` (not the bare
        // forms). We've already normalized via parse_type, so just
        // print our internal Display repr.
        s.push_str(&p.ty.to_string());
    }
    s.push(')');
    s
}

/// Errors from parsing event signatures.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AbiParseError {
    #[error("malformed signature: {0}")]
    Malformed(String),
    #[error("unknown type: {0}")]
    UnknownType(String),
    #[error("type not supported in v0.1: {0}")]
    UnsupportedType(String),
}

/// Errors from decoding an actual log against a registered signature.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AbiDecodeError {
    #[error("indexed param count mismatch: expected {expected}, got {got}")]
    IndexedCountMismatch { expected: usize, got: usize },
    #[error(
        "non-indexed data length mismatch: expected {expected} bytes ({words} words × 32), got {got}"
    )]
    DataLenMismatch {
        expected: usize,
        got: usize,
        words: usize,
    },
}

/// Output of [`EventRegistry::decode`]: the matched signature plus
/// each parameter paired with its decoded value. Order matches the
/// declared signature order.
#[derive(Debug, Clone)]
pub struct DecodedEvent {
    pub name: String,
    /// One entry per parameter, in declaration order. The `param`
    /// field is the original schema; the `value` is the decoded raw
    /// 32-byte word reinterpreted by type.
    pub params: Vec<(AbiParam, AbiValue)>,
}

impl DecodedEvent {
    /// Look up a parameter value by name. Returns `None` if no
    /// parameter with that name was decoded (either anonymous params,
    /// or the consumer's spelling diverges from the registered
    /// signature).
    pub fn value(&self, name: &str) -> Option<&AbiValue> {
        self.params
            .iter()
            .find(|(p, _)| p.name == name)
            .map(|(_, v)| v)
    }
}

/// Map of `topic[0]` hash → registered event signature. One topic hash
/// can collide across two different signatures with the same param
/// types — registry is last-write-wins; operators should refrain from
/// registering colliding signatures with different decode intent.
#[derive(Debug, Default, Clone)]
pub struct EventRegistry {
    by_topic0: HashMap<[u8; 32], EventSignature>,
}

impl EventRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one signature. Overwrites any prior registration with
    /// the same `topic[0]`.
    pub fn register(&mut self, sig: EventSignature) {
        self.by_topic0.insert(sig.topic0, sig);
    }

    /// Convenience: parse + register in one call.
    pub fn register_signature(&mut self, s: &str) -> Result<(), AbiParseError> {
        let sig = EventSignature::parse(s)?;
        self.register(sig);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.by_topic0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_topic0.is_empty()
    }

    /// Look up by topic[0] without decoding. Useful when the consumer
    /// just wants to know the event name.
    pub fn signature(&self, topic0: &[u8; 32]) -> Option<&EventSignature> {
        self.by_topic0.get(topic0)
    }

    /// Decode a log if its `topic[0]` matches a registered signature.
    /// Returns:
    /// - `Ok(Some(DecodedEvent))` on a clean decode
    /// - `Ok(None)` if the log's `topic[0]` isn't registered (not an
    ///   error — most logs in a real block won't match)
    /// - `Err(_)` if the log matches a registered signature but is
    ///   malformed for that schema (wrong topic count, wrong data len)
    pub fn decode(&self, log: &Log) -> Result<Option<DecodedEvent>, AbiDecodeError> {
        if log.topics.is_empty() {
            return Ok(None);
        }
        let Some(sig) = self.by_topic0.get(&log.topics[0]) else {
            return Ok(None);
        };
        decode_with_signature(sig, log).map(Some)
    }
}

fn decode_with_signature(sig: &EventSignature, log: &Log) -> Result<DecodedEvent, AbiDecodeError> {
    // Indexed params consume topics[1..]; non-indexed params consume
    // 32-byte chunks of `data`. The first topic is the event signature
    // hash itself, so the count of indexed params is `topics.len() - 1`.
    let expected_indexed = sig.params.iter().filter(|p| p.indexed).count();
    let got_indexed = log.topics.len().saturating_sub(1);
    if got_indexed != expected_indexed {
        return Err(AbiDecodeError::IndexedCountMismatch {
            expected: expected_indexed,
            got: got_indexed,
        });
    }

    let non_indexed_count = sig.params.len() - expected_indexed;
    let expected_data = non_indexed_count.saturating_mul(32);
    if log.data.len() != expected_data {
        return Err(AbiDecodeError::DataLenMismatch {
            expected: expected_data,
            got: log.data.len(),
            words: non_indexed_count,
        });
    }

    let mut topic_cursor: usize = 1;
    let mut data_cursor: usize = 0;
    let mut out = Vec::with_capacity(sig.params.len());
    for p in &sig.params {
        let word = if p.indexed {
            let t = log.topics[topic_cursor];
            topic_cursor += 1;
            t
        } else {
            let mut w = [0u8; 32];
            w.copy_from_slice(&log.data[data_cursor..data_cursor + 32]);
            data_cursor += 32;
            w
        };
        let value = match &p.ty {
            AbiType::Address => AbiValue::Address(word_to_tron_address(&word)),
            AbiType::Bool => AbiValue::Bool(word_to_bool(&word)),
            AbiType::Uint(_) | AbiType::Int(_) | AbiType::Bytes(_) => AbiValue::Word(word),
        };
        out.push((p.clone(), value));
    }
    Ok(DecodedEvent {
        name: sig.name.clone(),
        params: out,
    })
}

fn word_to_tron_address(w: &[u8; 32]) -> Address {
    // EVM addresses are 20-byte right-aligned in a 32-byte word; TRON
    // adds a 1-byte network prefix `0x41` to form the 21-byte form.
    let mut a = [0u8; 21];
    a[0] = 0x41;
    a[1..].copy_from_slice(&w[12..]);
    Address(a)
}

fn word_to_bool(w: &[u8; 32]) -> bool {
    // ABI encodes bool as 0x00..00 (false) or 0x00..01 (true). Geth
    // tolerates any non-zero in the LSB; we match that.
    w[31] != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_log(topics: Vec<[u8; 32]>, data: Vec<u8>) -> Log {
        Log {
            address: Address([0x41; 21]),
            topics,
            data,
        }
    }

    #[test]
    fn parse_compact_signature() {
        let sig = EventSignature::parse("Transfer(address,address,uint256)").unwrap();
        assert_eq!(sig.name, "Transfer");
        assert_eq!(sig.params.len(), 3);
        assert!(sig.params.iter().all(|p| p.name.is_empty() && !p.indexed));
        // Topic0 must match the keccak hash of the canonical form.
        let expect = Keccak256::digest(b"Transfer(address,address,uint256)");
        assert_eq!(&sig.topic0[..], &expect[..]);
    }

    #[test]
    fn parse_full_signature_with_names_and_indexed() {
        let sig = EventSignature::parse(
            "Transfer(address indexed from, address indexed to, uint256 value)",
        )
        .unwrap();
        assert_eq!(sig.name, "Transfer");
        assert_eq!(sig.params.len(), 3);
        assert!(sig.params[0].indexed);
        assert!(sig.params[1].indexed);
        assert!(!sig.params[2].indexed);
        assert_eq!(sig.params[0].name, "from");
        assert_eq!(sig.params[1].name, "to");
        assert_eq!(sig.params[2].name, "value");
        // Same canonical string → same topic0 as the compact form.
        let compact = EventSignature::parse("Transfer(address,address,uint256)").unwrap();
        assert_eq!(sig.topic0, compact.topic0);
    }

    #[test]
    fn parse_handles_messy_whitespace() {
        let sig = EventSignature::parse("  Swap (  address  ,  uint256  ,  bool  )  ").unwrap();
        assert_eq!(sig.name, "Swap");
        let canon = EventSignature::parse("Swap(address,uint256,bool)").unwrap();
        assert_eq!(sig.topic0, canon.topic0);
    }

    #[test]
    fn parse_recognizes_widths() {
        for ty in ["uint8", "uint16", "uint32", "uint128", "uint256"] {
            let s = format!("Foo({ty})");
            let sig = EventSignature::parse(&s).unwrap();
            assert_eq!(sig.params[0].ty.to_string(), ty);
        }
        for ty in ["int8", "int128", "int256"] {
            let s = format!("Foo({ty})");
            let sig = EventSignature::parse(&s).unwrap();
            assert_eq!(sig.params[0].ty.to_string(), ty);
        }
    }

    #[test]
    fn parse_rejects_dynamic_bytes() {
        let err = EventSignature::parse("Foo(bytes)").unwrap_err();
        match err {
            AbiParseError::UnsupportedType(_) => {}
            _ => panic!("expected UnsupportedType"),
        }
    }

    #[test]
    fn parse_rejects_string_in_v01() {
        let err = EventSignature::parse("Foo(string)").unwrap_err();
        match err {
            AbiParseError::UnsupportedType(_) => {}
            _ => panic!("expected UnsupportedType"),
        }
    }

    #[test]
    fn parse_rejects_unknown_type() {
        let err = EventSignature::parse("Foo(uint257)").unwrap_err();
        match err {
            AbiParseError::UnknownType(_) => {}
            other => panic!("expected UnknownType, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_array_types() {
        let err = EventSignature::parse("Foo(uint256[])").unwrap_err();
        match err {
            AbiParseError::UnsupportedType(_) => {}
            other => panic!("expected UnsupportedType, got {other:?}"),
        }
    }

    #[test]
    fn parse_empty_param_list() {
        let sig = EventSignature::parse("Pause()").unwrap();
        assert_eq!(sig.params.len(), 0);
    }

    #[test]
    fn registry_decodes_swap_like_event() {
        // Uniswap V2 Swap-flavored event with a typical mix.
        let mut reg = EventRegistry::new();
        reg.register_signature(
            "Swap(address indexed sender, uint256 amount0In, uint256 amount1In, \
             uint256 amount0Out, uint256 amount1Out, address indexed to)",
        )
        .unwrap();

        let sig = EventSignature::parse(
            "Swap(address indexed sender, uint256 amount0In, uint256 amount1In, \
             uint256 amount0Out, uint256 amount1Out, address indexed to)",
        )
        .unwrap();

        // Build a synthetic log with topic0 + 2 indexed addresses + 4
        // non-indexed uint256 in data.
        let mut sender = [0u8; 32];
        sender[12..].copy_from_slice(&[0x11; 20]);
        let mut to = [0u8; 32];
        to[12..].copy_from_slice(&[0x22; 20]);
        let topics = vec![sig.topic0, sender, to];
        let mut data = Vec::with_capacity(32 * 4);
        for v in [100u64, 0, 0, 200u64] {
            let mut w = [0u8; 32];
            w[24..].copy_from_slice(&v.to_be_bytes());
            data.extend_from_slice(&w);
        }

        let log = synth_log(topics, data);
        let decoded = reg.decode(&log).unwrap().expect("matched");
        assert_eq!(decoded.name, "Swap");
        assert_eq!(decoded.params.len(), 6);

        // Sender (indexed, address) should round-trip to TRON form.
        let sender_v = decoded.value("sender").unwrap();
        let AbiValue::Address(a) = sender_v else {
            panic!("expected Address")
        };
        assert_eq!(a.0[0], 0x41);
        assert_eq!(&a.0[1..], &[0x11; 20]);

        // amount0In should be 100.
        let amount0_in = decoded.value("amount0In").unwrap();
        let AbiValue::Word(w) = amount0_in else {
            panic!("expected Word")
        };
        let mut expect = [0u8; 32];
        expect[24..].copy_from_slice(&100u64.to_be_bytes());
        assert_eq!(w, &expect);
    }

    #[test]
    fn decode_returns_none_for_unregistered_event() {
        let reg = EventRegistry::new();
        let log = synth_log(vec![[0xab; 32]], vec![]);
        assert!(reg.decode(&log).unwrap().is_none());
    }

    #[test]
    fn decode_errors_on_indexed_count_mismatch() {
        let mut reg = EventRegistry::new();
        reg.register_signature("Move(address indexed who, uint256 amount)")
            .unwrap();
        let sig = EventSignature::parse("Move(address indexed who, uint256 amount)").unwrap();
        // Send only topic0 — missing the indexed `who` topic.
        let log = synth_log(vec![sig.topic0], vec![0u8; 32]);
        let err = reg.decode(&log).unwrap_err();
        match err {
            AbiDecodeError::IndexedCountMismatch { expected, got } => {
                assert_eq!(expected, 1);
                assert_eq!(got, 0);
            }
            _ => panic!("expected IndexedCountMismatch"),
        }
    }

    #[test]
    fn decode_errors_on_data_len_mismatch() {
        let mut reg = EventRegistry::new();
        reg.register_signature("Move(uint256 amount, uint256 fee)")
            .unwrap();
        let sig = EventSignature::parse("Move(uint256 amount, uint256 fee)").unwrap();
        // 2 non-indexed params expect 64 bytes; we send 32.
        let log = synth_log(vec![sig.topic0], vec![0u8; 32]);
        let err = reg.decode(&log).unwrap_err();
        match err {
            AbiDecodeError::DataLenMismatch {
                expected,
                got,
                words,
            } => {
                assert_eq!(expected, 64);
                assert_eq!(got, 32);
                assert_eq!(words, 2);
            }
            _ => panic!("expected DataLenMismatch"),
        }
    }

    #[test]
    fn decode_handles_anonymous_param_names() {
        // Operator omits names entirely — decode still works, value()
        // lookup just returns None for any name (params are
        // positional).
        let mut reg = EventRegistry::new();
        reg.register_signature("Swap(uint256,uint256)").unwrap();
        let sig = EventSignature::parse("Swap(uint256,uint256)").unwrap();
        let mut data = Vec::new();
        data.extend_from_slice(&[0u8; 31]);
        data.push(7);
        data.extend_from_slice(&[0u8; 31]);
        data.push(13);
        let log = synth_log(vec![sig.topic0], data);
        let decoded = reg.decode(&log).unwrap().expect("matched");
        assert_eq!(decoded.params.len(), 2);
        assert!(decoded.value("amount").is_none());
        let AbiValue::Word(w0) = &decoded.params[0].1 else {
            panic!()
        };
        assert_eq!(w0[31], 7);
    }

    #[test]
    fn decode_bool_word() {
        let mut reg = EventRegistry::new();
        reg.register_signature("Set(bool flag)").unwrap();
        let sig = EventSignature::parse("Set(bool flag)").unwrap();
        let mut data = vec![0u8; 32];
        data[31] = 1;
        let log = synth_log(vec![sig.topic0], data);
        let d = reg.decode(&log).unwrap().expect("match");
        match d.value("flag").unwrap() {
            AbiValue::Bool(true) => {}
            other => panic!("expected Bool(true), got {other:?}"),
        }

        let log_false = synth_log(vec![sig.topic0], vec![0u8; 32]);
        let d2 = reg.decode(&log_false).unwrap().expect("match");
        match d2.value("flag").unwrap() {
            AbiValue::Bool(false) => {}
            other => panic!("expected Bool(false), got {other:?}"),
        }
    }

    #[test]
    fn topic0_known_signature_matches_external_reference() {
        // Cross-check against the `events` module's pre-baked
        // TRC20_TRANSFER_TOPIC0 — both should derive identically from
        // the canonical signature.
        let sig = EventSignature::parse("Transfer(address,address,uint256)").unwrap();
        assert_eq!(sig.topic0, crate::events::TRC20_TRANSFER_TOPIC0);
    }

    #[test]
    fn registry_signature_lookup_returns_handle() {
        let mut reg = EventRegistry::new();
        let sig = EventSignature::parse("Pause()").unwrap();
        let topic = sig.topic0;
        reg.register(sig);
        assert!(reg.signature(&topic).is_some());
        assert!(reg.signature(&[0xff; 32]).is_none());
    }

    #[test]
    fn registry_register_overwrites_collision() {
        // Two signatures with the same canonical body collide on
        // topic0; second wins.
        let mut reg = EventRegistry::new();
        reg.register_signature("Foo(uint256)").unwrap();
        // Canonical "Foo(uint256)" — same topic0, but registered with
        // a parameter name.
        reg.register_signature("Foo(uint256value)").unwrap_err(); // bad parse
        reg.register_signature("Foo(uint256 value)").unwrap();
        // Still one entry; second registration overwrote the first.
        assert_eq!(reg.len(), 1);
        let sig = EventSignature::parse("Foo(uint256)").unwrap();
        assert_eq!(reg.signature(&sig.topic0).unwrap().params[0].name, "value");
    }
}
