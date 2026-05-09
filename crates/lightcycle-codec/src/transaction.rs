//! Transaction-side codec.
//!
//! v0.1 surface:
//!
//! - [`DecodedTransaction`] — friendly view of the wire `Transaction`.
//!   Carries the tx hash, timestamp / expiration / fee_limit envelope,
//!   the per-contract decoded payloads, and the witness signatures.
//!
//! - [`DecodedContract`] — typed contract payload. The four
//!   high-volume types (`Transfer`, `TransferAsset`,
//!   `TriggerSmartContract`, `CreateSmartContract`) get
//!   per-field-decoded variants. Everything else is surfaced as
//!   [`DecodedContract::Other`] with the raw `Any.value` bytes so
//!   consumers that care about a specific governance/admin contract
//!   can decode against the matching java-tron protobuf message.
//!
//! - [`ContractKind`] — lightweight tag-only enum mirroring
//!   java-tron's wire `ContractType`. Used by [`DecodedContract::kind`]
//!   and by consumers that filter by type without paying the
//!   payload-decode cost. `Other(i32)` keeps us forward-compatible
//!   with TRON forks that add a new contract type ahead of our vendor
//!   pin.
//!
//! `DecodedTransaction.contracts` is `Vec<DecodedContract>`. Most TRON
//! txs have a single contract; the `Vec` shape is preserved because
//! the wire schema permits multi-contract txs (multi-sig extension).

use prost::Message;

use lightcycle_proto::tron::protocol;
use lightcycle_proto::tron::protocol::transaction::contract::ContractType as WireContractType;
use lightcycle_types::{Address, TxHash};

use crate::error::{CodecError, Result};

/// Decoded transaction shape exposed to consumers.
#[derive(Debug, Clone)]
pub struct DecodedTransaction {
    /// 32-byte sha256 of `raw_data`. Stable identifier, matches what
    /// java-tron's `txID` returns over RPC.
    pub hash: TxHash,
    /// `Transaction.raw_data.timestamp` (ms since epoch).
    pub timestamp_ms: i64,
    /// `Transaction.raw_data.expiration` (ms since epoch).
    pub expiration_ms: i64,
    /// `Transaction.raw_data.fee_limit` — sun (1 TRX = 1e6 sun).
    pub fee_limit_sun: i64,
    /// Per-contract decoded payload. Length is normally 1 — TRON's
    /// wire schema permits multi-contract txs but the active mainnet
    /// contracts produce one each.
    pub contracts: Vec<DecodedContract>,
    /// Witness/SR/multisig signatures. Length is normally 1.
    pub signatures: Vec<Vec<u8>>,
}

/// Typed contract payload. The four high-volume types carry decoded
/// fields; everything else falls into `Other` with the raw bytes
/// preserved so consumers can decode against java-tron's protobuf
/// message for that type if they need to.
#[derive(Debug, Clone)]
pub enum DecodedContract {
    /// `TransferContract` — TRX transfer.
    Transfer {
        owner: Address,
        to: Address,
        /// Amount in sun (1 TRX = 1e6 sun).
        amount_sun: i64,
    },
    /// `TransferAssetContract` — TRC-10 token transfer. Different
    /// from TRC-20 (which is a `TriggerSmartContract` to an ERC-20-
    /// shaped contract); TRC-10 is a native token type.
    TransferAsset {
        owner: Address,
        to: Address,
        /// `asset_name` is the TRC-10 token id post-`ALLOW_SAME_TOKEN_NAME`
        /// (string-encoded numeric id) and the token name pre-proposal.
        /// Surfaced as raw bytes; consumers interpret based on the
        /// proposal-active flag they track.
        asset_name: Vec<u8>,
        amount: i64,
    },
    /// `TriggerSmartContract` — call into a deployed contract. This
    /// is the bulk of mainnet activity; TRC-20 transfers, DEX trades,
    /// USDT issuance — all flow through here.
    TriggerSmartContract {
        owner: Address,
        contract_address: Address,
        /// TRX value attached to the call (sun).
        call_value: i64,
        /// TRC-10 token value attached to the call.
        call_token_value: i64,
        /// TRC-10 token id (when `call_token_value > 0`).
        token_id: i64,
        /// ABI-encoded function selector + args. Consumers that want
        /// a structured view need an ABI decoder + the contract's ABI.
        data: Vec<u8>,
    },
    /// `CreateSmartContract` — deploy a new contract. We surface the
    /// deploy parameters but not the full ABI tree (kept as part of
    /// the on-chain `SmartContract.abi` proto, fetchable separately
    /// via `getContract`).
    CreateSmartContract {
        owner: Address,
        /// `SmartContract.contract_address` — set by java-tron after
        /// deploy. Populated only when java-tron has filled it in;
        /// often zero on a fresh tx.
        contract_address: Option<Address>,
        bytecode: Vec<u8>,
        name: String,
        /// 0..100. Percent of energy the user pays vs. the contract's
        /// origin pays (when origin has a stake).
        consume_user_resource_percent: i64,
        origin_energy_limit: i64,
        /// TRC-10 sent with the deploy (rare).
        call_token_value: i64,
        token_id: i64,
    },
    /// Anything else — administrative/governance contracts plus the
    /// long tail of low-volume types. Carries the kind tag and the
    /// raw `Any.value` bytes.
    Other {
        kind: ContractKind,
        /// Original `Contract.parameter.value` bytes (the inner Any
        /// payload). Empty if the contract had no parameter set.
        raw: Vec<u8>,
    },
}

