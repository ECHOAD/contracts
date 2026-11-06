use cosmwasm_schema::cw_serde;
use cosmwasm_std::{
    coin, coins, to_json_binary, Addr, Binary, Deps, DepsMut, Empty, Env, MessageInfo, Response,
    StdError, StdResult, Timestamp, Uint128, WasmMsg,
};
use cw721::{Cw721ReceiveMsg, OwnerOfResponse};
use cw_multi_test::{App, AppResponse, Contract, ContractWrapper, Executor};
use cw_utils::PaymentError;

use crate::contract::{execute, instantiate, migrate, query};
use crate::error::ContractError;
use crate::msg::{
    AddressClaimsResponse, CampaignResponse, CampaignStatus, ClaimResponse, ExecuteMsg,
    InstantiateMsg, MigrateMsg, NftDepositMsg, PayoutItem, QueryMsg, RewardInput, TierCount,
    TierInput,
};

const DENOM: &str = "upasg";
const OWNER: &str = "owner";
const DISTRIBUTOR: &str = "distributor";
const FEE_COLLECTOR: &str = "feecollector";
const HOST: &str = "host";
const ALICE: &str = "alice";
const BOB: &str = "bob";
const CAROL: &str = "carol";
const STRANGER: &str = "stranger";

const START_BALANCE: u128 = 1_000_000_000;
const FEE_BPS: u16 = 500; // 5 %
const MIN_FEE: u128 = 100_000; // 0.1 PASG
const NFT_FEE: u128 = 50_000;
const DAY: u64 = 86_400;
const MAX_RESERVATION: u64 = 30 * DAY;
const MAX_CLAIM_WINDOW: u64 = 90 * DAY;

/// 2 PASG per claim; 5 % of it equals the floor, so the fee is 0.1 PASG.
const REWARD: u128 = 2_000_000;
const FEE: u128 = 100_000;
const MAIN: &str = "main";
const NFT: &str = "nft";

type Cw721Exec = cw721_base::ExecuteMsg<cw721_base::Extension, Empty>;
type Cw721Query = cw721_base::QueryMsg<Empty>;

fn vault_contract() -> Box<dyn Contract<Empty>> {
    Box::new(ContractWrapper::new(execute, instantiate, query).with_migrate(migrate))
}

fn cw721_contract() -> Box<dyn Contract<Empty>> {
    Box::new(ContractWrapper::new(
        cw721_base::entry::execute,
        cw721_base::entry::instantiate,
        cw721_base::entry::query,
    ))
}

// --- tier and payout builders ----------------------------------------------------------------

fn native_tier(id: &str, amount: u128, max_claims: u32, per_address_cap: u32) -> TierInput {
    TierInput {
        tier_id: id.to_string(),
        reward: RewardInput::Native {
            amount_per_claim: Uint128::new(amount),
        },
        max_claims,
        per_address_cap,
    }
}

fn nft_tier(id: &str, collection: &Addr, max_claims: u32, per_address_cap: u32) -> TierInput {
    TierInput {
        tier_id: id.to_string(),
        reward: RewardInput::Nft {
            collection: collection.to_string(),
        },
        max_claims,
        per_address_cap,
    }
}

/// Same formula as the contract, with the test config.
fn fee_for(amount: u128) -> u128 {
    (amount * u128::from(FEE_BPS) / 10_000).max(MIN_FEE)
}

fn deposit_for(tiers: &[TierInput]) -> u128 {
    tiers
        .iter()
        .map(|tier| {
            let claims = u128::from(tier.max_claims);
            match &tier.reward {
                RewardInput::Native { amount_per_claim } => {
                    let amount = amount_per_claim.u128();
                    claims * (amount + fee_for(amount))
                }
                RewardInput::Nft { .. } => claims * NFT_FEE,
            }
        })
        .sum()
}

fn item(claim_id: &str, tier_id: &str, recipient: &str) -> PayoutItem {
    PayoutItem {
        claim_id: claim_id.to_string(),
        tier_id: tier_id.to_string(),
        recipient: recipient.to_string(),
        token_id: None,
    }
}

fn nft_item(claim_id: &str, tier_id: &str, recipient: &str, token_id: &str) -> PayoutItem {
    PayoutItem {
        token_id: Some(token_id.to_string()),
        ..item(claim_id, tier_id, recipient)
    }
}

fn assert_err<T: std::fmt::Debug>(result: anyhow::Result<T>, expected: ContractError) {
    let err = result.unwrap_err();
    assert_eq!(
        err.downcast_ref::<ContractError>(),
        Some(&expected),
        "unexpected error: {err:?}"
    );
}

// --- suite -----------------------------------------------------------------------------------

struct Suite {
    app: App,
    vault: Addr,
    vault_code: u64,
}

impl Suite {
    fn new() -> Self {
        let mut app = App::new(|router, _api, storage| {
            for who in [HOST, STRANGER] {
                router
                    .bank
                    .init_balance(storage, &Addr::unchecked(who), coins(START_BALANCE, DENOM))
                    .unwrap();
            }
        });
        let vault_code = app.store_code(vault_contract());
        let vault = app
            .instantiate_contract(
                vault_code,
                Addr::unchecked(OWNER),
                &InstantiateMsg {
                    owner: None,
                    distributor: DISTRIBUTOR.to_string(),
                    fee_collector: FEE_COLLECTOR.to_string(),
                    denom: DENOM.to_string(),
                    fee_bps: FEE_BPS,
                    min_fee: Uint128::new(MIN_FEE),
                    nft_fee: Uint128::new(NFT_FEE),
                    max_reservation_secs: MAX_RESERVATION,
                    max_claim_window_secs: MAX_CLAIM_WINDOW,
                    max_batch: 50,
                },
                &[],
                "reward-vault",
                Some(OWNER.to_string()),
            )
            .unwrap();
        Suite {
            app,
            vault,
            vault_code,
        }
    }

    fn now(&self) -> Timestamp {
        self.app.block_info().time
    }

