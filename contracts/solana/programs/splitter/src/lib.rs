//! Crown splitter: the immutable donate notary executed inside the donor's
//! transaction (docs/core-spec.md §3).
//!
//! One direct payer -> streamer transfer of the whole gross; the program
//! takes no fee and never owns tokens for a single slot. The settlement
//! event goes out via event-CPI, not logs.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked,
};

include!(concat!(env!("OUT_DIR"), "/deploy_params.rs"));

declare_id!("DDSeyx684iU9agHbXExwS3NstLvQeLKZcJWcJFSh1VDA");

#[program]
pub mod splitter {
    use super::*;

    /// Moves `gross` USDC minor units from the payer's token account straight
    /// to the streamer's ATA — the whole amount, no fee — then one `Settled`
    /// event. Reverts whole if the transfer fails.
    pub fn donate(ctx: Context<Donate>, gross: u64) -> Result<()> {
        require!(gross > 0, SplitterError::ZeroDonation);

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
            gross,
            decimals,
        )?;

        emit_cpi!(Settled {
            payer: ctx.accounts.payer.key(),
            streamer: ctx.accounts.streamer.key(),
            gross,
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
    pub token_program: Interface<'info, TokenInterface>,
}

/// The settlement, as the indexer reads it back from the chain.
#[event]
pub struct Settled {
    pub payer: Pubkey,
    pub streamer: Pubkey,
    pub gross: u64,
}

#[error_code]
pub enum SplitterError {
    #[msg("zero donation: nothing to settle")]
    ZeroDonation,
    #[msg("mint is not the pinned USDC mint")]
    WrongMint,
}
