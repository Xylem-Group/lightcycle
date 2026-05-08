//! Transaction-side codec: a friendlier `ContractKind` enum mapped from
//! java-tron's wire `ContractType`, plus the `DecodedTransaction` struct
//! that the relayer + downstream consumers see.

use lightcycle_proto::tron::protocol::transaction::contract::ContractType as WireContractType;
use lightcycle_types::TxHash;

use crate::error::{CodecError, Result};

/// Decoded transaction shape exposed to consumers. Mirrors the wire
/// `Transaction` but flattens the `raw_data` envelope and strips fields
/// (`provider`, `permission_id`, …) that no consumer in the lightcycle
/// surface uses today. Restore them here when a real consumer asks.
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
    /// One contract per element. Most TRON txs have a single contract;
    /// we keep the `Vec` in case multi-contract txs ever appear.
    pub contracts: Vec<ContractKind>,
    /// Witness/SR/multisig signatures. Length is normally 1.
    pub signatures: Vec<Vec<u8>>,
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

/// Decode a single wire `Transaction` into our friendlier shape. Pure;
/// no signature verification, no contract-payload `Any`-unwrapping yet.
pub(crate) fn decode_transaction(
    tx: &lightcycle_proto::tron::protocol::Transaction,
) -> Result<DecodedTransaction> {
    use prost::Message;
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
        .map(|c| ContractKind::from_wire(c.r#type))
        .collect();

    Ok(DecodedTransaction {
        hash: TxHash(hash_arr),
        timestamp_ms: raw.timestamp,
        expiration_ms: raw.expiration,
        fee_limit_sun: raw.fee_limit,
        contracts,
        signatures: tx.signature.clone(),
    })
}