    fn advance(&mut self, secs: u64) {
        self.app.update_block(|block| {
            block.time = block.time.plus_seconds(secs);
            block.height += secs / 5 + 1;
        });
    }

    fn balance(&self, who: &str) -> u128 {
        self.app
            .wrap()
            .query_balance(who, DENOM)
            .unwrap()
            .amount
            .u128()
    }

    /// Claim window of one day, reservation until day eight.
    fn create(
        &mut self,
        id: &str,
        tiers: Vec<TierInput>,
        per_address_cap: Option<u32>,
        deposit: u128,
    ) -> anyhow::Result<AppResponse> {
        let now = self.now();
        let funds = if deposit == 0 {
            vec![]
        } else {
            coins(deposit, DENOM)
        };
        self.app.execute_contract(
            Addr::unchecked(HOST),
            self.vault.clone(),
            &ExecuteMsg::CreateCampaign {
                campaign_id: id.to_string(),
                tiers,
                per_address_cap,
                claim_end: now.plus_seconds(DAY),
                reservation_end: now.plus_seconds(8 * DAY),
            },
            &funds,
        )
    }

    fn create_funded(
        &mut self,
        id: &str,
        tiers: Vec<TierInput>,
        per_address_cap: Option<u32>,
    ) -> anyhow::Result<AppResponse> {
        let deposit = deposit_for(&tiers);
        self.create(id, tiers, per_address_cap, deposit)
    }

    /// One native tier, `main`, paying `REWARD`.
    fn create_native(
        &mut self,
        id: &str,
        max_claims: u32,
        per_address_cap: u32,
    ) -> anyhow::Result<AppResponse> {
        self.create_funded(
            id,
            vec![native_tier(MAIN, REWARD, max_claims, per_address_cap)],
            None,
        )
    }

    fn payout(
        &mut self,
        sender: &str,
        id: &str,
        items: Vec<PayoutItem>,
    ) -> anyhow::Result<AppResponse> {
        self.app.execute_contract(
            Addr::unchecked(sender),
            self.vault.clone(),
            &ExecuteMsg::Payout {
                host: HOST.to_string(),
                campaign_id: id.to_string(),
                items,
            },
            &[],
        )
    }

    fn refund(&mut self, id: &str) -> anyhow::Result<AppResponse> {
        self.refund_tier(id, None)
    }

    fn refund_tier(&mut self, id: &str, tier_id: Option<&str>) -> anyhow::Result<AppResponse> {
        self.app.execute_contract(
            Addr::unchecked(STRANGER),
            self.vault.clone(),
            &ExecuteMsg::Refund {
                host: HOST.to_string(),
                campaign_id: id.to_string(),
                tier_id: tier_id.map(str::to_string),
            },
            &[],
        )
    }

    fn campaign(&self, id: &str) -> CampaignResponse {
        self.app
            .wrap()
            .query_wasm_smart(
                self.vault.clone(),
                &QueryMsg::Campaign {
                    host: HOST.to_string(),
                    campaign_id: id.to_string(),
                },
            )
            .unwrap()
    }

    // --- cw721 helpers -----------------------------------------------------------------------

    fn new_collection(&mut self, minter: &str) -> Addr {
        let code = self.app.store_code(cw721_contract());
        self.app
            .instantiate_contract(
                code,
                Addr::unchecked(minter),
                &cw721_base::InstantiateMsg {
                    name: "Rewards".to_string(),
                    symbol: "RWD".to_string(),
                    minter: minter.to_string(),
                },
                &[],
                "collection",
                None,
            )
            .unwrap()
    }

    fn mint(&mut self, collection: &Addr, token_id: &str, owner: &str) {
        self.app
            .execute_contract(
                Addr::unchecked(HOST),
                collection.clone(),
                &Cw721Exec::Mint {
                    token_id: token_id.to_string(),
                    owner: owner.to_string(),
                    token_uri: None,
                    extension: None,
                },
                &[],
            )
            .unwrap();
    }

    fn send_nft(
        &mut self,
        collection: &Addr,
        from: &str,
        token_id: &str,
        campaign_id: &str,
        tier_id: &str,
    ) -> anyhow::Result<AppResponse> {
        self.app.execute_contract(
            Addr::unchecked(from),
            collection.clone(),
            &Cw721Exec::SendNft {
                contract: self.vault.to_string(),
                token_id: token_id.to_string(),
                msg: to_json_binary(&NftDepositMsg {
                    campaign_id: campaign_id.to_string(),
                    tier_id: tier_id.to_string(),
                })
                .unwrap(),
            },
            &[],
        )
    }

    fn owner_of(&self, collection: &Addr, token_id: &str) -> String {
        let response: OwnerOfResponse = self
            .app
            .wrap()
            .query_wasm_smart(
                collection.clone(),
                &Cw721Query::OwnerOf {
                    token_id: token_id.to_string(),
                    include_expired: None,
                },
            )
            .unwrap();
        response.owner
    }
}

// ---------------------------------------------------------------------------------------------
// Single native tier
// ---------------------------------------------------------------------------------------------

