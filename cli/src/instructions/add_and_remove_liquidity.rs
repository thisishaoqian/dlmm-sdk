use crate::*;
use commons::dlmm::accounts::{LbPair, PositionV2};
use instructions::*;
use instructions::utils::get_token_account_balance;
use serde_json::json;

#[derive(Debug, Parser)]
pub struct AddAndRemoveLiquidityParams {
    /// Address of the liquidity pair.
    pub lb_pair: Pubkey,
    /// Position for the deposit and withdrawal.
    pub position: Pubkey,
    /// Amount of token X to be deposited.
    pub amount_x: u64,
    /// Amount of token Y to be deposited.
    pub amount_y: u64,
    /// Liquidity distribution to the bins. "<DELTA_ID,DIST_X,DIST_Y, DELTA_ID,DIST_X,DIST_Y, ...>" where
    /// DELTA_ID = Number of bins surrounding the active bin. This decide which bin the token is going to deposit to. For example: if the current active id is 5555, delta_ids is 1, the user will be depositing to bin 5554, 5555, and 5556.
    /// DIST_X = Percentage of amount_x to be deposited to the bins. Must not > 1.0
    /// DIST_Y = Percentage of amount_y to be deposited to the bins. Must not > 1.0
    /// For example: --bin-liquidity-distribution "-1,0.0,0.25 0,0.75,0.75 1,0.25,0.0"
    #[clap(long, value_parser = parse_bin_liquidity_distribution, value_delimiter = ' ', allow_hyphen_values = true)]
    pub bin_liquidity_distribution: Vec<(i32, f64, f64)>,
    /// Bin liquidity information to be remove. "<BIN_ID,BPS_TO_REMOVE, BIN_ID,BPS_TO_REMOVE, ...>" where
    /// BIN_ID = bin id to withdraw
    /// BPS_TO_REMOVE = Percentage of position owned share to be removed. Maximum is 1.0f, which equivalent to 100%.
    #[clap(long, value_parser = parse_bin_liquidity_removal, value_delimiter = ' ', allow_hyphen_values = true)]
    pub bin_liquidity_removal: Vec<(i32, f64)>,
}