impl DecodedContract {
    /// Lightweight kind tag, regardless of whether the variant carries
    /// a full payload or is `Other`. Useful for filtering / counting
    /// without matching on every variant.
    pub fn kind(&self) -> ContractKind {
        match self {
            Self::Transfer { .. } => ContractKind::Transfer,
            Self::TransferAsset { .. } => ContractKind::TransferAsset,
            Self::TriggerSmartContract { .. } => ContractKind::TriggerSmartContract,
            Self::CreateSmartContract { .. } => ContractKind::CreateSmartContract,
            Self::Other { kind, .. } => *kind,
        }
    }
}

/// Friendly contract enum. The wire enum's variants all end in
/// `Contract`; this drops that suffix and groups the v2 variants.
/// `Other(i32)` keeps the codec forward-compatible with a TRON fork
/// that adds a new contract type before we re-vendor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractKind {
    AccountCreate,
    Transfer,
    TransferAsset,
    VoteAsset,
    VoteWitness,
    WitnessCreate,
    AssetIssue,
    WitnessUpdate,
    ParticipateAssetIssue,
    AccountUpdate,
    FreezeBalance,
    UnfreezeBalance,
    WithdrawBalance,
    UnfreezeAsset,
    UpdateAsset,
    ProposalCreate,
    ProposalApprove,
    ProposalDelete,
    SetAccountId,
    Custom,
    CreateSmartContract,
    TriggerSmartContract,
    GetContract,
    UpdateSetting,
    ExchangeCreate,
    ExchangeInject,
    ExchangeWithdraw,
    ExchangeTransaction,
    UpdateEnergyLimit,
    AccountPermissionUpdate,
    ClearAbi,
    UpdateBrokerage,
    ShieldedTransfer,
    MarketSellAsset,
    MarketCancelOrder,
    FreezeBalanceV2,
    UnfreezeBalanceV2,
    WithdrawExpireUnfreeze,
    DelegateResource,
    UnDelegateResource,
    CancelAllUnfreezeV2,
    /// Unknown / future contract type. The wrapped int is the raw
    /// `ContractType` tag from the wire — useful for telemetry.
    Other(i32),
}

impl ContractKind {
    /// Map a wire-enum-derived `i32` to our friendly enum. We accept
    /// the raw `i32` (rather than the wire enum) because protobuf's
    /// open-enum semantics mean we may receive a tag that prost
    /// can't decode into its enum.
    pub fn from_wire(tag: i32) -> Self {
        match WireContractType::try_from(tag) {
            Ok(t) => Self::from_wire_enum(t),
            Err(_) => Self::Other(tag),
        }
    }