#[test]
fn native_campaign_full_lifecycle() {
    let mut s = Suite::new();
    s.create_native("evt-1", 3, 1).unwrap();
    assert_eq!(s.balance(HOST), START_BALANCE - 3 * (REWARD + FEE));

    let campaign = s.campaign("evt-1");
    assert_eq!(campaign.status, CampaignStatus::Open);
    assert_eq!(campaign.remaining_claims, 3);
    assert_eq!(campaign.remaining_reward, Uint128::new(3 * REWARD));
    assert_eq!(campaign.remaining_fee, Uint128::new(3 * FEE));
    assert_eq!(campaign.campaign.tiers[0].fee_per_claim, Uint128::new(FEE));

    s.payout(
        DISTRIBUTOR,
        "evt-1",
        vec![item("c1", MAIN, ALICE), item("c2", MAIN, BOB)],
    )
    .unwrap();
    assert_eq!(s.balance(ALICE), REWARD);
    assert_eq!(s.balance(BOB), REWARD);
    assert_eq!(s.balance(FEE_COLLECTOR), 2 * FEE);

    let claim: ClaimResponse = s
        .app
        .wrap()
        .query_wasm_smart(
            s.vault.clone(),
            &QueryMsg::Claim {
                host: HOST.to_string(),
                campaign_id: "evt-1".to_string(),
                claim_id: "c1".to_string(),
            },
        )
        .unwrap();
    let claim = claim.claim.unwrap();
    assert_eq!(claim.recipient, Addr::unchecked(ALICE));
    assert_eq!(claim.tier_id, MAIN);

    // Claim window over: reserved claims can still be paid, but no refund yet.
    s.advance(2 * DAY);
    assert_eq!(s.campaign("evt-1").status, CampaignStatus::Reservation);
    assert_err(
        s.refund("evt-1"),
        ContractError::RefundTooEarly {
            campaign_id: "evt-1".to_string(),
        },
    );

    // Reservation over: payouts stop, refund returns the unused reward and its fee.
    s.advance(7 * DAY);
    assert_eq!(s.campaign("evt-1").status, CampaignStatus::Ended);
    assert_err(
        s.payout(DISTRIBUTOR, "evt-1", vec![item("c3", MAIN, CAROL)]),
        ContractError::PayoutClosed {
            campaign_id: "evt-1".to_string(),
        },
    );
    s.refund("evt-1").unwrap();
    assert_eq!(
        s.balance(HOST),
        START_BALANCE - 3 * (REWARD + FEE) + (REWARD + FEE)
    );
    assert_eq!(s.balance(s.vault.as_str()), 0);
    assert_eq!(s.campaign("evt-1").status, CampaignStatus::Settled);
    assert_err(
        s.refund("evt-1"),
        ContractError::AlreadySettled {
            campaign_id: "evt-1".to_string(),
        },
    );
}

#[test]
fn deposit_must_match_exactly() {
    let mut s = Suite::new();
    let tiers = vec![native_tier(MAIN, REWARD, 3, 1)];
    let exact = deposit_for(&tiers);
    assert_err(
        s.create("evt-1", tiers.clone(), None, exact - 1),
        ContractError::WrongDeposit {
            expected: exact.to_string(),
            received: (exact - 1).to_string(),
            denom: DENOM.to_string(),
        },
    );
    assert_err(
        s.create("evt-1", tiers, None, 0),
        ContractError::Payment(PaymentError::NoFunds {}),
    );
}

#[test]
fn fee_never_drops_below_the_floor() {
    let mut s = Suite::new();
    // 5 % of 0.1 PASG is 0.005 PASG, below the 0.1 PASG floor.
    s.create(
        "small",
        vec![native_tier(MAIN, 100_000, 2, 1)],
        None,
        2 * (100_000 + MIN_FEE),
    )
    .unwrap();
    assert_eq!(
        s.campaign("small").campaign.tiers[0].fee_per_claim,
        Uint128::new(MIN_FEE)
    );
}

#[test]
fn only_the_distributor_can_pay() {
    let mut s = Suite::new();
    s.create_native("evt-1", 3, 1).unwrap();
    assert_err(
        s.payout(HOST, "evt-1", vec![item("c1", MAIN, HOST)]),
        ContractError::Unauthorized {},
    );
}

#[test]
fn tier_cap_is_enforced_inside_a_batch() {
    let mut s = Suite::new();
    s.create_native("evt-1", 3, 1).unwrap();
    assert_err(
        s.payout(
            DISTRIBUTOR,
            "evt-1",
            vec![item("c1", MAIN, ALICE), item("c2", MAIN, ALICE)],
        ),
        ContractError::AddressCapReached {
            recipient: ALICE.to_string(),
            tier_id: MAIN.to_string(),
        },
    );
    // The batch is atomic: nothing was paid.
    assert_eq!(s.balance(ALICE), 0);
    assert_eq!(s.campaign("evt-1").campaign.tiers[0].claims_paid, 0);
}

#[test]
fn tier_total_is_enforced() {
    let mut s = Suite::new();
    s.create_native("evt-1", 2, 2).unwrap();
    assert_err(
        s.payout(
            DISTRIBUTOR,
            "evt-1",
            vec![
                item("c1", MAIN, ALICE),
                item("c2", MAIN, BOB),
                item("c3", MAIN, CAROL),
            ],
        ),
        ContractError::TierExhausted {
            tier_id: MAIN.to_string(),
        },
    );
}

#[test]
fn retried_batches_are_idempotent() {
    let mut s = Suite::new();
    s.create_native("evt-1", 3, 1).unwrap();
    s.payout(DISTRIBUTOR, "evt-1", vec![item("c1", MAIN, ALICE)])
        .unwrap();

    // Resending c1 (same recipient) is skipped; only c2 pays.
    let response = s
        .payout(
            DISTRIBUTOR,
            "evt-1",
            vec![item("c1", MAIN, ALICE), item("c2", MAIN, BOB)],
        )
        .unwrap();
    let wasm = response.events.iter().find(|e| e.ty == "wasm").unwrap();
    let attr = |key: &str| {
        wasm.attributes
            .iter()
            .find(|a| a.key == key)
            .unwrap()
            .value
            .clone()
    };
    assert_eq!(attr("paid"), "1");
    assert_eq!(attr("skipped"), "1");
    assert_eq!(s.balance(ALICE), REWARD);
    assert_eq!(s.balance(BOB), REWARD);

    // The same claim id to a different recipient is a conflict, not a skip.
    assert_err(
        s.payout(DISTRIBUTOR, "evt-1", vec![item("c1", MAIN, CAROL)]),
        ContractError::ClaimConflict {
            claim_id: "c1".to_string(),
        },
    );
    assert_err(
        s.payout(
            DISTRIBUTOR,
            "evt-1",
            vec![item("c3", MAIN, CAROL), item("c3", MAIN, CAROL)],
        ),
        ContractError::DuplicateClaimInBatch {
            claim_id: "c3".to_string(),
        },
    );
}

