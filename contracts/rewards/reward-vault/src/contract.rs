use std::collections::{BTreeMap, BTreeSet};

#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    from_json, to_json_binary, Addr, BankMsg, Binary, Coin, CosmosMsg, Deps, DepsMut, Empty, Env,
    MessageInfo, Order, Response, StdResult, Storage, Timestamp, Uint128, WasmMsg,
};
use cw2::{get_contract_version, set_contract_version};
use cw721::{Cw721ExecuteMsg, Cw721ReceiveMsg};
use cw_storage_plus::Bound;
use cw_utils::{must_pay, nonpayable};

use crate::error::ContractError;
use crate::msg::{
    AddressClaimsResponse, CampaignNftsResponse, CampaignResponse, CampaignStatus,
    CampaignsResponse, ClaimResponse, ExecuteMsg, InstantiateMsg, MigrateMsg, NftDepositMsg,
    PayoutItem, QueryMsg, RewardInput, TierCount, TierInput, TierSummary,
};
use crate::state::{
    campaign_key, Campaign, ClaimRecord, Config, Reward, Tier, ADDRESS_CLAIMS, CAMPAIGNS,
    CAMPAIGN_NFTS, CLAIMS, CONFIG, TIER_ADDRESS_CLAIMS,
};

const CONTRACT_NAME: &str = "crates.io:reward-vault";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

const BPS_DENOMINATOR: u128 = 10_000;
const MAX_FEE_BPS: u16 = 10_000;
const MAX_BATCH_LIMIT: u32 = 100;
const MAX_TIERS: usize = 20;
const MAX_CAMPAIGN_ID_LEN: usize = 64;
const MAX_TIER_ID_LEN: usize = 32;
const MAX_CLAIM_ID_LEN: usize = 128;
const DEFAULT_QUERY_LIMIT: u32 = 30;
const MAX_QUERY_LIMIT: u32 = 100;

// ---------------------------------------------------------------------------------------------
// Instantiate
// ---------------------------------------------------------------------------------------------

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    nonpayable(&info)?;
    let owner = match msg.owner {
        Some(owner) => deps.api.addr_validate(&owner)?,
        None => info.sender,
    };
    let config = Config {
        owner,
        distributor: deps.api.addr_validate(&msg.distributor)?,
        fee_collector: deps.api.addr_validate(&msg.fee_collector)?,
        denom: msg.denom,
        fee_bps: msg.fee_bps,
        min_fee: msg.min_fee,
        nft_fee: msg.nft_fee,
        max_reservation_secs: msg.max_reservation_secs,
        max_claim_window_secs: msg.max_claim_window_secs,
        max_batch: msg.max_batch,
    };
    validate_config(&config)?;

    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new()
        .add_attribute("action", "instantiate")
        .add_attribute("owner", config.owner)
        .add_attribute("distributor", config.distributor)
        .add_attribute("fee_collector", config.fee_collector))
}