    fn from_wire_enum(t: WireContractType) -> Self {
        use WireContractType::*;
        match t {
            AccountCreateContract => Self::AccountCreate,
            TransferContract => Self::Transfer,
            TransferAssetContract => Self::TransferAsset,
            VoteAssetContract => Self::VoteAsset,
            VoteWitnessContract => Self::VoteWitness,
            WitnessCreateContract => Self::WitnessCreate,
            AssetIssueContract => Self::AssetIssue,
            WitnessUpdateContract => Self::WitnessUpdate,
            ParticipateAssetIssueContract => Self::ParticipateAssetIssue,
            AccountUpdateContract => Self::AccountUpdate,
            FreezeBalanceContract => Self::FreezeBalance,
            UnfreezeBalanceContract => Self::UnfreezeBalance,
            WithdrawBalanceContract => Self::WithdrawBalance,
            UnfreezeAssetContract => Self::UnfreezeAsset,
            UpdateAssetContract => Self::UpdateAsset,
            ProposalCreateContract => Self::ProposalCreate,
            ProposalApproveContract => Self::ProposalApprove,
            ProposalDeleteContract => Self::ProposalDelete,
            SetAccountIdContract => Self::SetAccountId,
            CustomContract => Self::Custom,
            CreateSmartContract => Self::CreateSmartContract,
            TriggerSmartContract => Self::TriggerSmartContract,
            GetContract => Self::GetContract,
            UpdateSettingContract => Self::UpdateSetting,
            ExchangeCreateContract => Self::ExchangeCreate,
            ExchangeInjectContract => Self::ExchangeInject,
            ExchangeWithdrawContract => Self::ExchangeWithdraw,
            ExchangeTransactionContract => Self::ExchangeTransaction,
            UpdateEnergyLimitContract => Self::UpdateEnergyLimit,
            AccountPermissionUpdateContract => Self::AccountPermissionUpdate,
            ClearAbiContract => Self::ClearAbi,
            UpdateBrokerageContract => Self::UpdateBrokerage,
            ShieldedTransferContract => Self::ShieldedTransfer,
            MarketSellAssetContract => Self::MarketSellAsset,
            MarketCancelOrderContract => Self::MarketCancelOrder,
            FreezeBalanceV2Contract => Self::FreezeBalanceV2,
            UnfreezeBalanceV2Contract => Self::UnfreezeBalanceV2,
            WithdrawExpireUnfreezeContract => Self::WithdrawExpireUnfreeze,
            DelegateResourceContract => Self::DelegateResource,
            UnDelegateResourceContract => Self::UnDelegateResource,
            CancelAllUnfreezeV2Contract => Self::CancelAllUnfreezeV2,
        }
    }
}

/// Decode a single wire `Transaction` into our friendlier shape.
/// Decodes contract `Any` payloads inline for the four high-volume
/// types; falls back to [`DecodedContract::Other`] for everything
/// else, preserving the raw bytes.
pub(crate) fn decode_transaction(tx: &protocol::Transaction) -> Result<DecodedTransaction> {
    use sha2::{Digest, Sha256};

    let raw = tx
        .raw_data
        .as_ref()
        .ok_or(CodecError::MissingTransactionRaw)?;

    // Tx hash = sha256 of the encoded raw_data. We re-encode rather
    // than holding onto the original byte slice; the input we receive
    // is the outer Block's encoded form, and the inner raw_data slice
    // isn't directly addressable from here.
    let raw_bytes = raw.encode_to_vec();
    let hash = Sha256::digest(&raw_bytes);
    let mut hash_arr = [0u8; 32];
    hash_arr.copy_from_slice(&hash);

    let contracts = raw
        .contract
        .iter()
        .map(decode_contract)
        .collect::<Result<Vec<_>>>()?;

    Ok(DecodedTransaction {
        hash: TxHash(hash_arr),
        timestamp_ms: raw.timestamp,
        expiration_ms: raw.expiration,
        fee_limit_sun: raw.fee_limit,
        contracts,
        signatures: tx.signature.clone(),
    })
}