#[test]
fn campaign_parameters_are_validated() {
    let mut s = Suite::new();
    let one = || vec![native_tier(MAIN, REWARD, 3, 1)];

    assert_err(
        s.create_funded("bad id!", one(), None),
        ContractError::InvalidCampaignId {},
    );
    assert_err(
        s.create_funded("evt-1", vec![native_tier(MAIN, REWARD, 3, 4)], None),
        ContractError::InvalidTier {
            reason: "main: per_address_cap must be between 1 and max_claims".to_string(),
        },
    );
    assert_err(
        s.create_funded(
            "evt-1",
            vec![
                native_tier(MAIN, REWARD, 3, 1),
                native_tier(MAIN, REWARD, 1, 1),
            ],
            None,
        ),
        ContractError::InvalidTier {
            reason: "main: repeats".to_string(),
        },
    );
    assert_err(
        s.create("evt-1", vec![], None, 0),
        ContractError::InvalidCampaign {
            reason: "a campaign needs between 1 and 20 tiers".to_string(),
        },
    );
    let too_many: Vec<TierInput> = (0..21)
        .map(|i| native_tier(&format!("t{i}"), REWARD, 1, 1))
        .collect();
    assert_err(
        s.create_funded("evt-1", too_many, None),
        ContractError::InvalidCampaign {
            reason: "a campaign needs between 1 and 20 tiers".to_string(),
        },
    );
    assert_err(
        s.create_funded("evt-1", one(), Some(0)),
        ContractError::InvalidCampaign {
            reason: "per_address_cap must be positive when set".to_string(),
        },
    );

    let now = s.now();
    let too_long = s.app.execute_contract(
        Addr::unchecked(HOST),
        s.vault.clone(),
        &ExecuteMsg::CreateCampaign {
            campaign_id: "evt-1".to_string(),
            tiers: one(),
            per_address_cap: None,
            claim_end: now.plus_seconds(DAY),
            reservation_end: now.plus_seconds(DAY + MAX_RESERVATION + 1),
        },
        &coins(deposit_for(&one()), DENOM),
    );
    assert_err(
        too_long,
        ContractError::InvalidCampaign {
            reason: "reservation period exceeds the platform maximum".to_string(),
        },
    );

    s.create_native("evt-1", 3, 1).unwrap();
    assert_err(
        s.create_native("evt-1", 3, 1),
        ContractError::CampaignExists {
            campaign_id: "evt-1".to_string(),
        },
    );
}

#[test]
fn config_changes_do_not_touch_funded_campaigns() {
    let mut s = Suite::new();
    s.create_native("evt-1", 3, 1).unwrap();

    let update = ExecuteMsg::UpdateConfig {
        owner: None,
        distributor: None,
        fee_collector: None,
        fee_bps: Some(1_000),
        min_fee: Some(Uint128::new(200_000)),
        nft_fee: None,
        max_reservation_secs: None,
        max_claim_window_secs: None,
        max_batch: None,
    };
    assert_err(
        s.app
            .execute_contract(Addr::unchecked(HOST), s.vault.clone(), &update, &[]),
        ContractError::Unauthorized {},
    );
    s.app
        .execute_contract(Addr::unchecked(OWNER), s.vault.clone(), &update, &[])
        .unwrap();

    assert_eq!(
        s.campaign("evt-1").campaign.tiers[0].fee_per_claim,
        Uint128::new(FEE)
    );
    s.payout(DISTRIBUTOR, "evt-1", vec![item("c1", MAIN, ALICE)])
        .unwrap();
    assert_eq!(s.balance(FEE_COLLECTOR), FEE);
}

#[test]
fn migrate_keeps_state() {
    let mut s = Suite::new();
    s.create_native("evt-1", 3, 1).unwrap();
    let code = s.app.store_code(vault_contract());
    assert_ne!(code, s.vault_code);
    s.app
        .migrate_contract(Addr::unchecked(OWNER), s.vault.clone(), &MigrateMsg {}, code)
        .unwrap();
    assert_eq!(s.campaign("evt-1").remaining_claims, 3);
}

// ---------------------------------------------------------------------------------------------
// An event with several tiers
// ---------------------------------------------------------------------------------------------

