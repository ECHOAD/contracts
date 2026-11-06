use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::{Timestamp, Uint128};
use cw721::Cw721ReceiveMsg;

use crate::state::{Campaign, ClaimRecord};

#[cw_serde]
pub struct InstantiateMsg {
    /// Defaults to the instantiator.
    pub owner: Option<String>,
    pub distributor: String,
    pub fee_collector: String,
    pub denom: String,
    pub fee_bps: u16,
    pub min_fee: Uint128,
    pub nft_fee: Uint128,
    pub max_reservation_secs: u64,
    pub max_claim_window_secs: u64,
    pub max_batch: u32,
}

#[cw_serde]
pub enum RewardInput {
    Native { amount_per_claim: Uint128 },
    Nft { collection: String },
}

/// One kind of prize of the event.
#[cw_serde]
pub struct TierInput {
    /// 1 to 32 characters from [A-Za-z0-9_-], unique inside the campaign.
    pub tier_id: String,
    pub reward: RewardInput,
    pub max_claims: u32,
    /// Claims one address can receive from this tier (1 to `max_claims`).
    pub per_address_cap: u32,
}

#[cw_serde]
pub struct PayoutItem {
    /// Backend claim id. Single-use inside its campaign.
    pub claim_id: String,
    pub tier_id: String,
    pub recipient: String,
    /// Required for NFT tiers, forbidden for native ones.
    pub token_id: Option<String>,
}

#[cw_serde]
pub enum ExecuteMsg {
    /// Host creates the event's campaign. The same message deposits, for every native tier,
    /// `max_claims * (amount_per_claim + fee)` and, for every NFT tier, `max_claims * nft_fee`.
    /// The NFTs follow with `SendNft`.
    CreateCampaign {
        campaign_id: String,
        tiers: Vec<TierInput>,
        /// Optional cap on claims per address across all tiers.
        per_address_cap: Option<u32>,
        claim_end: Timestamp,
        reservation_end: Timestamp,
    },
    /// cw721 hook: the host sends a reward NFT with `SendNft` and
    /// `msg = {"campaign_id": "...", "tier_id": "..."}`.
    ReceiveNft(Cw721ReceiveMsg),
    /// Distributor pays a batch of authorized claims of one host's campaign.
    Payout {
        host: String,
        campaign_id: String,
        items: Vec<PayoutItem>,
    },
    /// After `reservation_end`, or once cancelled. The first call returns the unspent
    /// rewards and fees, alone, so no NFT transfer can block them. Later calls return up to
    /// `max_batch` NFTs each, from every tier or only from `tier_id`, so a collection that
    /// refuses transfers cannot block the others. Anyone can call it; funds only go to the host.
    Refund {
        host: String,
        campaign_id: String,
        tier_id: Option<String>,
    },
    /// Host cancels its campaign while some NFT tier is incomplete. Returns the unspent
    /// rewards and fees at once; the NFTs follow with `Refund`.
    CancelUnfunded { campaign_id: String },
    /// Owner updates platform settings. Funded campaigns keep their fee snapshot.
    UpdateConfig {
        owner: Option<String>,
        distributor: Option<String>,
        fee_collector: Option<String>,
        fee_bps: Option<u16>,
        min_fee: Option<Uint128>,
        nft_fee: Option<Uint128>,
        max_reservation_secs: Option<u64>,
        max_claim_window_secs: Option<u64>,
        max_batch: Option<u32>,
    },
}

/// Payload of `SendNft` into the vault. The campaign is the sender's own: only its host
/// can deposit into it.
#[cw_serde]
pub struct NftDepositMsg {
    pub campaign_id: String,
    pub tier_id: String,
}

#[cw_serde]
pub struct MigrateMsg {}

#[cw_serde]
#[derive(QueryResponses)]
pub enum QueryMsg {
    #[returns(crate::state::Config)]
    Config {},
    #[returns(CampaignResponse)]
    Campaign { host: String, campaign_id: String },
    /// One host's campaigns, in campaign id order.
    #[returns(CampaignsResponse)]
    Campaigns {
        host: String,
        start_after: Option<String>,
        limit: Option<u32>,
    },
    /// Lets the backend confirm a payout from state, without looking the tx up by hash.
    #[returns(ClaimResponse)]
    Claim {
        host: String,
        campaign_id: String,
        claim_id: String,
    },
    #[returns(AddressClaimsResponse)]
    AddressClaims {
        host: String,
        campaign_id: String,
        address: String,
    },
    #[returns(CampaignNftsResponse)]
    CampaignNfts {
        host: String,
        campaign_id: String,
        tier_id: String,
        start_after: Option<String>,
        limit: Option<u32>,
    },
}

#[cw_serde]
pub enum CampaignStatus {
    /// Some NFT tier is still waiting for deposits.
    Funding,
    /// Funded and inside the claim window.
    Open,
    /// Claim window over; reserved claims can still be paid.
    Reservation,
    /// Reservation period over; waiting for the refund.
    Ended,
    Cancelled,
    Settled,
}

#[cw_serde]
pub struct TierSummary {
    pub tier_id: String,
    pub funded: bool,
    pub remaining_claims: u32,
    /// Native reward still held for this tier (zero for NFT tiers).
    pub remaining_reward: Uint128,
    pub remaining_fee: Uint128,
}

#[cw_serde]
pub struct CampaignResponse {
    pub campaign: Campaign,
    pub status: CampaignStatus,
    pub remaining_claims: u32,
    /// Native reward still held for the whole campaign.
    pub remaining_reward: Uint128,
    /// Fees still held for the whole campaign.
    pub remaining_fee: Uint128,
    pub tiers: Vec<TierSummary>,
}

#[cw_serde]
pub struct CampaignsResponse {
    pub campaigns: Vec<CampaignResponse>,
}

#[cw_serde]
pub struct ClaimResponse {
    pub claim: Option<ClaimRecord>,
}

#[cw_serde]
pub struct TierCount {
    pub tier_id: String,
    pub count: u32,
}

#[cw_serde]
pub struct AddressClaimsResponse {
    /// Claims paid to the address across all tiers.
    pub total: u32,
    pub tiers: Vec<TierCount>,
}

#[cw_serde]
pub struct CampaignNftsResponse {
    pub token_ids: Vec<String>,
}