fn validate_config(config: &Config) -> Result<(), ContractError> {
    let fail = |reason: &str| {
        Err(ContractError::InvalidConfig {
            reason: reason.to_string(),
        })
    };
    if config.denom.trim().is_empty() {
        return fail("denom is empty");
    }
    if config.fee_bps > MAX_FEE_BPS {
        return fail("fee_bps cannot exceed 10000");
    }
    if config.max_reservation_secs == 0 {
        return fail("max_reservation_secs must be positive");
    }
    if config.max_claim_window_secs == 0 {
        return fail("max_claim_window_secs must be positive");
    }
    if config.max_batch == 0 || config.max_batch > MAX_BATCH_LIMIT {
        return fail("max_batch must be between 1 and 100");
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Execute
// ---------------------------------------------------------------------------------------------

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::CreateCampaign {
            campaign_id,
            tiers,
            per_address_cap,
            claim_end,
            reservation_end,
        } => execute_create_campaign(
            deps,
            env,
            info,
            campaign_id,
            tiers,
            per_address_cap,
            claim_end,
            reservation_end,
        ),
        ExecuteMsg::ReceiveNft(wrapper) => execute_receive_nft(deps, env, info, wrapper),
        ExecuteMsg::Payout {
            host,
            campaign_id,
            items,
        } => execute_payout(deps, env, info, host, campaign_id, items),
        ExecuteMsg::Refund {
            host,
            campaign_id,
            tier_id,
        } => execute_refund(deps, env, info, host, campaign_id, tier_id),
        ExecuteMsg::CancelUnfunded { campaign_id } => {
            execute_cancel_unfunded(deps, info, campaign_id)
        }
        ExecuteMsg::UpdateConfig {
            owner,
            distributor,
            fee_collector,
            fee_bps,
            min_fee,
            nft_fee,
            max_reservation_secs,
            max_claim_window_secs,
            max_batch,
        } => execute_update_config(
            deps,
            info,
            ConfigUpdate {
                owner,
                distributor,
                fee_collector,
                fee_bps,
                min_fee,
                nft_fee,
                max_reservation_secs,
                max_claim_window_secs,
                max_batch,
            },
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_create_campaign(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    campaign_id: String,
    tier_inputs: Vec<TierInput>,
    per_address_cap: Option<u32>,
    claim_end: Timestamp,
    reservation_end: Timestamp,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    if !is_valid_id(&campaign_id, MAX_CAMPAIGN_ID_LEN) {
        return Err(ContractError::InvalidCampaignId {});
    }
    let key = campaign_key(&info.sender, &campaign_id);
    if CAMPAIGNS.has(deps.storage, &key) {
        return Err(ContractError::CampaignExists { campaign_id });
    }
    if tier_inputs.is_empty() || tier_inputs.len() > MAX_TIERS {
        return Err(invalid_campaign("a campaign needs between 1 and 20 tiers"));
    }
    if per_address_cap == Some(0) {
        return Err(invalid_campaign("per_address_cap must be positive when set"));
    }
    if claim_end <= env.block.time {
        return Err(invalid_campaign("claim_end must be in the future"));
    }
    if claim_end.seconds() - env.block.time.seconds() > config.max_claim_window_secs {
        return Err(invalid_campaign("claim window exceeds the platform maximum"));
    }
    if reservation_end < claim_end {
        return Err(invalid_campaign("reservation_end cannot be before claim_end"));
    }
    if reservation_end.seconds() - claim_end.seconds() > config.max_reservation_secs {
        return Err(invalid_campaign(
            "reservation period exceeds the platform maximum",
        ));
    }

    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut tiers: Vec<Tier> = Vec::with_capacity(tier_inputs.len());
    let mut required_deposit = Uint128::zero();
    for input in tier_inputs {
        let (tier, tier_deposit) = build_tier(deps.as_ref(), &config, input, &mut seen)?;
        required_deposit = required_deposit.checked_add(tier_deposit)?;
        tiers.push(tier);
    }
    check_deposit(&info, &config.denom, required_deposit)?;

    let tier_count = tiers.len();
    let campaign = Campaign {
        id: campaign_id.clone(),
        host: info.sender.clone(),
        denom: config.denom,
        tiers,
        per_address_cap,
        claim_end,
        reservation_end,
        leftover_refunded: false,
        cancelled: false,
        settled: false,
    };
    CAMPAIGNS.save(deps.storage, &key, &campaign)?;

    Ok(Response::new()
        .add_attribute("action", "create_campaign")
        .add_attribute("campaign_id", campaign_id)
        .add_attribute("host", info.sender)
        .add_attribute("tiers", tier_count.to_string())
        .add_attribute("max_claims", campaign.remaining_claims().to_string())
        .add_attribute("deposit", required_deposit))
}

/// Validates one tier and returns it with the deposit it needs.
fn build_tier(
    deps: Deps,
    config: &Config,
    input: TierInput,
    seen: &mut BTreeSet<String>,
) -> Result<(Tier, Uint128), ContractError> {
    let id = input.tier_id;
    let fail = |reason: &str| ContractError::InvalidTier {
        reason: format!("{id}: {reason}"),
    };
    if !is_valid_id(&id, MAX_TIER_ID_LEN) {
        return Err(fail(
            "tier_id must have 1 to 32 characters from [A-Za-z0-9_-]",
        ));
    }
    if !seen.insert(id.clone()) {
        return Err(fail("repeats"));
    }
    if input.max_claims == 0 {
        return Err(fail("max_claims must be positive"));
    }
    if input.per_address_cap == 0 || input.per_address_cap > input.max_claims {
        return Err(fail("per_address_cap must be between 1 and max_claims"));
    }

    let claims = Uint128::from(input.max_claims);
    let (reward, fee, deposit) = match input.reward {
        RewardInput::Native { amount_per_claim } => {
            if amount_per_claim.is_zero() {
                return Err(fail("amount_per_claim must be positive"));
            }
            let fee = native_fee(config, amount_per_claim);
            let deposit = claims.checked_mul(amount_per_claim.checked_add(fee)?)?;
            (Reward::Native { amount_per_claim }, fee, deposit)
        }
        RewardInput::Nft { collection } => {
            let collection = deps.api.addr_validate(&collection)?;
            // A wallet address could call `receive_nft` itself and fake every deposit.
            if deps.querier.query_wasm_contract_info(&collection).is_err() {
                return Err(fail("collection is not a contract"));
            }
            let fee = config.nft_fee;
            (Reward::Nft { collection }, fee, claims.checked_mul(fee)?)
        }
    };

    let tier = Tier {
        id,
        reward,
        max_claims: input.max_claims,
        per_address_cap: input.per_address_cap,
        fee_per_claim: fee,
        nfts_deposited: 0,
        claims_paid: 0,
    };
    Ok((tier, deposit))
}

/// Percentage of the reward, never below the platform floor.
fn native_fee(config: &Config, amount_per_claim: Uint128) -> Uint128 {
    amount_per_claim
        .multiply_ratio(u128::from(config.fee_bps), BPS_DENOMINATOR)
        .max(config.min_fee)
}

/// The deposit has to match exactly: a campaign is either fully funded or not created.
fn check_deposit(info: &MessageInfo, denom: &str, required: Uint128) -> Result<(), ContractError> {
    if required.is_zero() {
        nonpayable(info)?;
        return Ok(());
    }
    let received = must_pay(info, denom)?;
    if received != required {
        return Err(ContractError::WrongDeposit {
            expected: required.to_string(),
            received: received.to_string(),
            denom: denom.to_string(),
        });
    }
    Ok(())
}

fn execute_receive_nft(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    wrapper: Cw721ReceiveMsg,
) -> Result<Response, ContractError> {
    nonpayable(&info)?;
    let deposit: NftDepositMsg = from_json(&wrapper.msg)?;
    // The depositor's own campaign: only its host can fund it.
    let host = deps.api.addr_validate(&wrapper.sender)?;
    let key = campaign_key(&host, &deposit.campaign_id);
    let mut campaign = load_campaign(deps.storage, &key, &deposit.campaign_id)?;
    let index = campaign
        .tier_index(&deposit.tier_id)
        .ok_or_else(|| ContractError::UnknownTier {
            tier_id: deposit.tier_id.clone(),
        })?;
    if campaign.cancelled || campaign.settled || env.block.time >= campaign.claim_end {
        return Err(ContractError::NftDepositClosed {
            campaign_id: campaign.id,
        });
    }

    let tier = &campaign.tiers[index];
    let collection = match &tier.reward {
        Reward::Nft { collection } => collection.clone(),
        Reward::Native { .. } => {
            return Err(ContractError::InvalidTier {
                reason: format!("{}: this tier does not take NFTs", tier.id),
            })
        }
    };
    // Only the tier's collection can call this hook, so the vault really owns the token
    // once we get here.
    if info.sender != collection {
        return Err(ContractError::WrongCollection {
            collection: info.sender.to_string(),
            tier_id: tier.id.clone(),
        });
    }
    if tier.is_funded() {
        return Err(ContractError::TierFull {
            tier_id: tier.id.clone(),
        });
    }
    let nft_key = (
        key.as_str(),
        deposit.tier_id.as_str(),
        wrapper.token_id.as_str(),
    );
    if CAMPAIGN_NFTS.has(deps.storage, nft_key) {
        return Err(ContractError::InvalidTier {
            reason: format!("{}: token already deposited", deposit.tier_id),
        });
    }
    CAMPAIGN_NFTS.save(deps.storage, nft_key, &Empty {})?;

    campaign.tiers[index].nfts_deposited += 1;
    CAMPAIGNS.save(deps.storage, &key, &campaign)?;

    Ok(Response::new()
        .add_attribute("action", "receive_nft")
        .add_attribute("host", host)
        .add_attribute("campaign_id", &campaign.id)
        .add_attribute("tier_id", deposit.tier_id)
        .add_attribute("token_id", wrapper.token_id)
        .add_attribute(
            "nfts_deposited",
            campaign.tiers[index].nfts_deposited.to_string(),
        )
        .add_attribute("funded", campaign.is_funded().to_string()))
}

fn execute_payout(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    host: String,
    campaign_id: String,
    items: Vec<PayoutItem>,
) -> Result<Response, ContractError> {
    nonpayable(&info)?;
    let config = CONFIG.load(deps.storage)?;
    if info.sender != config.distributor {
        return Err(ContractError::Unauthorized {});
    }
    let host = deps.api.addr_validate(&host)?;
    let key = campaign_key(&host, &campaign_id);
    let mut campaign = load_campaign(deps.storage, &key, &campaign_id)?;
    if campaign.cancelled || campaign.settled || env.block.time >= campaign.reservation_end {
        return Err(ContractError::PayoutClosed { campaign_id });
    }
    if !campaign.is_funded() {
        return Err(ContractError::NotFunded { campaign_id });
    }
    if items.is_empty() || items.len() > config.max_batch as usize {
        return Err(ContractError::InvalidBatchSize {
            max: config.max_batch,
        });
    }

    let mut seen: BTreeSet<String> = BTreeSet::new();
    // Native rewards are merged per recipient: one bank send each, in a deterministic order.
    let mut native_out: BTreeMap<Addr, Uint128> = BTreeMap::new();
    let mut nft_msgs: Vec<CosmosMsg> = vec![];
    let mut fee_total = Uint128::zero();
    let mut paid: u32 = 0;
    let mut skipped: u32 = 0;

    for item in items {
        if item.claim_id.is_empty() || item.claim_id.len() > MAX_CLAIM_ID_LEN {
            return Err(ContractError::InvalidPayoutItem {
                claim_id: item.claim_id,
                reason: "claim_id must have 1 to 128 characters".to_string(),
            });
        }
        if !seen.insert(item.claim_id.clone()) {
            return Err(ContractError::DuplicateClaimInBatch {
                claim_id: item.claim_id,
            });
        }
        let Some(index) = campaign.tier_index(&item.tier_id) else {
            return Err(ContractError::InvalidPayoutItem {
                reason: format!("unknown tier {}", item.tier_id),
                claim_id: item.claim_id,
            });
        };
        let recipient = deps.api.addr_validate(&item.recipient)?;
        // Paying the vault itself would leave the funds owned by no campaign.
        if recipient == env.contract.address {
            return Err(ContractError::InvalidPayoutItem {
                claim_id: item.claim_id,
                reason: "recipient cannot be the vault".to_string(),
            });
        }

        // A retried batch is a no-op for claims already paid to the same recipient, so the
        // distributor can resend after an unconfirmed broadcast without paying twice.
        if let Some(existing) = CLAIMS.may_load(deps.storage, (&key, &item.claim_id))? {
            if existing.recipient == recipient
                && existing.tier_id == item.tier_id
                && existing.token_id == item.token_id
            {
                skipped += 1;
                continue;
            }
            return Err(ContractError::ClaimConflict {
                claim_id: item.claim_id,
            });
        }

        let tier = &campaign.tiers[index];
        if tier.claims_paid >= tier.max_claims {
            return Err(ContractError::TierExhausted {
                tier_id: tier.id.clone(),
            });
        }
        let tier_count = TIER_ADDRESS_CLAIMS
            .may_load(deps.storage, (&key, &tier.id, &recipient))?
            .unwrap_or(0);
        if tier_count >= tier.per_address_cap {
            return Err(ContractError::AddressCapReached {
                recipient: recipient.to_string(),
                tier_id: tier.id.clone(),
            });
        }
        let event_count = ADDRESS_CLAIMS
            .may_load(deps.storage, (&key, &recipient))?
            .unwrap_or(0);
        if let Some(cap) = campaign.per_address_cap {
            if event_count >= cap {
                return Err(ContractError::EventCapReached {
                    recipient: recipient.to_string(),
                    campaign_id: campaign.id,
                });
            }
        }

        let fee = tier.fee_per_claim;
        let (amount, token_id) = match &tier.reward {
            Reward::Native { amount_per_claim } => {
                if item.token_id.is_some() {
                    return Err(ContractError::InvalidPayoutItem {
                        claim_id: item.claim_id,
                        reason: "token_id is not allowed in a native tier".to_string(),
                    });
                }
                let total = native_out.entry(recipient.clone()).or_default();
                *total = total.checked_add(*amount_per_claim)?;
                (*amount_per_claim, None)
            }
            Reward::Nft { collection } => {
                let Some(token_id) = item.token_id.clone() else {
                    return Err(ContractError::InvalidPayoutItem {
                        claim_id: item.claim_id,
                        reason: "token_id is required in an NFT tier".to_string(),
                    });
                };
                let nft_key = (key.as_str(), tier.id.as_str(), token_id.as_str());
                if !CAMPAIGN_NFTS.has(deps.storage, nft_key) {
                    return Err(ContractError::TokenNotAvailable {
                        token_id,
                        tier_id: tier.id.clone(),
                    });
                }
                CAMPAIGN_NFTS.remove(deps.storage, nft_key);
                nft_msgs.push(transfer_nft_msg(collection, &recipient, &token_id)?);
                (Uint128::zero(), Some(token_id))
            }
        };

        fee_total = fee_total.checked_add(fee)?;
        CLAIMS.save(
            deps.storage,
            (&key, &item.claim_id),
            &ClaimRecord {
                tier_id: item.tier_id.clone(),
                recipient: recipient.clone(),
                amount,
                token_id,
                fee,
                paid_at: env.block.time,
            },
        )?;
        TIER_ADDRESS_CLAIMS.save(
            deps.storage,
            (&key, &item.tier_id, &recipient),
            &(tier_count + 1),
        )?;
        ADDRESS_CLAIMS.save(deps.storage, (&key, &recipient), &(event_count + 1))?;
        campaign.tiers[index].claims_paid += 1;
        paid += 1;
    }

    let mut msgs: Vec<CosmosMsg> = native_out
        .into_iter()
        .map(|(recipient, amount)| bank_send(&recipient, &campaign.denom, amount))
        .collect();
    msgs.extend(nft_msgs);
    if !fee_total.is_zero() {
        msgs.push(bank_send(&config.fee_collector, &campaign.denom, fee_total));
    }
    CAMPAIGNS.save(deps.storage, &key, &campaign)?;

    Ok(Response::new()
        .add_messages(msgs)
        .add_attribute("action", "payout")
        .add_attribute("host", host)
        .add_attribute("campaign_id", &campaign.id)
        .add_attribute("paid", paid.to_string())
        .add_attribute("skipped", skipped.to_string())
        .add_attribute("fee_total", fee_total)
        .add_attribute("claims_paid", campaign.claims_paid().to_string()))
}

fn execute_refund(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    host: String,
    campaign_id: String,
    tier_id: Option<String>,
) -> Result<Response, ContractError> {
    nonpayable(&info)?;
    let config = CONFIG.load(deps.storage)?;
    let host = deps.api.addr_validate(&host)?;
    let key = campaign_key(&host, &campaign_id);
    let mut campaign = load_campaign(deps.storage, &key, &campaign_id)?;
    if campaign.settled {
        return Err(ContractError::AlreadySettled { campaign_id });
    }
    if !campaign.cancelled && env.block.time < campaign.reservation_end {
        return Err(ContractError::RefundTooEarly { campaign_id });
    }
    if let Some(tier_id) = &tier_id {
        if campaign.tier_index(tier_id).is_none() {
            return Err(ContractError::UnknownTier {
                tier_id: tier_id.clone(),
            });
        }
    }

    // The unspent PASG travels alone, never next to an NFT transfer that could fail.
    let (msgs, leftover, nfts_returned) = if campaign.leftover_refunded {
        let (msgs, returned) = return_nfts(
            deps.storage,
            &key,
            &campaign,
            tier_id.as_deref(),
            config.max_batch,
        )?;
        (msgs, Uint128::zero(), returned)
    } else {
        let (msgs, leftover) = refund_leftover(&mut campaign)?;
        (msgs, leftover, 0)
    };
    campaign.settled = campaign.leftover_refunded && !holds_nfts(deps.storage, &key);
    CAMPAIGNS.save(deps.storage, &key, &campaign)?;

    Ok(Response::new()
        .add_messages(msgs)
        .add_attribute("action", "refund")
        .add_attribute("host", host)
        .add_attribute("campaign_id", &campaign.id)
        .add_attribute("leftover", leftover)
        .add_attribute("nfts_returned", nfts_returned.to_string())
        .add_attribute("settled", campaign.settled.to_string()))
}

fn execute_cancel_unfunded(
    deps: DepsMut,
    info: MessageInfo,
    campaign_id: String,
) -> Result<Response, ContractError> {
    nonpayable(&info)?;
    let key = campaign_key(&info.sender, &campaign_id);
    let mut campaign = load_campaign(deps.storage, &key, &campaign_id)?;
    if campaign.settled {
        return Err(ContractError::AlreadySettled { campaign_id });
    }
    if campaign.cancelled || campaign.is_funded() || campaign.claims_paid() > 0 {
        return Err(ContractError::CannotCancel { campaign_id });
    }

    campaign.cancelled = true;
    let (msgs, leftover) = refund_leftover(&mut campaign)?;
    campaign.settled = !holds_nfts(deps.storage, &key);
    CAMPAIGNS.save(deps.storage, &key, &campaign)?;

    Ok(Response::new()
        .add_messages(msgs)
        .add_attribute("action", "cancel_unfunded")
        .add_attribute("host", info.sender)
        .add_attribute("campaign_id", &campaign.id)
        .add_attribute("leftover", leftover)
        .add_attribute("settled", campaign.settled.to_string()))
}

/// Unspent rewards and fees of every tier, sent to the host once.
fn refund_leftover(campaign: &mut Campaign) -> Result<(Vec<CosmosMsg>, Uint128), ContractError> {
    let (reward_left, fee_left) = campaign.held_balances()?;
    let leftover = reward_left.checked_add(fee_left)?;
    campaign.leftover_refunded = true;
    let msgs = if leftover.is_zero() {
        vec![]
    } else {
        vec![bank_send(&campaign.host, &campaign.denom, leftover)]
    };
    Ok((msgs, leftover))
}

/// Up to `max_batch` NFTs back to the host, from every tier or from one, so a large
/// campaign never exceeds the block gas limit and a stuck collection can be skipped.
fn return_nfts(
    storage: &mut dyn Storage,
    key: &str,
    campaign: &Campaign,
    tier_id: Option<&str>,
    max_batch: u32,
) -> Result<(Vec<CosmosMsg>, u32), ContractError> {
    let page: Vec<(String, String)> = match tier_id {
        Some(tier_id) => CAMPAIGN_NFTS
            .prefix((key, tier_id))
            .keys(storage, None, None, Order::Ascending)
            .take(max_batch as usize)
            .map(|token| token.map(|token| (tier_id.to_string(), token)))
            .collect::<StdResult<_>>()?,
        None => CAMPAIGN_NFTS
            .sub_prefix(key)
            .keys(storage, None, None, Order::Ascending)
            .take(max_batch as usize)
            .collect::<StdResult<_>>()?,
    };
    let mut msgs: Vec<CosmosMsg> = vec![];
    for (tier, token_id) in &page {
        let collection = match campaign
            .tier_index(tier)
            .map(|index| &campaign.tiers[index].reward)
        {
            Some(Reward::Nft { collection }) => collection.clone(),
            _ => {
                return Err(ContractError::InvalidTier {
                    reason: format!("{tier}: holds an NFT but is not an NFT tier"),
                })
            }
        };
        CAMPAIGN_NFTS.remove(storage, (key, tier, token_id));
        msgs.push(transfer_nft_msg(&collection, &campaign.host, token_id)?);
    }
    Ok((msgs, page.len() as u32))
}

fn holds_nfts(storage: &dyn Storage, key: &str) -> bool {
    CAMPAIGN_NFTS
        .sub_prefix(key)
        .keys(storage, None, None, Order::Ascending)
        .next()
        .is_some()
}

struct ConfigUpdate {
    owner: Option<String>,
    distributor: Option<String>,
    fee_collector: Option<String>,
    fee_bps: Option<u16>,
    min_fee: Option<Uint128>,
    nft_fee: Option<Uint128>,
    max_reservation_secs: Option<u64>,
    max_claim_window_secs: Option<u64>,
    max_batch: Option<u32>,
}

fn execute_update_config(
    deps: DepsMut,
    info: MessageInfo,
    update: ConfigUpdate,
) -> Result<Response, ContractError> {
    nonpayable(&info)?;
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }
    if let Some(owner) = update.owner {
        config.owner = deps.api.addr_validate(&owner)?;
    }
    if let Some(distributor) = update.distributor {
        config.distributor = deps.api.addr_validate(&distributor)?;
    }
    if let Some(fee_collector) = update.fee_collector {
        config.fee_collector = deps.api.addr_validate(&fee_collector)?;
    }
    config.fee_bps = update.fee_bps.unwrap_or(config.fee_bps);
    config.min_fee = update.min_fee.unwrap_or(config.min_fee);
    config.nft_fee = update.nft_fee.unwrap_or(config.nft_fee);
    config.max_reservation_secs = update
        .max_reservation_secs
        .unwrap_or(config.max_reservation_secs);
    config.max_claim_window_secs = update
        .max_claim_window_secs
        .unwrap_or(config.max_claim_window_secs);
    config.max_batch = update.max_batch.unwrap_or(config.max_batch);
    validate_config(&config)?;
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new()
        .add_attribute("action", "update_config")
        .add_attribute("owner", config.owner)
        .add_attribute("distributor", config.distributor))
}

// ---------------------------------------------------------------------------------------------
// Migrate
// ---------------------------------------------------------------------------------------------

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(deps: DepsMut, _env: Env, _msg: MigrateMsg) -> Result<Response, ContractError> {
    let previous = get_contract_version(deps.storage)?;
    if previous.contract != CONTRACT_NAME {
        return Err(ContractError::CannotMigrate {
            previous_contract: previous.contract,
            previous_version: previous.version,
        });
    }
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    Ok(Response::new()
        .add_attribute("action", "migrate")
        .add_attribute("from_version", previous.version)
        .add_attribute("to_version", CONTRACT_VERSION))
}

// ---------------------------------------------------------------------------------------------
// Query
// ---------------------------------------------------------------------------------------------

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_json_binary(&CONFIG.load(deps.storage)?),
        QueryMsg::Campaign { host, campaign_id } => {
            let key = campaign_key(&deps.api.addr_validate(&host)?, &campaign_id);
            to_json_binary(&campaign_response(&env, CAMPAIGNS.load(deps.storage, &key)?)?)
        }
        QueryMsg::Campaigns {
            host,
            start_after,
            limit,
        } => {
            let host = deps.api.addr_validate(&host)?;
            let limit = query_limit(limit);
            // Keys of one host share the "<host>/" prefix; "<host>0" is the first key after it.
            let lower = match &start_after {
                Some(id) => campaign_key(&host, id),
                None => format!("{host}/"),
            };
            let upper = format!("{host}0");
            let campaigns = CAMPAIGNS
                .range(
                    deps.storage,
                    Some(Bound::exclusive(lower.as_str())),
                    Some(Bound::exclusive(upper.as_str())),
                    Order::Ascending,
                )
                .take(limit)
                .map(|item| item.and_then(|(_, campaign)| campaign_response(&env, campaign)))
                .collect::<StdResult<Vec<_>>>()?;
            to_json_binary(&CampaignsResponse { campaigns })
        }
        QueryMsg::Claim {
            host,
            campaign_id,
            claim_id,
        } => {
            let key = campaign_key(&deps.api.addr_validate(&host)?, &campaign_id);
            to_json_binary(&ClaimResponse {
                claim: CLAIMS.may_load(deps.storage, (&key, &claim_id))?,
            })
        }
        QueryMsg::AddressClaims {
            host,
            campaign_id,
            address,
        } => {
            let key = campaign_key(&deps.api.addr_validate(&host)?, &campaign_id);
            let address = deps.api.addr_validate(&address)?;
            let campaign = CAMPAIGNS.load(deps.storage, &key)?;
            let total = ADDRESS_CLAIMS
                .may_load(deps.storage, (&key, &address))?
                .unwrap_or(0);
            let tiers = campaign
                .tiers
                .iter()
                .map(|tier| {
                    let count = TIER_ADDRESS_CLAIMS
                        .may_load(deps.storage, (&key, &tier.id, &address))?
                        .unwrap_or(0);
                    Ok(TierCount {
                        tier_id: tier.id.clone(),
                        count,
                    })
                })
                .collect::<StdResult<Vec<_>>>()?;
            to_json_binary(&AddressClaimsResponse { total, tiers })
        }
        QueryMsg::CampaignNfts {
            host,
            campaign_id,
            tier_id,
            start_after,
            limit,
        } => {
            let key = campaign_key(&deps.api.addr_validate(&host)?, &campaign_id);
            let limit = query_limit(limit);
            let start = start_after.as_deref().map(Bound::exclusive);
            let token_ids = CAMPAIGN_NFTS
                .prefix((key.as_str(), tier_id.as_str()))
                .keys(deps.storage, start, None, Order::Ascending)
                .take(limit)
                .collect::<StdResult<Vec<_>>>()?;
            to_json_binary(&CampaignNftsResponse { token_ids })
        }
    }
}

