use cosmwasm_std::{OverflowError, StdError};
use cw_utils::PaymentError;
use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum ContractError {
    #[error("{0}")]
    Std(#[from] StdError),

    #[error("{0}")]
    Payment(#[from] PaymentError),

    #[error("{0}")]
    Overflow(#[from] OverflowError),

    #[error("unauthorized")]
    Unauthorized {},

    #[error("invalid configuration: {reason}")]
    InvalidConfig { reason: String },

    #[error("invalid campaign id: use 1 to 64 characters from [A-Za-z0-9_-]")]
    InvalidCampaignId {},

    #[error("campaign {campaign_id} already exists")]
    CampaignExists { campaign_id: String },

    #[error("campaign {campaign_id} not found")]
    CampaignNotFound { campaign_id: String },

    #[error("invalid campaign: {reason}")]
    InvalidCampaign { reason: String },

    #[error("invalid tier {reason}")]
    InvalidTier { reason: String },

    #[error("tier {tier_id} does not exist in this campaign")]
    UnknownTier { tier_id: String },

    #[error("wrong deposit: expected {expected}{denom}, received {received}{denom}")]
    WrongDeposit {
        expected: String,
        received: String,
        denom: String,
    },

    #[error("campaign {campaign_id} is not accepting NFT deposits")]
    NftDepositClosed { campaign_id: String },

    #[error("NFT from collection {collection} does not belong to tier {tier_id}")]
    WrongCollection { collection: String, tier_id: String },

    #[error("tier {tier_id} already holds all its NFTs")]
    TierFull { tier_id: String },

    #[error("campaign {campaign_id} is not fully funded")]
    NotFunded { campaign_id: String },

    #[error("campaign {campaign_id} no longer accepts payouts")]
    PayoutClosed { campaign_id: String },

    #[error("payout batch is empty or larger than {max}")]
    InvalidBatchSize { max: u32 },

    #[error("tier {tier_id} has no claims left")]
    TierExhausted { tier_id: String },

    #[error("claim {claim_id} was already paid to a different recipient")]
    ClaimConflict { claim_id: String },

    #[error("claim {claim_id} repeats inside the same batch")]
    DuplicateClaimInBatch { claim_id: String },

    #[error("recipient {recipient} reached the per-address cap of tier {tier_id}")]
    AddressCapReached { recipient: String, tier_id: String },

    #[error("recipient {recipient} reached the per-address cap of campaign {campaign_id}")]
    EventCapReached {
        recipient: String,
        campaign_id: String,
    },

    #[error("claim {claim_id}: {reason}")]
    InvalidPayoutItem { claim_id: String, reason: String },

    #[error("token {token_id} is not available in tier {tier_id}")]
    TokenNotAvailable { token_id: String, tier_id: String },

    #[error("campaign {campaign_id} cannot be refunded before its reservation period ends")]
    RefundTooEarly { campaign_id: String },

    #[error("campaign {campaign_id} is already settled")]
    AlreadySettled { campaign_id: String },

    #[error("campaign {campaign_id} cannot be cancelled: it is funded or has paid claims")]
    CannotCancel { campaign_id: String },

    #[error("cannot migrate from {previous_contract} {previous_version}")]
    CannotMigrate {
        previous_contract: String,
        previous_version: String,
    },
}
