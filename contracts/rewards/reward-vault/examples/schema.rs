use std::env::current_dir;
use std::fs::create_dir_all;

use cosmwasm_schema::{export_schema, remove_schemas, schema_for};

use reward_vault::msg::{
    AddressClaimsResponse, CampaignNftsResponse, CampaignResponse, CampaignsResponse,
    ClaimResponse, ExecuteMsg, InstantiateMsg, MigrateMsg, NftDepositMsg, QueryMsg,
};
use reward_vault::state::Config;

fn main() {
    let mut out_dir = current_dir().unwrap();
    out_dir.push("schema");
    create_dir_all(&out_dir).unwrap();
    remove_schemas(&out_dir).unwrap();

    export_schema(&schema_for!(InstantiateMsg), &out_dir);
    export_schema(&schema_for!(ExecuteMsg), &out_dir);
    export_schema(&schema_for!(QueryMsg), &out_dir);
    export_schema(&schema_for!(MigrateMsg), &out_dir);
    // The message a host puts inside cw721 `send_nft` to fund an NFT tier.
    export_schema(&schema_for!(NftDepositMsg), &out_dir);

    export_schema(&schema_for!(Config), &out_dir);
    export_schema(&schema_for!(CampaignResponse), &out_dir);
    export_schema(&schema_for!(CampaignsResponse), &out_dir);
    export_schema(&schema_for!(ClaimResponse), &out_dir);
    export_schema(&schema_for!(AddressClaimsResponse), &out_dir);
    export_schema(&schema_for!(CampaignNftsResponse), &out_dir);
}