#[test]
fn event_with_several_tiers_and_both_caps() {
    let mut s = Suite::new();
    let swords = s.new_collection(HOST);
    s.mint(&swords, "s1", HOST);
    s.mint(&swords, "s2", HOST);

    // Chest: 1 PASG x5 (cap 3). Boss: 10 PASG x1. Sword: NFT x2. At most 4 prizes per wallet.
    let tiers = vec![
        native_tier("chest", 1_000_000, 5, 3),
        native_tier("boss", 10_000_000, 1, 1),
        nft_tier("sword", &swords, 2, 1),
    ];
    let deposit = deposit_for(&tiers);
    assert_eq!(
        deposit,
        5 * (1_000_000 + 100_000) + (10_000_000 + 500_000) + 2 * NFT_FEE
    );
    s.create("ev-1", tiers, Some(4), deposit).unwrap();

    // Fail closed: no tier pays until the sword tier holds its NFTs.
    assert_eq!(s.campaign("ev-1").status, CampaignStatus::Funding);
    assert_err(
        s.payout(DISTRIBUTOR, "ev-1", vec![item("k1", "chest", ALICE)]),
        ContractError::NotFunded {
            campaign_id: "ev-1".to_string(),
        },
    );
    s.send_nft(&swords, HOST, "s1", "ev-1", "sword").unwrap();
    s.send_nft(&swords, HOST, "s2", "ev-1", "sword").unwrap();
    let campaign = s.campaign("ev-1");
    assert_eq!(campaign.status, CampaignStatus::Open);
    assert!(campaign.tiers.iter().all(|tier| tier.funded));

    // Tier cap: three chests for Alice, the fourth is refused.
    s.payout(
        DISTRIBUTOR,
        "ev-1",
        vec![
            item("k1", "chest", ALICE),
            item("k2", "chest", ALICE),
            item("k3", "chest", ALICE),
        ],
    )
    .unwrap();
    assert_eq!(s.balance(ALICE), 3_000_000);
    assert_err(
        s.payout(DISTRIBUTOR, "ev-1", vec![item("k4", "chest", ALICE)]),
        ContractError::AddressCapReached {
            recipient: ALICE.to_string(),
            tier_id: "chest".to_string(),
        },
    );

    // A sword brings Alice to the event cap of 4, so the boss is refused for her.
    s.payout(
        DISTRIBUTOR,
        "ev-1",
        vec![nft_item("w1", "sword", ALICE, "s1")],
    )
    .unwrap();
    assert_eq!(s.owner_of(&swords, "s1"), ALICE);
    assert_err(
        s.payout(DISTRIBUTOR, "ev-1", vec![item("b1", "boss", ALICE)]),
        ContractError::EventCapReached {
            recipient: ALICE.to_string(),
            campaign_id: "ev-1".to_string(),
        },
    );

    // Bob takes the only boss; nobody else can.
    s.payout(DISTRIBUTOR, "ev-1", vec![item("b1", "boss", BOB)])
        .unwrap();
    assert_eq!(s.balance(BOB), 10_000_000);
    assert_err(
        s.payout(DISTRIBUTOR, "ev-1", vec![item("b2", "boss", CAROL)]),
        ContractError::TierExhausted {
            tier_id: "boss".to_string(),
        },
    );
    assert_err(
        s.payout(DISTRIBUTOR, "ev-1", vec![item("u1", "nope", CAROL)]),
        ContractError::InvalidPayoutItem {
            claim_id: "u1".to_string(),
            reason: "unknown tier nope".to_string(),
        },
    );

    let counts: AddressClaimsResponse = s
        .app
        .wrap()
        .query_wasm_smart(
            s.vault.clone(),
            &QueryMsg::AddressClaims {
                host: HOST.to_string(),
                campaign_id: "ev-1".to_string(),
                address: ALICE.to_string(),
            },
        )
        .unwrap();
    assert_eq!(counts.total, 4);
    assert_eq!(
        counts.tiers,
        vec![
            TierCount {
                tier_id: "chest".to_string(),
                count: 3
            },
            TierCount {
                tier_id: "boss".to_string(),
                count: 0
            },
            TierCount {
                tier_id: "sword".to_string(),
                count: 1
            },
        ]
    );
    assert_eq!(s.balance(FEE_COLLECTOR), 3 * 100_000 + NFT_FEE + 500_000);

    // Settle: first the PASG (two unused chests with their fees, one unused sword fee),
    // alone; then sword s2.
    s.advance(9 * DAY);
    s.refund("ev-1").unwrap();
    assert_eq!(s.owner_of(&swords, "s2"), s.vault.to_string());
    s.refund("ev-1").unwrap();
    assert_eq!(s.owner_of(&swords, "s2"), HOST);
    assert_eq!(
        s.balance(HOST),
        START_BALANCE - deposit + 2 * (1_000_000 + 100_000) + NFT_FEE
    );
    assert_eq!(s.balance(s.vault.as_str()), 0);
    assert_eq!(s.campaign("ev-1").status, CampaignStatus::Settled);
}

// ---------------------------------------------------------------------------------------------
// NFT tiers
// ---------------------------------------------------------------------------------------------

#[test]
fn nft_campaign_full_lifecycle() {
    let mut s = Suite::new();
    let collection = s.new_collection(HOST);
    s.mint(&collection, "t1", HOST);
    s.mint(&collection, "t2", HOST);

    s.create_funded("nft-1", vec![nft_tier(NFT, &collection, 2, 1)], None)
        .unwrap();
    s.send_nft(&collection, HOST, "t1", "nft-1", NFT).unwrap();
    s.send_nft(&collection, HOST, "t2", "nft-1", NFT).unwrap();
    assert_eq!(s.owner_of(&collection, "t1"), s.vault.to_string());

    assert_err(
        s.payout(DISTRIBUTOR, "nft-1", vec![nft_item("n1", NFT, ALICE, "t9")]),
        ContractError::TokenNotAvailable {
            token_id: "t9".to_string(),
            tier_id: NFT.to_string(),
        },
    );
    s.payout(DISTRIBUTOR, "nft-1", vec![nft_item("n1", NFT, ALICE, "t1")])
        .unwrap();
    assert_eq!(s.owner_of(&collection, "t1"), ALICE);
    assert_eq!(s.balance(FEE_COLLECTOR), NFT_FEE);

    s.advance(9 * DAY);
    s.refund("nft-1").unwrap();
    assert_eq!(s.balance(HOST), START_BALANCE - NFT_FEE);
    s.refund("nft-1").unwrap();
    assert_eq!(s.owner_of(&collection, "t2"), HOST);
    assert_eq!(s.campaign("nft-1").status, CampaignStatus::Settled);
}

#[test]
fn nft_deposits_are_restricted() {
    let mut s = Suite::new();
    let collection = s.new_collection(HOST);
    let other = s.new_collection(HOST);
    s.mint(&collection, "mine", STRANGER);
    s.mint(&other, "other", HOST);
    s.mint(&collection, "t1", HOST);
    s.mint(&collection, "t2", HOST);

    s.create_funded(
        "nft-1",
        vec![
            native_tier(MAIN, REWARD, 1, 1),
            nft_tier(NFT, &collection, 1, 1),
        ],
        None,
    )
    .unwrap();

    // A deposit always targets the sender's own campaign, so a stranger finds none.
    assert_err(
        s.send_nft(&collection, STRANGER, "mine", "nft-1", NFT),
        ContractError::CampaignNotFound {
            campaign_id: "nft-1".to_string(),
        },
    );
    assert_err(
        s.send_nft(&other, HOST, "other", "nft-1", NFT),
        ContractError::WrongCollection {
            collection: other.to_string(),
            tier_id: NFT.to_string(),
        },
    );
    assert_err(
        s.send_nft(&collection, HOST, "t1", "nft-1", MAIN),
        ContractError::InvalidTier {
            reason: "main: this tier does not take NFTs".to_string(),
        },
    );
    assert_err(
        s.send_nft(&collection, HOST, "t1", "nft-1", "zzz"),
        ContractError::UnknownTier {
            tier_id: "zzz".to_string(),
        },
    );
    s.send_nft(&collection, HOST, "t1", "nft-1", NFT).unwrap();
    assert_err(
        s.send_nft(&collection, HOST, "t2", "nft-1", NFT),
        ContractError::TierFull {
            tier_id: NFT.to_string(),
        },
    );
}

