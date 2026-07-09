//! Crown splitter: immutable 97/3 split executed inside the donor's
//! transaction (docs/core-spec.md §3).
//!
//! Two direct payer -> recipient transfers; the program never owns tokens
//! for a single slot. The settlement event goes out via event-CPI, not logs.

use anchor_lang::prelude::*;
use anchor_spl::associated_token::get_associated_token_address_with_program_id;
use anchor_spl::token_interface::{
    Mint, TokenAccount, TokenInterface, TransferChecked, transfer_checked,
};

include!(concat!(env!("OUT_DIR"), "/deploy_params.rs"));

declare_id!("3R4dk7uuLt5rnuD95roDhQkt2ZKV9xMAFjfx1Eb96nxP");

pub const BPS_DENOMINATOR: u64 = 10_000;

/// fee = gross * FEE_BPS / 10000 (floor), payout = gross - fee, so
/// payout + fee == gross exactly. `None` when gross is below the fee floor:
/// a fee of zero would emit reputation for free.
pub fn split(gross: u64) -> Option<(u64, u64)> {
    let fee_wide = u128::from(gross)
        .checked_mul(u128::from(FEE_BPS))?
        .checked_div(u128::from(BPS_DENOMINATOR))?;
    let fee = u64::try_from(fee_wide).ok()?;
    if fee == 0 {
        return None;
    }
    let payout = gross.checked_sub(fee)?;
    Some((payout, fee))
}

#[program]
pub mod splitter {
    use super::*;

    /// Splits `gross` USDC minor units from the payer's token account:
    /// payout straight to the streamer's ATA, fee straight to the treasury's
    /// ATA, then one `Settled` event. Reverts whole if any transfer fails.
    pub fn donate(ctx: Context<Donate>, gross: u64) -> Result<()> {
        let (payout, fee) = split(gross).ok_or(SplitterError::BelowFeeFloor)?;

        let decimals = ctx.accounts.mint.decimals;
        transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.payer_usdc.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.streamer_usdc.to_account_info(),
                    authority: ctx.accounts.payer.to_account_info(),
                },
            ),
            payout,
            decimals,
        )?;
        transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.payer_usdc.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.treasury_usdc.to_account_info(),
                    authority: ctx.accounts.payer.to_account_info(),
                },
            ),
            fee,
            decimals,
        )?;

        emit_cpi!(Settled {
            payer: ctx.accounts.payer.key(),
            streamer: ctx.accounts.streamer.key(),
            gross,
            fee,
        });
        Ok(())
    }
}

#[event_cpi]
#[derive(Accounts)]
pub struct Donate<'info> {
    /// The funder. Tokens move only out of an account this signer owns;
    /// reputation is credited to the wallet that actually paid.
    pub payer: Signer<'info>,
    /// Streamer wallet: ATA derivation seed and the event identity.
    pub streamer: SystemAccount<'info>,
    #[account(address = USDC_MINT @ SplitterError::WrongMint)]
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        token::mint = mint,
        token::authority = payer,
        token::token_program = token_program,
    )]
    pub payer_usdc: InterfaceAccount<'info, TokenAccount>,
    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = streamer,
        associated_token::token_program = token_program,
    )]
    pub streamer_usdc: InterfaceAccount<'info, TokenAccount>,
    #[account(
        mut,
        address = get_associated_token_address_with_program_id(
            &TREASURY,
            &mint.key(),
            &token_program.key(),
        ) @ SplitterError::WrongTreasuryAccount,
    )]
    pub treasury_usdc: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}

/// The settlement, as the indexer reads it back from the chain.
#[event]
pub struct Settled {
    pub payer: Pubkey,
    pub streamer: Pubkey,
    pub gross: u64,
    pub fee: u64,
}

#[error_code]
pub enum SplitterError {
    #[msg("gross below fee floor: fee rounds to zero")]
    BelowFeeFloor,
    #[msg("mint is not the pinned USDC mint")]
    WrongMint,
    #[msg("treasury account is not the treasury USDC ATA")]
    WrongTreasuryAccount,
}
