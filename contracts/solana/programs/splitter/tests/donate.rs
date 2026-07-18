//! DoD tests of the splitter (docs/build-plan.md S2): exact pass-through,
//! zero-donation revert, structural donor, and the no-token-account invariant.

use anchor_lang::solana_program::entrypoint::ProgramResult;
use anchor_lang::{InstructionData, ToAccountMetas};
use anchor_spl::associated_token::get_associated_token_address;
use anchor_spl::token::spl_token;
use solana_program_test::{processor, BanksClient, BanksClientError, ProgramTest};
use solana_sdk::account::Account;
use solana_sdk::account_info::AccountInfo;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{Instruction, InstructionError};
use solana_sdk::program_option::COption;
use solana_sdk::program_pack::Pack;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::{Transaction, TransactionError};

const PAYER_FUNDS: u64 = 1_000_000_000_000; // 1M USDC in minor units

fn process(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    // anchor's entry ties the slice lifetime to AccountInfo's inner lifetime,
    // the test harness passes decoupled ones; shrinking the inner lifetime is
    // sound because entry keeps no reference past the call.
    let accounts =
        unsafe { core::mem::transmute::<&[AccountInfo<'_>], &[AccountInfo<'_>]>(accounts) };
    splitter::entry(program_id, accounts, data)
}

fn event_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], &splitter::ID).0
}

fn packed_mint() -> Vec<u8> {
    let mut data = vec![0u8; spl_token::state::Mint::LEN];
    spl_token::state::Mint::pack(
        spl_token::state::Mint {
            mint_authority: COption::None,
            supply: 10 * PAYER_FUNDS,
            decimals: 6,
            is_initialized: true,
            freeze_authority: COption::None,
        },
        &mut data,
    )
    .unwrap();
    data
}

fn packed_token_account(mint: Pubkey, owner: Pubkey, amount: u64) -> Vec<u8> {
    let mut data = vec![0u8; spl_token::state::Account::LEN];
    spl_token::state::Account::pack(
        spl_token::state::Account {
            mint,
            owner,
            amount,
            delegate: COption::None,
            state: spl_token::state::AccountState::Initialized,
            is_native: COption::None,
            delegated_amount: 0,
            close_authority: COption::None,
        },
        &mut data,
    )
    .unwrap();
    data
}

struct Ctx {
    banks: BanksClient,
    fee_payer: Keypair,
    blockhash: Hash,
    donor: Keypair,
    recipient: Pubkey,
    victim: Pubkey,
    fake_mint: Pubkey,
}

impl Ctx {
    async fn new() -> Self {
        let donor = Keypair::new();
        let recipient = Pubkey::new_unique();
        let victim = Pubkey::new_unique();
        let fake_mint = Pubkey::new_unique();

        let mut pt = ProgramTest::new("splitter", splitter::ID, processor!(process));
        let spl = |data: Vec<u8>| Account {
            lamports: 1_000_000_000,
            data,
            owner: spl_token::ID,
            executable: false,
            rent_epoch: 0,
        };
        pt.add_account(splitter::USDC_MINT, spl(packed_mint()));
        pt.add_account(fake_mint, spl(packed_mint()));
        for (mint, owner, amount) in [
            (splitter::USDC_MINT, donor.pubkey(), PAYER_FUNDS),
            (splitter::USDC_MINT, recipient, 0),
            (splitter::USDC_MINT, victim, PAYER_FUNDS),
            (fake_mint, donor.pubkey(), PAYER_FUNDS),
            (fake_mint, recipient, 0),
        ] {
            pt.add_account(
                get_associated_token_address(&owner, &mint),
                spl(packed_token_account(mint, owner, amount)),
            );
        }

        let (banks, fee_payer, blockhash) = pt.start().await;
        Self {
            banks,
            fee_payer,
            blockhash,
            donor,
            recipient,
            victim,
            fake_mint,
        }
    }

    fn donate_ix(&self, gross: u64) -> Instruction {
        self.donate_ix_with(
            splitter::USDC_MINT,
            get_associated_token_address(&self.donor.pubkey(), &splitter::USDC_MINT),
            gross,
        )
    }

    fn donate_ix_with(&self, mint: Pubkey, donor_usdc: Pubkey, gross: u64) -> Instruction {
        let accounts = splitter::accounts::Donate {
            donor: self.donor.pubkey(),
            recipient: self.recipient,
            mint,
            donor_usdc,
            recipient_usdc: get_associated_token_address(&self.recipient, &mint),
            token_program: spl_token::ID,
            event_authority: event_authority(),
            program: splitter::ID,
        };
        Instruction {
            program_id: splitter::ID,
            accounts: accounts.to_account_metas(None),
            data: splitter::instruction::Donate { gross }.data(),
        }
    }