fn decode_contract(c: &protocol::transaction::Contract) -> Result<DecodedContract> {
    let kind = ContractKind::from_wire(c.r#type);
    let any_bytes: &[u8] = c
        .parameter
        .as_ref()
        .map(|a| a.value.as_slice())
        .unwrap_or(&[]);

    match kind {
        ContractKind::Transfer => {
            let m = protocol::TransferContract::decode(any_bytes)?;
            Ok(DecodedContract::Transfer {
                owner: bytes_to_address(&m.owner_address, "TransferContract.owner_address")?,
                to: bytes_to_address(&m.to_address, "TransferContract.to_address")?,
                amount_sun: m.amount,
            })
        }
        ContractKind::TransferAsset => {
            let m = protocol::TransferAssetContract::decode(any_bytes)?;
            Ok(DecodedContract::TransferAsset {
                owner: bytes_to_address(&m.owner_address, "TransferAssetContract.owner_address")?,
                to: bytes_to_address(&m.to_address, "TransferAssetContract.to_address")?,
                asset_name: m.asset_name,
                amount: m.amount,
            })
        }
        ContractKind::TriggerSmartContract => {
            let m = protocol::TriggerSmartContract::decode(any_bytes)?;
            Ok(DecodedContract::TriggerSmartContract {
                owner: bytes_to_address(&m.owner_address, "TriggerSmartContract.owner_address")?,
                contract_address: bytes_to_address(
                    &m.contract_address,
                    "TriggerSmartContract.contract_address",
                )?,
                call_value: m.call_value,
                call_token_value: m.call_token_value,
                token_id: m.token_id,
                data: m.data,
            })
        }
        ContractKind::CreateSmartContract => {
            let m = protocol::CreateSmartContract::decode(any_bytes)?;
            // Inner SmartContract carries bytecode, name, and the
            // contract_address java-tron fills in post-deploy.
            let inner = m.new_contract.unwrap_or_default();
            // contract_address may be zero-length on fresh deploy txs
            // (java-tron fills it in only after execution succeeds).
            let contract_address = if inner.contract_address.is_empty() {
                None
            } else {
                Some(bytes_to_address(
                    &inner.contract_address,
                    "SmartContract.contract_address",
                )?)
            };
            Ok(DecodedContract::CreateSmartContract {
                owner: bytes_to_address(&m.owner_address, "CreateSmartContract.owner_address")?,
                contract_address,
                bytecode: inner.bytecode,
                name: inner.name,
                consume_user_resource_percent: inner.consume_user_resource_percent,
                origin_energy_limit: inner.origin_energy_limit,
                call_token_value: m.call_token_value,
                token_id: m.token_id,
            })
        }
        // Everything else: preserve the raw bytes so consumers can
        // decode against the right protobuf message themselves.
        other => Ok(DecodedContract::Other {
            kind: other,
            raw: any_bytes.to_vec(),
        }),
    }
}

fn bytes_to_address(b: &[u8], field: &'static str) -> Result<Address> {
    if b.len() != 21 {
        return Err(CodecError::BadAddress {
            field,
            got: b.len(),
        });
    }
    let mut a = [0u8; 21];
    a.copy_from_slice(b);
    Ok(Address(a))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightcycle_proto::tron::protocol::transaction::Contract;
    use prost_types::Any;

    fn addr(b: u8) -> Address {
        let mut a = [0u8; 21];
        a[0] = 0x41;
        a[1..].fill(b);
        Address(a)
    }

    fn build_contract(type_tag: i32, value: Vec<u8>) -> Contract {
        Contract {
            r#type: type_tag,
            parameter: Some(Any {
                type_url: String::new(),
                value,
            }),
            provider: vec![],
            contract_name: vec![],
            permission_id: 0,
        }
    }

    #[test]
    fn decodes_transfer_payload() {
        let m = protocol::TransferContract {
            owner_address: addr(0x11).0.to_vec(),
            to_address: addr(0x22).0.to_vec(),
            amount: 1_000_000,
        };
        let c = build_contract(WireContractType::TransferContract as i32, m.encode_to_vec());
        let d = decode_contract(&c).expect("decode");
        match d {
            DecodedContract::Transfer { owner, to, amount_sun } => {
                assert_eq!(owner, addr(0x11));
                assert_eq!(to, addr(0x22));
                assert_eq!(amount_sun, 1_000_000);
            }
            other => panic!("expected Transfer, got {other:?}"),
        }
    }

    #[test]
    fn decodes_trigger_smart_contract_payload() {
        let m = protocol::TriggerSmartContract {
            owner_address: addr(0x33).0.to_vec(),
            contract_address: addr(0x44).0.to_vec(),
            call_value: 0,
            data: vec![0xa9, 0x05, 0x9c, 0xbb, 0xde, 0xad, 0xbe, 0xef],
            call_token_value: 0,
            token_id: 0,
        };
        let c = build_contract(
            WireContractType::TriggerSmartContract as i32,
            m.encode_to_vec(),
        );
        let d = decode_contract(&c).unwrap();
        match d {
            DecodedContract::TriggerSmartContract { contract_address, data, .. } => {
                assert_eq!(contract_address, addr(0x44));
                assert_eq!(&data[..4], &[0xa9, 0x05, 0x9c, 0xbb]);
            }
            other => panic!("expected TriggerSmartContract, got {other:?}"),
        }
    }

    #[test]
    fn decodes_transfer_asset_payload() {
        let m = protocol::TransferAssetContract {
            asset_name: b"1000001".to_vec(),
            owner_address: addr(0x55).0.to_vec(),
            to_address: addr(0x66).0.to_vec(),
            amount: 42,
        };
        let c = build_contract(
            WireContractType::TransferAssetContract as i32,
            m.encode_to_vec(),
        );
        let d = decode_contract(&c).unwrap();
        match d {
            DecodedContract::TransferAsset { asset_name, amount, .. } => {
                assert_eq!(asset_name, b"1000001");
                assert_eq!(amount, 42);
            }
            other => panic!("expected TransferAsset, got {other:?}"),
        }
    }

    #[test]
    fn create_smart_contract_with_empty_address_yields_none() {
        let inner = protocol::SmartContract {
            origin_address: addr(0x77).0.to_vec(),
            // contract_address omitted (empty) — fresh deploy.
            contract_address: vec![],
            abi: None,
            bytecode: vec![0x60, 0x80, 0x60, 0x40],
            call_value: 0,
            consume_user_resource_percent: 100,
            name: "TestToken".into(),
            origin_energy_limit: 1_000_000,
            code_hash: vec![],
            trx_hash: vec![],
            version: 0,
        };
        let m = protocol::CreateSmartContract {
            owner_address: addr(0x77).0.to_vec(),
            new_contract: Some(inner),
            call_token_value: 0,
            token_id: 0,
        };
        let c = build_contract(
            WireContractType::CreateSmartContract as i32,
            m.encode_to_vec(),
        );
        let d = decode_contract(&c).unwrap();
        match d {
            DecodedContract::CreateSmartContract {
                contract_address,
                name,
                consume_user_resource_percent,
                ..
            } => {
                assert!(contract_address.is_none());
                assert_eq!(name, "TestToken");
                assert_eq!(consume_user_resource_percent, 100);
            }
            other => panic!("expected CreateSmartContract, got {other:?}"),
        }
    }

    #[test]
    fn unrecognized_high_volume_type_falls_through_to_other() {
        // FreezeBalanceV2 isn't one of the four; should land in Other
        // with the raw bytes preserved.
        let raw_payload = vec![0x01, 0x02, 0x03];
        let c = build_contract(
            WireContractType::FreezeBalanceV2Contract as i32,
            raw_payload.clone(),
        );
        let d = decode_contract(&c).unwrap();
        match d {
            DecodedContract::Other { kind, raw } => {
                assert_eq!(kind, ContractKind::FreezeBalanceV2);
                assert_eq!(raw, raw_payload);
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn forward_compat_unknown_tag_lands_in_other() {
        // Tag 9999 isn't in our vendored enum; should still decode.
        let c = build_contract(9999, vec![0xff]);
        let d = decode_contract(&c).unwrap();
        match d {
            DecodedContract::Other { kind, raw } => {
                assert_eq!(kind, ContractKind::Other(9999));
                assert_eq!(raw, vec![0xff]);
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn kind_method_matches_variant() {
        let t = DecodedContract::Transfer {
            owner: addr(1),
            to: addr(2),
            amount_sun: 0,
        };
        assert_eq!(t.kind(), ContractKind::Transfer);
        let o = DecodedContract::Other {
            kind: ContractKind::WitnessCreate,
            raw: vec![],
        };
        assert_eq!(o.kind(), ContractKind::WitnessCreate);
    }
}
