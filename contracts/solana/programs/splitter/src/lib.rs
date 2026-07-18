//! Crown splitter: the immutable donate program executed inside the donor's
//! transaction (docs/core-spec.md §3).
//!
//! One direct donor -> recipient transfer of the whole gross; the program
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

    /// Moves `gross` USDC minor units from the donor's token account straight
    /// to the recipient's ATA — the whole amount, no fee — then one `Settled`
    /// event. Reverts whole if the transfer fails.
    pub fn donate(ctx: Context<Donate>, gross: u64) -> Result<()> {
        require!(gross > 0, SplitterError::ZeroDonation);

        let decimals = ctx.accounts.mint.decimals;
        transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.donor_usdc.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.recipient_usdc.to_account_info(),
                    authority: ctx.accounts.donor.to_account_info(),
                },
            ),
            gross,
            decimals,
        )?;

        emit_cpi!(Settled {
            donor: ctx.accounts.donor.key(),
            recipient: ctx.accounts.recipient.key(),
            gross,
        });
        Ok(())
    }
}

#[event_cpi]
#[derive(Accounts)]
pub struct Donate<'info> {
    /// The donor. Tokens move only out of an account this signer owns;
    /// reputation is credited to the wallet that actually paid.
    pub donor: Signer<'info>,
    /// Recipient wallet: ATA derivation seed and the event identity.
    pub recipient: SystemAccount<'info>,
    #[account(address = USDC_MINT @ SplitterError::WrongMint)]
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        token::mint = mint,
        token::authority = donor,
        token::token_program = token_program,
    )]
    pub donor_usdc: InterfaceAccount<'info, TokenAccount>,
    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = recipient,
        associated_token::token_program = token_program,
    )]
    pub recipient_usdc: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}

/// The settlement, as the indexer reads it back from the chain.
#[event]
pub struct Settled {
    pub donor: Pubkey,
    pub recipient: Pubkey,
    pub gross: u64,
}

#[error_code]
pub enum SplitterError {
    #[msg("zero donation: nothing to settle")]
    ZeroDonation,
    #[msg("mint is not the pinned USDC mint")]
    WrongMint,
}