    async fn send(&mut self, ix: Instruction) -> Result<(), BanksClientError> {
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&self.fee_payer.pubkey()),
            &[&self.fee_payer, &self.donor],
            self.blockhash,
        );
        self.banks.process_transaction(tx).await
    }

    async fn usdc_balance(&mut self, owner: Pubkey) -> u64 {
        let ata = get_associated_token_address(&owner, &splitter::USDC_MINT);
        let account = self.banks.get_account(ata).await.unwrap().unwrap();
        spl_token::state::Account::unpack(&account.data)
            .unwrap()
            .amount
    }
}

fn custom_error(err: &BanksClientError) -> Option<u32> {
    match err {
        BanksClientError::TransactionError(TransactionError::InstructionError(
            _,
            InstructionError::Custom(code),
        )) => Some(*code),
        _ => None,
    }
}

// out == in: the recipient receives exactly gross, the donor loses exactly
// gross, nothing lands anywhere else.
#[tokio::test]
async fn donate_transfers_gross_exactly() {
    let mut ctx = Ctx::new().await;
    let gross = 1_000_000; // 1 USDC
    ctx.send(ctx.donate_ix(gross)).await.unwrap();

    assert_eq!(ctx.usdc_balance(ctx.recipient).await, gross);
    assert_eq!(
        ctx.usdc_balance(ctx.donor.pubkey()).await,
        PAYER_FUNDS - gross
    );
}

// A zero donation reverts: an empty transfer must not mint an event.
#[tokio::test]
async fn zero_donation_reverts() {
    let mut ctx = Ctx::new().await;

    ctx.send(ctx.donate_ix(1)).await.unwrap();
    assert_eq!(ctx.usdc_balance(ctx.recipient).await, 1);

    let err = ctx.send(ctx.donate_ix(0)).await.unwrap_err();
    assert_eq!(custom_error(&err), Some(6000));
    assert_eq!(ctx.usdc_balance(ctx.recipient).await, 1);
}

// On-chain pass-through is exact over a spread of values: totals agree on
// both ends after a series of donations.
#[tokio::test]
async fn donations_accumulate_exactly() {
    let mut ctx = Ctx::new().await;
    let mut spent = 0u64;

    let mut gross = 1u64;
    while gross < 40_000_000_000 {
        ctx.send(ctx.donate_ix(gross)).await.unwrap();
        spent += gross;
        gross = gross * 7 + 13;
    }

    assert_eq!(ctx.usdc_balance(ctx.recipient).await, spent);
    assert_eq!(
        ctx.usdc_balance(ctx.donor.pubkey()).await,
        PAYER_FUNDS - spent
    );
}

// Zero-balance invariant, structurally: neither the program nor its only PDA
// has a token account at all — there is nothing to hold funds with.
#[tokio::test]
async fn program_and_pda_have_no_token_account() {
    let mut ctx = Ctx::new().await;
    ctx.send(ctx.donate_ix(1_000_000)).await.unwrap();

    for owner in [splitter::ID, event_authority()] {
        let ata = get_associated_token_address(&owner, &splitter::USDC_MINT);
        assert!(ctx.banks.get_account(ata).await.unwrap().is_none());
    }
}

// Structural donor, leg 1: a signer cannot pull from a token account they
// don't own — reputation cannot be funded with someone else's money.
#[tokio::test]
async fn cannot_pull_from_foreign_token_account() {
    let mut ctx = Ctx::new().await;
    let victim_ata = get_associated_token_address(&ctx.victim, &splitter::USDC_MINT);
    let err = ctx
        .send(ctx.donate_ix_with(splitter::USDC_MINT, victim_ata, 1_000_000))
        .await
        .unwrap_err();
    assert!(custom_error(&err).is_some());
    assert_eq!(ctx.usdc_balance(ctx.victim).await, PAYER_FUNDS);
}

// Structural donor, leg 2: without the donor's signature nothing moves.
#[tokio::test]
async fn donor_signature_is_required() {
    let mut ctx = Ctx::new().await;
    let mut ix = ctx.donate_ix(1_000_000);
    for meta in &mut ix.accounts {
        if meta.pubkey == ctx.donor.pubkey() {
            meta.is_signer = false;
        }
    }
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.fee_payer.pubkey()),
        &[&ctx.fee_payer],
        ctx.blockhash,
    );
    assert!(ctx.banks.process_transaction(tx).await.is_err());
    assert_eq!(ctx.usdc_balance(ctx.donor.pubkey()).await, PAYER_FUNDS);
}

// Only the pinned USDC mint is accepted.
#[tokio::test]
async fn wrong_mint_is_rejected() {
    let mut ctx = Ctx::new().await;
    let fake = ctx.fake_mint;
    let donor_fake_ata = get_associated_token_address(&ctx.donor.pubkey(), &fake);
    let err = ctx
        .send(ctx.donate_ix_with(fake, donor_fake_ata, 1_000_000))
        .await
        .unwrap_err();
    assert_eq!(custom_error(&err), Some(6001));
}