pub async fn execute_add_and_remove_liquidity<C: Deref<Target = impl Signer> + Clone>(
    params: AddAndRemoveLiquidityParams,
    program: &Program<C>,
    transaction_config: RpcSendTransactionConfig,
    compute_unit_price: Option<Instruction>,
) -> Result<()> {
    let AddAndRemoveLiquidityParams {
        lb_pair,
        position,
        amount_x,
        amount_y,
        mut bin_liquidity_distribution,
        mut bin_liquidity_removal,
    } = params;

    // Sort by bin id for both operations
    bin_liquidity_distribution.sort_by(|a, b| a.0.cmp(&b.0));
    bin_liquidity_removal.sort_by(|a, b| a.0.cmp(&b.0));

    let rpc_client = program.rpc();

    // Get account data
    let mut accounts = rpc_client
        .get_multiple_accounts(&[lb_pair, position])
        .await?;

    let lb_pair_account = accounts[0].take().context("lb_pair not found")?;
    let position_account = accounts[1].take().context("position not found")?;

    let lb_pair_state: LbPair = bytemuck::pod_read_unaligned(&lb_pair_account.data[8..]);
    let position_state: PositionV2 = bytemuck::pod_read_unaligned(&position_account.data[8..]);

    // Prepare bin liquidity distribution for add liquidity
    let bin_liquidity_distribution = bin_liquidity_distribution
        .into_iter()
        .map(|(bin_id, dist_x, dist_y)| BinLiquidityDistribution {
            bin_id,
            distribution_x: (dist_x * BASIS_POINT_MAX as f64) as u16,
            distribution_y: (dist_y * BASIS_POINT_MAX as f64) as u16,
        })
        .collect::<Vec<_>>();

    // Prepare bin liquidity removal for remove liquidity
    let bin_liquidity_removal = bin_liquidity_removal
        .into_iter()
        .map(|(bin_id, bps)| BinLiquidityReduction {
            bin_id,
            bps_to_remove: (bps * BASIS_POINT_MAX as f64) as u16,
        })
        .collect::<Vec<BinLiquidityReduction>>();

    // Calculate bin ranges for both operations
    let add_min_bin_id = bin_liquidity_distribution
        .first()
        .map(|bld| bld.bin_id)
        .context("No bin liquidity distribution provided")?;
    let add_max_bin_id = bin_liquidity_distribution
        .last()
        .map(|bld| bld.bin_id)
        .context("No bin liquidity distribution provided")?;

    let remove_min_bin_id = bin_liquidity_removal
        .first()
        .map(|reduction| reduction.bin_id)
        .context("bin_liquidity_removal is empty")?;
    let remove_max_bin_id = bin_liquidity_removal
        .last()
        .map(|reduction| reduction.bin_id)
        .context("bin_liquidity_removal is empty")?;

    // Get bin array accounts for both operations
    let add_bin_arrays_account_meta =
        position_state.get_bin_array_accounts_meta_coverage_by_chunk(add_min_bin_id, add_max_bin_id)?;
    let remove_bin_arrays_account_meta =
        position_state.get_bin_array_accounts_meta_coverage_by_chunk(remove_min_bin_id, remove_max_bin_id)?;

    // Get user token accounts
    let user_token_x = get_or_create_ata(
        program,
        transaction_config,
        lb_pair_state.token_x_mint,
        program.payer(),
        compute_unit_price.clone(),
    )
    .await?;

    let user_token_y = get_or_create_ata(
        program,
        transaction_config,
        lb_pair_state.token_y_mint,
        program.payer(),
        compute_unit_price.clone(),
    )
    .await?;

    // Get token balances before transaction
    let token_x_balance_before = get_token_account_balance(&rpc_client, user_token_x).await?;
    let token_y_balance_before = get_token_account_balance(&rpc_client, user_token_y).await?;
    
    // Store balances before transaction for final JSON output
    let token_x_before = token_x_balance_before;
    let token_y_before = token_y_balance_before;

    let (bin_array_bitmap_extension, _bump) = derive_bin_array_bitmap_extension(lb_pair);
    let bin_array_bitmap_extension = rpc_client
        .get_account(&bin_array_bitmap_extension)
        .await
        .map(|_| bin_array_bitmap_extension)
        .ok()
        .or(Some(dlmm::ID));

    let (event_authority, _bump) = derive_event_authority_pda();

    // Get token programs
    let [token_x_program, token_y_program] = lb_pair_state.get_token_programs()?;

    // Prepare remaining accounts for both operations
    let mut remaining_accounts_info = RemainingAccountsInfo { slices: vec![] };
    let mut remaining_accounts = vec![];

    if let Some((slices, transfer_hook_remaining_accounts)) =
        get_potential_token_2022_related_ix_data_and_accounts(
            &lb_pair_state,
            program.rpc(),
            ActionType::Liquidity,
        )
        .await?
    {
        remaining_accounts_info.slices = slices;
        remaining_accounts.extend(transfer_hook_remaining_accounts);
    };

    // Combine bin array accounts from both operations
    let mut all_bin_arrays = add_bin_arrays_account_meta;
    all_bin_arrays.extend(remove_bin_arrays_account_meta);
    remaining_accounts.extend(all_bin_arrays);

    // Create add liquidity instruction
    let add_main_accounts = dlmm::client::accounts::AddLiquidity2 {
        lb_pair,
        bin_array_bitmap_extension,
        position,
        reserve_x: lb_pair_state.reserve_x,
        reserve_y: lb_pair_state.reserve_y,
        token_x_mint: lb_pair_state.token_x_mint,
        token_y_mint: lb_pair_state.token_y_mint,
        sender: program.payer(),
        user_token_x,
        user_token_y,
        token_x_program,
        token_y_program,
        event_authority,
        program: dlmm::ID,
    }
    .to_account_metas(None);

    let add_data = dlmm::client::args::AddLiquidity2 {
        liquidity_parameter: LiquidityParameter {
            amount_x,
            amount_y,
            bin_liquidity_dist: bin_liquidity_distribution,
        },
        remaining_accounts_info: remaining_accounts_info.clone(),
    }
    .data();

    let add_liquidity_ix = Instruction {
        program_id: dlmm::ID,
        accounts: [add_main_accounts.to_vec(), remaining_accounts.clone()].concat(),
        data: add_data,
    };

    // Create remove liquidity instruction
    let remove_main_accounts = dlmm::client::accounts::RemoveLiquidity2 {
        position,
        lb_pair,
        bin_array_bitmap_extension,
        user_token_x,
        user_token_y,
        reserve_x: lb_pair_state.reserve_x,
        reserve_y: lb_pair_state.reserve_y,
        token_x_mint: lb_pair_state.token_x_mint,
        token_x_program,
        token_y_mint: lb_pair_state.token_y_mint,
        token_y_program,
        sender: program.payer(),
        memo_program: spl_memo::ID,
        event_authority,
        program: dlmm::ID,
    }
    .to_account_metas(None);

    let remove_data = dlmm::client::args::RemoveLiquidity2 {
        bin_liquidity_removal,
        remaining_accounts_info,
    }
    .data();

    let remove_liquidity_ix = Instruction {
        program_id: dlmm::ID,
        data: remove_data,
        accounts: [remove_main_accounts.to_vec(), remaining_accounts].concat(),
    };

    let compute_budget_ix = ComputeBudgetInstruction::set_compute_unit_limit(2_800_000); // Increased for both operations

    // Create custom config with extended retry settings
    let mut extended_config = transaction_config.clone();
    extended_config.max_retries = Some(20);
    extended_config.skip_preflight = false;
    extended_config.preflight_commitment = Some(anchor_client::solana_sdk::commitment_config::CommitmentConfig::confirmed().commitment);

    let request_builder = program.request();
    let signature = request_builder
        .instruction(compute_budget_ix)
        .instruction(add_liquidity_ix)
        .instruction(remove_liquidity_ix)
        .send_with_spinner_and_config(extended_config)
        .await?;

    // Get token balances after transaction
    let token_x_balance_after = get_token_account_balance(&rpc_client, user_token_x).await?;
    let token_y_balance_after = get_token_account_balance(&rpc_client, user_token_y).await?;
    
    // Create simplified JSON output
    let result = json!({
        "signature": signature.to_string(),
        "token_x_before": token_x_before,
        "token_x_after": token_x_balance_after,
        "token_y_before": token_y_before,
        "token_y_after": token_y_balance_after
    });
    
    println!("{}", serde_json::to_string_pretty(&result)?);

    Ok(())
}
