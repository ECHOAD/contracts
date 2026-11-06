use cosmwasm_schema::cw_serde;
use cosmwasm_std::{Addr, Empty, StdResult, Timestamp, Uint128};
use cw_storage_plus::{Item, Map};

/// Platform settings. Fees are set here, never by the host, and each tier keeps a snapshot
/// of its fee so a later change cannot alter a campaign that is already funded.
#[cw_serde]
pub struct Config {
    /// Can change this config. Single key on testnet; the mainnet choice (on-chain
    /// multisig or no owner) is still open.
    pub owner: Addr,
    /// The only address allowed to execute payouts (Passage's distributor key).
    pub distributor: Addr,
    /// Receives the fee of every paid claim.
    pub fee_collector: Addr,
    /// Native denom of deposits, rewards and fees (`upasg` on passage-2).
    pub denom: String,
    /// Fee per PASG claim, in basis points of the reward.
    pub fee_bps: u16,
    /// Floor for the fee of a PASG claim, so the fee never drops below the gas it pays.
    pub min_fee: Uint128,
    /// Fixed fee per NFT claim, paid in `denom`.
    pub nft_fee: Uint128,
    /// Platform maximum for the reservation period that follows the claim window.
    pub max_reservation_secs: u64,
    /// Platform maximum between campaign creation and `claim_end`.
    pub max_claim_window_secs: u64,
    /// Maximum items per payout batch and NFTs returned per refund call.
    pub max_batch: u32,
}

/// Campaign ids are scoped to their host: two hosts can use the same id, and nobody can
/// take an id away from another host. `/` is outside the id alphabet, so keys never collide.
pub fn campaign_key(host: &Addr, campaign_id: &str) -> String {
    format!("{host}/{campaign_id}")
}

#[cw_serde]
pub enum Reward {
    /// A fixed amount of `denom` per claim.
    Native { amount_per_claim: Uint128 },
    /// One pre-minted NFT of `collection` per claim, deposited by the host.
    Nft { collection: Addr },
}

/// One kind of prize inside an event: a chest, a boss, a sword. Each event defines its own.
#[cw_serde]
pub struct Tier {
    pub id: String,
    pub reward: Reward,
    pub max_claims: u32,
    /// Claims one address can receive from this tier. Always set.
    pub per_address_cap: u32,
    /// Fee snapshot taken at creation.
    pub fee_per_claim: Uint128,
    pub nfts_deposited: u32,
    pub claims_paid: u32,
}

impl Tier {
    pub fn is_funded(&self) -> bool {
        match self.reward {
            Reward::Native { .. } => true,
            Reward::Nft { .. } => self.nfts_deposited == self.max_claims,
        }
    }

    pub fn remaining_claims(&self) -> u32 {
        self.max_claims - self.claims_paid
    }

    /// Native reward and fees the vault still holds for this tier, from its counters.
    pub fn held_balances(&self) -> StdResult<(Uint128, Uint128)> {
        let remaining = Uint128::from(self.remaining_claims());
        let reward = match &self.reward {
            Reward::Native { amount_per_claim } => remaining.checked_mul(*amount_per_claim)?,
            Reward::Nft { .. } => Uint128::zero(),
        };
        let fee = remaining.checked_mul(self.fee_per_claim)?;
        Ok((reward, fee))
    }
}

/// One event's rewards: several tiers funded together and settled together.
#[cw_serde]
pub struct Campaign {
    pub id: String,
    pub host: Addr,
    pub denom: String,
    pub tiers: Vec<Tier>,
    /// Optional cap on claims per address across every tier of the event.
    pub per_address_cap: Option<u32>,
    /// Payouts are authorized off-chain until here; NFT deposits close here too.
    pub claim_end: Timestamp,
    /// Reserved claims can still be paid until here. After it, anyone can trigger the refund.
    pub reservation_end: Timestamp,
    /// Unspent rewards and fees already returned to the host.
    pub leftover_refunded: bool,
    /// Host cancelled a campaign that never became fully funded.
    pub cancelled: bool,
    /// Nothing left in the vault for this campaign.
    pub settled: bool,
}

impl Campaign {
    /// Fail closed: no tier pays until every NFT tier holds all its NFTs.
    pub fn is_funded(&self) -> bool {
        self.tiers.iter().all(Tier::is_funded)
    }

    pub fn tier_index(&self, tier_id: &str) -> Option<usize> {
        self.tiers.iter().position(|tier| tier.id == tier_id)
    }

    pub fn remaining_claims(&self) -> u32 {
        self.tiers.iter().map(Tier::remaining_claims).sum()
    }

    pub fn claims_paid(&self) -> u32 {
        self.tiers.iter().map(|tier| tier.claims_paid).sum()
    }

    pub fn held_balances(&self) -> StdResult<(Uint128, Uint128)> {
        let mut reward = Uint128::zero();
        let mut fee = Uint128::zero();
        for tier in &self.tiers {
            let (tier_reward, tier_fee) = tier.held_balances()?;
            reward = reward.checked_add(tier_reward)?;
            fee = fee.checked_add(tier_fee)?;
        }
        Ok((reward, fee))
    }
}

/// One paid claim. Its presence is what makes a claim id single-use.
#[cw_serde]
pub struct ClaimRecord {
    pub tier_id: String,
    pub recipient: Addr,
    /// Native amount paid; zero for NFT claims.
    pub amount: Uint128,
    pub token_id: Option<String>,
    pub fee: Uint128,
    pub paid_at: Timestamp,
}

pub const CONFIG: Item<Config> = Item::new("config");
/// Every map below is keyed by `campaign_key(host, campaign_id)`.
pub const CAMPAIGNS: Map<&str, Campaign> = Map::new("campaigns");
/// (campaign key, claim_id) -> record
pub const CLAIMS: Map<(&str, &str), ClaimRecord> = Map::new("claims");
/// (campaign key, recipient) -> claims paid to that address across all tiers
pub const ADDRESS_CLAIMS: Map<(&str, &Addr), u32> = Map::new("address_claims");
/// (campaign key, tier_id, recipient) -> claims paid to that address from one tier
pub const TIER_ADDRESS_CLAIMS: Map<(&str, &str, &Addr), u32> = Map::new("tier_address_claims");
/// (campaign key, tier_id, token_id) for NFTs deposited and not yet paid or returned
pub const CAMPAIGN_NFTS: Map<(&str, &str, &str), Empty> = Map::new("campaign_nfts");