#[test]
fn host_can_cancel_an_unfunded_campaign() {
    let mut s = Suite::new();
    let collection = s.new_collection(HOST);
    s.mint(&collection, "t1", HOST);
    // A native tier plus an NFT tier that only gets one of its two NFTs.
    s.create_funded(
        "ev-1",
        vec![
            native_tier(MAIN, REWARD, 1, 1),
            nft_tier(NFT, &collection, 2, 1),
        ],
        None,
    )
    .unwrap();
    s.send_nft(&collection, HOST, "t1", "ev-1", NFT).unwrap();

    let cancel = ExecuteMsg::CancelUnfunded {
        campaign_id: "ev-1".to_string(),
    };
    // Cancel acts on the sender's own campaign, so a stranger finds none.
    assert_err(
        s.app
            .execute_contract(Addr::unchecked(STRANGER), s.vault.clone(), &cancel, &[]),
        ContractError::CampaignNotFound {
            campaign_id: "ev-1".to_string(),
        },
    );
    s.app
        .execute_contract(Addr::unchecked(HOST), s.vault.clone(), &cancel, &[])
        .unwrap();
    // The PASG comes back at once; the NFT follows with a refund.
    assert_eq!(s.balance(HOST), START_BALANCE);
    assert_eq!(s.campaign("ev-1").status, CampaignStatus::Cancelled);
    s.refund("ev-1").unwrap();
    assert_eq!(s.owner_of(&collection, "t1"), HOST);
    assert_eq!(s.campaign("ev-1").status, CampaignStatus::Settled);

    // A campaign with only native tiers is funded at creation, so it cannot be cancelled.
    s.create_native("evt-2", 1, 1).unwrap();
    assert_err(
        s.app.execute_contract(
            Addr::unchecked(HOST),
            s.vault.clone(),
            &ExecuteMsg::CancelUnfunded {
                campaign_id: "evt-2".to_string(),
            },
            &[],
        ),
        ContractError::CannotCancel {
            campaign_id: "evt-2".to_string(),
        },
    );
}

#[test]
fn refund_returns_nfts_in_pages() {
    let mut s = Suite::new();
    s.app
        .execute_contract(
            Addr::unchecked(OWNER),
            s.vault.clone(),
            &ExecuteMsg::UpdateConfig {
                owner: None,
                distributor: None,
                fee_collector: None,
                fee_bps: None,
                min_fee: None,
                nft_fee: None,
                max_reservation_secs: None,
                max_claim_window_secs: None,
                max_batch: Some(1),
            },
            &[],
        )
        .unwrap();
    let collection = s.new_collection(HOST);
    s.mint(&collection, "t1", HOST);
    s.mint(&collection, "t2", HOST);
    s.create_funded("nft-1", vec![nft_tier(NFT, &collection, 2, 1)], None)
        .unwrap();
    s.send_nft(&collection, HOST, "t1", "nft-1", NFT).unwrap();
    s.send_nft(&collection, HOST, "t2", "nft-1", NFT).unwrap();

    s.advance(9 * DAY);
    s.refund("nft-1").unwrap(); // the PASG leftover, alone
    assert_eq!(s.owner_of(&collection, "t1"), s.vault.to_string());
    s.refund("nft-1").unwrap(); // first NFT page
    assert_eq!(s.owner_of(&collection, "t1"), HOST);
    assert_eq!(s.owner_of(&collection, "t2"), s.vault.to_string());
    assert_eq!(s.campaign("nft-1").status, CampaignStatus::Ended);

    s.refund("nft-1").unwrap();
    assert_eq!(s.owner_of(&collection, "t2"), HOST);
    assert_eq!(s.campaign("nft-1").status, CampaignStatus::Settled);
    // The fee leftover was returned once, not once per page.
    assert_eq!(s.balance(HOST), START_BALANCE);
}

// ---------------------------------------------------------------------------------------------
// Fixes from the independent review (2026-11-06) and edge cases
// ---------------------------------------------------------------------------------------------

/// A collection whose `transfer_nft` always fails, as if its admin froze it after funding.
#[cw_serde]
enum FrozenExec {
    Deposit {
        vault: String,
        campaign_id: String,
        tier_id: String,
        token_id: String,
    },
    TransferNft {
        recipient: String,
        token_id: String,
    },
}

fn frozen_instantiate(_: DepsMut, _: Env, _: MessageInfo, _: Empty) -> StdResult<Response> {
    Ok(Response::new())
}

fn frozen_execute(_: DepsMut, _: Env, info: MessageInfo, msg: FrozenExec) -> StdResult<Response> {
    match msg {
        FrozenExec::Deposit {
            vault,
            campaign_id,
            tier_id,
            token_id,
        } => Ok(Response::new().add_message(WasmMsg::Execute {
            contract_addr: vault,
            msg: to_json_binary(&ExecuteMsg::ReceiveNft(Cw721ReceiveMsg {
                sender: info.sender.to_string(),
                token_id,
                msg: to_json_binary(&NftDepositMsg {
                    campaign_id,
                    tier_id,
                })?,
            }))?,
            funds: vec![],
        })),
        FrozenExec::TransferNft { .. } => Err(StdError::generic_err("frozen collection")),
    }
}

fn frozen_query(_: Deps, _: Env, _: Empty) -> StdResult<Binary> {
    Err(StdError::generic_err("no queries"))
}