fn campaign_response(env: &Env, campaign: Campaign) -> StdResult<CampaignResponse> {
    let refunded = campaign.leftover_refunded;
    let tiers = campaign
        .tiers
        .iter()
        .map(|tier| {
            let (remaining_reward, remaining_fee) = if refunded {
                (Uint128::zero(), Uint128::zero())
            } else {
                tier.held_balances()?
            };
            Ok(TierSummary {
                tier_id: tier.id.clone(),
                funded: tier.is_funded(),
                remaining_claims: tier.remaining_claims(),
                remaining_reward,
                remaining_fee,
            })
        })
        .collect::<StdResult<Vec<_>>>()?;
    let (remaining_reward, remaining_fee) = if refunded {
        (Uint128::zero(), Uint128::zero())
    } else {
        campaign.held_balances()?
    };
    Ok(CampaignResponse {
        status: campaign_status(env, &campaign),
        remaining_claims: campaign.remaining_claims(),
        remaining_reward,
        remaining_fee,
        tiers,
        campaign,
    })
}

fn campaign_status(env: &Env, campaign: &Campaign) -> CampaignStatus {
    let now = env.block.time;
    if campaign.settled {
        CampaignStatus::Settled
    } else if campaign.cancelled {
        CampaignStatus::Cancelled
    } else if now >= campaign.reservation_end {
        CampaignStatus::Ended
    } else if !campaign.is_funded() {
        CampaignStatus::Funding
    } else if now < campaign.claim_end {
        CampaignStatus::Open
    } else {
        CampaignStatus::Reservation
    }
}

