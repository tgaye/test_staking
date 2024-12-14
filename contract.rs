use anchor_lang::prelude::*;
use anchor_spl::{
    token::{self, Token, TokenAccount, Transfer},
    associated_token::AssociatedToken,
};

pub fn get_agent_pool_pda(
    agent: &Pubkey,
    program_id: &Pubkey,
) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[b"agent_pool", agent.as_ref()],
        program_id,
    )
}

pub fn get_stake_position_pda(
    user: &Pubkey,
    pool: &Pubkey,
    program_id: &Pubkey,
) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[b"stake", user.as_ref(), pool.as_ref()],
        program_id,
    )
}

declare_id!("YOUR_PROGRAM_ID");

// Module-level constants
const MIN_STAKE_SOL: u64 = 1_000_000_000; // 1 SOL
const UNSTAKE_FEE_BPS: u16 = 1000; // 10% on unstake
const STAKE_FEE_BPS: u16 = 300; // 3% on initial stake
const MIN_STAKE_DURATION: i64 = 3600; // 1 hour minimum
const MIN_SHARE_BPS: u64 = 10; // 0.1% minimum share
const MAX_TRADE_SIZE_BPS: u16 = 2000; // 20% max per trade
const DUST_THRESHOLD: u64 = 1_000; // 0.001 SOL
pub const RAYDIUM_PROGRAM_ID: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"; 

#[error_code]
pub enum ErrorCode {
    Unauthorized,
    StakeTooSmall,
    RaydiumError,
    InvalidShare,
    TradeSizeTooLarge,
    PoolPaused,
    MathOverflow,
    ShareTooSmall,
    StakeDurationNotMet,
    DustAmount,
    EmergencyOnly,
}

#[account]
pub struct AgentPool {
    pub agent: Pubkey,
    pub total_staked: u64,
    pub fee_destination: Pubkey,
    pub vault: Pubkey,
    pub paused: bool,
    pub total_shares_bps: u64, 
    pub bump: u8,
    pub emergency_mode: bool,
}

#[account]
pub struct StakePosition {
    pub owner: Pubkey,
    pub agent_pool: Pubkey,
    pub initial_stake: u64,
    pub share_bps: u64,
    pub stake_timestamp: i64,
    pub bump: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize)]
pub struct RaydiumSwap {
    pub amount_in: u64,
    pub min_amount_out: u64,
}

#[program]
pub mod unified_stake_trading {
    use super::*;

    pub fn initialize_agent_pool(ctx: Context<InitializeAgentPool>) -> Result<()> {
        let pool = &mut ctx.accounts.agent_pool;
        pool.agent = ctx.accounts.agent.key();
        pool.total_staked = 0;
        pool.vault = ctx.accounts.pool_vault.key();
        pool.paused = false;
        pool.total_shares_bps = 0;
        pool.bump = *ctx.bumps.get("agent_pool").unwrap();
        Ok(())
    }

    pub fn stake(ctx: Context<Stake>, amount: u64) -> Result<()> {
        require!(amount >= MIN_STAKE_SOL, ErrorCode::StakeTooSmall);
        require!(!ctx.accounts.agent_pool.paused, ErrorCode::PoolPaused);

        let stake_fee = (amount * STAKE_FEE_BPS as u64) / 10000;
        let stake_amount = amount - stake_fee;

        // Calculate share of pool
        let pool = &mut ctx.accounts.agent_pool;
        let position = &mut ctx.accounts.stake_position;
        
        let share_bps = if pool.total_staked == 0 {
            10000
        } else {
            ((stake_amount as u128 * 10000) / (pool.total_staked + stake_amount) as u128) as u64
        };

        require!(share_bps >= MIN_SHARE_BPS, ErrorCode::ShareTooSmall);
        require!(pool.total_shares_bps + share_bps <= 10000, ErrorCode::InvalidShare);    

        // Transfer stake amount to pool vault
        let transfer_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.user_token_account.to_account_info(),
                to: ctx.accounts.pool_vault.to_account_info(),
                authority: ctx.accounts.user.to_account_info(),
            },
        );
        token::transfer(transfer_ctx, stake_amount)?;

        // Transfer fee
        let fee_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.user_token_account.to_account_info(),
                to: ctx.accounts.fee_account.to_account_info(),
                authority: ctx.accounts.user.to_account_info(),
            },
        );
        token::transfer(fee_ctx, stake_fee)?;

        // Update pool state
        pool.total_staked = pool.total_staked.checked_add(stake_amount).ok_or(ErrorCode::MathOverflow)?;
        pool.total_shares_bps = pool.total_shares_bps.checked_add(share_bps).ok_or(ErrorCode::MathOverflow)?;

        // Initialize stake position
        position.owner = ctx.accounts.user.key();
        position.agent_pool = pool.key();
        position.initial_stake = stake_amount;
        position.share_bps = share_bps;
        position.stake_timestamp = Clock::get()?.unix_timestamp;
        position.bump = *ctx.bumps.get("stake_position").unwrap();

        Ok(())
    }

    pub fn execute_trade(ctx: Context<ExecuteTrade>, swap: RaydiumSwap) -> Result<()> {
        let pool = &ctx.accounts.agent_pool;
        require!(!pool.paused, ErrorCode::PoolPaused);
        require!(pool.agent == ctx.accounts.agent.key(), ErrorCode::Unauthorized);
        
        // Verify trade size
        let trade_size_bps = ((swap.amount_in as u128 * 10000) / pool.total_staked as u128) as u16;
        require!(trade_size_bps <= MAX_TRADE_SIZE_BPS, ErrorCode::TradeSizeTooLarge);

        let swap_ix = raydium_amm::instruction::swap(
            ctx.accounts.raydium_program.key(),
            ctx.accounts.pool_vault.key(),
            ctx.accounts.token_a_vault.key(),
            ctx.accounts.token_b_vault.key(),
            ctx.accounts.amm_pool.key(),
            ctx.accounts.amm.key(),
            swap.amount_in,
            swap.min_amount_out,
        );

        solana_program::program::invoke(
            &swap_ix,
            &[
                ctx.accounts.pool_vault.to_account_info(),
                ctx.accounts.token_a_vault.to_account_info(),
                ctx.accounts.token_b_vault.to_account_info(),
                ctx.accounts.amm_pool.to_account_info(),
                ctx.accounts.amm.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
            ],
        ).map_err(|_| ErrorCode::RaydiumError)?;

        Ok(())
    }

    pub fn withdraw(ctx: Context<Withdraw>) -> Result<()> {
        let pool = &ctx.accounts.agent_pool;
        let position = &ctx.accounts.stake_position;
        
        // Check lock duration unless emergency
        if !pool.emergency_mode {
            let current_time = Clock::get()?.unix_timestamp;
            require!(
                current_time >= position.stake_timestamp + MIN_STAKE_DURATION,
                ErrorCode::StakeDurationNotMet
            );
        }
    
        let current_pool_balance = ctx.accounts.pool_vault.amount;
        let share_amount = (current_pool_balance * position.share_bps) / 10000;
        
        // Handle dust amounts
        if share_amount < DUST_THRESHOLD {
            return Err(ErrorCode::DustAmount.into());
        }
        
        // Calculate fee on profits
        let profit = if share_amount > position.initial_stake {
            share_amount - position.initial_stake
        } else {
            0
        };
        
        let fee = (profit * UNSTAKE_FEE_BPS as u64) / 10000;
        let withdrawal_amount = share_amount - fee;

        // Transfer to user
        let transfer_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.pool_vault.to_account_info(),
                to: ctx.accounts.user_token_account.to_account_info(),
                authority: ctx.accounts.agent_pool.to_account_info(),
            },
        );
        token::transfer(transfer_ctx, withdrawal_amount)?;

        // Transfer fee if any
        if fee > 0 {
            let fee_ctx = CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.pool_vault.to_account_info(),
                    to: ctx.accounts.fee_account.to_account_info(),
                    authority: ctx.accounts.agent_pool.to_account_info(),
                },
            );
            token::transfer(fee_ctx, fee)?;
        }

        // Close stake position
        let pool = &mut ctx.accounts.agent_pool;
        pool.total_shares_bps = pool.total_shares_bps.checked_sub(position.share_bps).ok_or(ErrorCode::MathOverflow)?;
        pool.total_staked = pool.total_staked.checked_sub(withdrawal_amount + fee).ok_or(ErrorCode::MathOverflow)?;

        Ok(())
    }
}