#[test]
fn a_frozen_collection_cannot_lock_the_pasg_or_other_tiers() {
    let mut s = Suite::new();
    let frozen_code = s.app.store_code(Box::new(ContractWrapper::new(
        frozen_execute,
        frozen_instantiate,
        frozen_query,
    )));
    let frozen = s
        .app
        .instantiate_contract(frozen_code, Addr::unchecked(HOST), &Empty {}, &[], "frozen", None)
        .unwrap();
    let good = s.new_collection(HOST);
    s.mint(&good, "g1", HOST);

    s.create_funded(
        "ev-1",
        vec![
            native_tier(MAIN, REWARD, 2, 1),
            nft_tier("aaa", &frozen, 1, 1), // sorts first, so it heads every full page
            nft_tier("zzz", &good, 1, 1),
        ],
        None,
    )
    .unwrap();
    s.app
        .execute_contract(
            Addr::unchecked(HOST),
            frozen.clone(),
            &FrozenExec::Deposit {
                vault: s.vault.to_string(),
                campaign_id: "ev-1".to_string(),
                tier_id: "aaa".to_string(),
                token_id: "f1".to_string(),
            },
            &[],
        )
        .unwrap();
    s.send_nft(&good, HOST, "g1", "ev-1", "zzz").unwrap();

    s.advance(9 * DAY);
    let before = s.balance(HOST);
    s.refund("ev-1").unwrap();
    assert_eq!(
        s.balance(HOST) - before,
        2 * (REWARD + FEE) + 2 * NFT_FEE,
        "the PASG leftover is returned even though one collection is frozen"
    );
    assert!(s.refund("ev-1").is_err(), "a full page still hits the frozen tier");
    s.refund_tier("ev-1", Some("zzz")).unwrap();
    assert_eq!(s.owner_of(&good, "g1"), HOST);
    assert_eq!(s.campaign("ev-1").status, CampaignStatus::Ended);
    assert_err(
        s.refund_tier("ev-1", Some("nope")),
        ContractError::UnknownTier {
            tier_id: "nope".to_string(),
        },
    );
}

#[test]
fn campaign_ids_are_scoped_to_their_host() {
    let mut s = Suite::new();
    let now = s.now();
    // A stranger takes the id first, in its own namespace...
    s.app
        .execute_contract(
            Addr::unchecked(STRANGER),
            s.vault.clone(),
            &ExecuteMsg::CreateCampaign {
                campaign_id: "evt-1".to_string(),
                tiers: vec![native_tier(MAIN, REWARD, 1, 1)],
                per_address_cap: None,
                claim_end: now.plus_seconds(DAY),
                reservation_end: now.plus_seconds(2 * DAY),
            },
            &coins(REWARD + FEE, DENOM),
        )
        .unwrap();
    // ...and the host can still create and use its own.
    s.create_native("evt-1", 3, 1).unwrap();
    s.payout(DISTRIBUTOR, "evt-1", vec![item("c1", MAIN, ALICE)])
        .unwrap();
    assert_eq!(s.campaign("evt-1").campaign.host, Addr::unchecked(HOST));
    assert_eq!(s.campaign("evt-1").campaign.tiers[0].max_claims, 3);
}

#[test]
fn the_vault_cannot_be_a_recipient() {
    let mut s = Suite::new();
    s.create_native("evt-1", 3, 1).unwrap();
    let vault = s.vault.to_string();
    assert_err(
        s.payout(DISTRIBUTOR, "evt-1", vec![item("c1", MAIN, &vault)]),
        ContractError::InvalidPayoutItem {
            claim_id: "c1".to_string(),
            reason: "recipient cannot be the vault".to_string(),
        },
    );
}

#[test]
fn the_claim_window_has_a_platform_maximum() {
    let mut s = Suite::new();
    let now = s.now();
    let tiers = vec![native_tier(MAIN, REWARD, 1, 1)];
    let too_far = s.app.execute_contract(
        Addr::unchecked(HOST),
        s.vault.clone(),
        &ExecuteMsg::CreateCampaign {
            campaign_id: "evt-1".to_string(),
            tiers: tiers.clone(),
            per_address_cap: None,
            claim_end: now.plus_seconds(MAX_CLAIM_WINDOW + 1),
            reservation_end: now.plus_seconds(MAX_CLAIM_WINDOW + 2),
        },
        &coins(deposit_for(&tiers), DENOM),
    );
    assert_err(
        too_far,
        ContractError::InvalidCampaign {
            reason: "claim window exceeds the platform maximum".to_string(),
        },
    );
}

#[test]
fn an_nft_tier_needs_a_contract_collection() {
    let mut s = Suite::new();
    let wallet = Addr::unchecked(STRANGER);
    assert_err(
        s.create_funded("nft-1", vec![nft_tier(NFT, &wallet, 1, 1)], None),
        ContractError::InvalidTier {
            reason: "nft: collection is not a contract".to_string(),
        },
    );
}

#[test]
fn deposits_with_no_fee_or_the_wrong_coins() {
    let mut s = Suite::new();
    s.app
        .execute_contract(
            Addr::unchecked(OWNER),
            s.vault.clone(),
            &ExecuteMsg::UpdateConfig {
                owner: None,
                distributor: None,
                fee_collector: None,
                fee_bps: None,
                min_fee: None,
                nft_fee: Some(Uint128::zero()),
                max_reservation_secs: None,
                max_claim_window_secs: None,
                max_batch: None,
            },
            &[],
        )
        .unwrap();
    let collection = s.new_collection(HOST);
    // nft_fee = 0: an NFT-only campaign needs no funds and refuses any.
    assert_err(
        s.create("nft-1", vec![nft_tier(NFT, &collection, 1, 1)], None, 1),
        ContractError::Payment(PaymentError::NonPayable {}),
    );
    s.create("nft-1", vec![nft_tier(NFT, &collection, 1, 1)], None, 0)
        .unwrap();
    // A deposit in another denom is refused.
    let tiers = vec![native_tier(MAIN, REWARD, 1, 1)];
    let exact = deposit_for(&tiers);
    let now = s.now();
    let other_denom = s.app.execute_contract(
        Addr::unchecked(HOST),
        s.vault.clone(),
        &ExecuteMsg::CreateCampaign {
            campaign_id: "evt-1".to_string(),
            tiers,
            per_address_cap: None,
            claim_end: now.plus_seconds(DAY),
            reservation_end: now.plus_seconds(2 * DAY),
        },
        &[coin(exact, "uatom")],
    );
    assert!(other_denom.is_err());
}