fn query_limit(limit: Option<u32>) -> usize {
    limit.unwrap_or(DEFAULT_QUERY_LIMIT).min(MAX_QUERY_LIMIT) as usize
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn load_campaign(
    storage: &dyn Storage,
    key: &str,
    campaign_id: &str,
) -> Result<Campaign, ContractError> {
    CAMPAIGNS
        .may_load(storage, key)?
        .ok_or_else(|| ContractError::CampaignNotFound {
            campaign_id: campaign_id.to_string(),
        })
}

fn is_valid_id(id: &str, max_len: usize) -> bool {
    !id.is_empty()
        && id.len() <= max_len
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn invalid_campaign(reason: &str) -> ContractError {
    ContractError::InvalidCampaign {
        reason: reason.to_string(),
    }
}

fn bank_send(to: &Addr, denom: &str, amount: Uint128) -> CosmosMsg {
    BankMsg::Send {
        to_address: to.to_string(),
        amount: vec![Coin {
            denom: denom.to_string(),
            amount,
        }],
    }
    .into()
}

fn transfer_nft_msg(
    collection: &Addr,
    recipient: &Addr,
    token_id: &str,
) -> Result<CosmosMsg, ContractError> {
    Ok(WasmMsg::Execute {
        contract_addr: collection.to_string(),
        msg: to_json_binary(&Cw721ExecuteMsg::TransferNft {
            recipient: recipient.to_string(),
            token_id: token_id.to_string(),
        })?,
        funds: vec![],
    }
    .into())
}