#[derive(Accounts)]
pub struct InitializeAgentPool<'info> {
    #[account(
        init,
        payer = agent,
        space = 8 + std::mem::size_of::<AgentPool>(),
        seeds = [b"agent_pool", agent.key().as_ref()],
        bump
    )]
    pub agent_pool: Account<'info, AgentPool>,
    
    #[account(mut)]
    pub agent: Signer<'info>,
    
    #[account(
        init,
        payer = agent,
        space = 8 + std::mem::size_of::<TokenAccount>(),
        seeds = [b"pool_vault", agent.key().as_ref()],
        bump
    )]
    pub pool_vault: Account<'info, TokenAccount>,
    
    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Stake<'info> {
    #[account(mut)]
    pub agent_pool: Account<'info, AgentPool>,
    
    #[account(
        init,
        payer = user,
        space = 8 + std::mem::size_of::<StakePosition>(),
        seeds = [b"stake", user.key().as_ref(), agent_pool.key().as_ref()],
        bump
    )]
    pub stake_position: Account<'info, StakePosition>,
    
    #[account(mut)]
    pub user: Signer<'info>,
    
    #[account(mut)]
    pub pool_vault: Account<'info, TokenAccount>,
    
    #[account(mut)]
    pub user_token_account: Account<'info, TokenAccount>,
    
    #[account(mut)]
    pub fee_account: Account<'info, TokenAccount>,
    
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ExecuteTrade<'info> {
    #[account(mut)]
    pub agent_pool: Account<'info, AgentPool>,
    
    #[account(
        constraint = agent_pool.agent == agent.key() @ ErrorCode::Unauthorized
    )]
    pub agent: Signer<'info>,
    
    #[account(mut)]
    pub pool_vault: Account<'info, TokenAccount>,
    
    #[account(mut)]
    pub token_a_vault: Account<'info, TokenAccount>,
    
    #[account(mut)]
    pub token_b_vault: Account<'info, TokenAccount>,
    
    #[account(mut)]
    pub amm_pool: Account<'info, TokenAccount>,
    
    /// CHECK: Validated in CPI
    pub amm: AccountInfo<'info>,
    
    /// CHECK: Validated in CPI
    pub raydium_program: AccountInfo<'info>,
    
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(mut)]
    pub agent_pool: Account<'info, AgentPool>,
    
    #[account(
        mut,
        has_one = owner,
        seeds = [b"stake", owner.key().as_ref(), agent_pool.key().as_ref()],
        bump = stake_position.bump,
        close = owner
    )]
    pub stake_position: Account<'info, StakePosition>,
    
    #[account(mut)]
    pub owner: Signer<'info>,
    
    #[account(mut)]
    pub pool_vault: Account<'info, TokenAccount>,
    
    #[account(mut)]
    pub user_token_account: Account<'info, TokenAccount>,
    
    #[account(mut)]
    pub fee_account: Account<'info, TokenAccount>,
    
    pub token_program: Program<'info, Token>,
}