#[test]
fn payout_items_are_validated() {
    let mut s = Suite::new();
    let collection = s.new_collection(HOST);
    s.mint(&collection, "t1", HOST);
    s.create_funded(
        "ev-1",
        vec![
            native_tier(MAIN, REWARD, 3, 3),
            nft_tier(NFT, &collection, 1, 1),
        ],
        None,
    )
    .unwrap();
    s.send_nft(&collection, HOST, "t1", "ev-1", NFT).unwrap();

    assert_err(
        s.payout(DISTRIBUTOR, "ev-1", vec![]),
        ContractError::InvalidBatchSize { max: 50 },
    );
    let too_many: Vec<PayoutItem> = (0..51)
        .map(|i| item(&format!("c{i}"), MAIN, ALICE))
        .collect();
    assert_err(
        s.payout(DISTRIBUTOR, "ev-1", too_many),
        ContractError::InvalidBatchSize { max: 50 },
    );
    assert_err(
        s.payout(DISTRIBUTOR, "ev-1", vec![nft_item("c1", MAIN, ALICE, "t1")]),
        ContractError::InvalidPayoutItem {
            claim_id: "c1".to_string(),
            reason: "token_id is not allowed in a native tier".to_string(),
        },
    );
    assert_err(
        s.payout(DISTRIBUTOR, "ev-1", vec![item("n1", NFT, ALICE)]),
        ContractError::InvalidPayoutItem {
            claim_id: "n1".to_string(),
            reason: "token_id is required in an NFT tier".to_string(),
        },
    );
    // NFT retry is skipped; the same claim with another token is a conflict.
    s.payout(DISTRIBUTOR, "ev-1", vec![nft_item("n1", NFT, ALICE, "t1")])
        .unwrap();
    s.payout(DISTRIBUTOR, "ev-1", vec![nft_item("n1", NFT, ALICE, "t1")])
        .unwrap();
    assert_eq!(s.owner_of(&collection, "t1"), ALICE);
    assert_err(
        s.payout(DISTRIBUTOR, "ev-1", vec![nft_item("n1", NFT, ALICE, "t2")]),
        ContractError::ClaimConflict {
            claim_id: "n1".to_string(),
        },
    );
    // Two PASG claims to one wallet in one batch arrive as a single transfer.
    s.payout(
        DISTRIBUTOR,
        "ev-1",
        vec![item("c1", MAIN, BOB), item("c2", MAIN, BOB)],
    )
    .unwrap();
    assert_eq!(s.balance(BOB), 2 * REWARD);
}

#[test]
fn time_boundaries_are_exact() {
    let mut s = Suite::new();
    let collection = s.new_collection(HOST);
    s.mint(&collection, "t1", HOST);
    s.create_funded("nft-1", vec![nft_tier(NFT, &collection, 1, 1)], None)
        .unwrap();
    s.create_native("evt-1", 2, 1).unwrap();

    s.advance(DAY); // exactly claim_end
    assert_err(
        s.send_nft(&collection, HOST, "t1", "nft-1", NFT),
        ContractError::NftDepositClosed {
            campaign_id: "nft-1".to_string(),
        },
    );
    // Reserved claims can still be paid after claim_end.
    s.payout(DISTRIBUTOR, "evt-1", vec![item("c1", MAIN, ALICE)])
        .unwrap();

    s.advance(7 * DAY); // exactly reservation_end
    assert_err(
        s.payout(DISTRIBUTOR, "evt-1", vec![item("c2", MAIN, BOB)]),
        ContractError::PayoutClosed {
            campaign_id: "evt-1".to_string(),
        },
    );
    s.refund("evt-1").unwrap();
    // A campaign that never got its NFTs settles too.
    s.refund("nft-1").unwrap();
    assert_eq!(s.campaign("nft-1").status, CampaignStatus::Settled);
}

#[test]
fn campaigns_never_touch_each_other() {
    let mut s = Suite::new();
    s.create_native("a", 3, 1).unwrap();
    s.create_funded("b", vec![native_tier(MAIN, 5_000_000, 2, 1)], None)
        .unwrap();
    s.payout(DISTRIBUTOR, "a", vec![item("c1", MAIN, ALICE)])
        .unwrap();
    s.payout(DISTRIBUTOR, "b", vec![item("c1", MAIN, ALICE)])
        .unwrap();

    s.advance(9 * DAY);
    s.refund("a").unwrap();
    // After settling "a", the vault holds exactly what "b" still owes.
    let b = s.campaign("b");
    assert_eq!(
        s.balance(s.vault.as_str()),
        (b.remaining_reward + b.remaining_fee).u128()
    );
    s.refund("b").unwrap();
    assert_eq!(s.balance(s.vault.as_str()), 0);
}

#[test]
fn a_new_distributor_takes_over_at_once() {
    let mut s = Suite::new();
    s.create_native("evt-1", 3, 1).unwrap();
    s.app
        .execute_contract(
            Addr::unchecked(OWNER),
            s.vault.clone(),
            &ExecuteMsg::UpdateConfig {
                owner: None,
                distributor: Some(CAROL.to_string()),
                fee_collector: None,
                fee_bps: None,
                min_fee: None,
                nft_fee: None,
                max_reservation_secs: None,
                max_claim_window_secs: None,
                max_batch: None,
            },
            &[],
        )
        .unwrap();
    assert_err(
        s.payout(DISTRIBUTOR, "evt-1", vec![item("c1", MAIN, ALICE)]),
        ContractError::Unauthorized {},
    );
    s.payout(CAROL, "evt-1", vec![item("c1", MAIN, ALICE)])
        .unwrap();
    assert_eq!(s.balance(ALICE), REWARD);
}
