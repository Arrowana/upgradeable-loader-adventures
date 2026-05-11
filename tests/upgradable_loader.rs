use {
    anyhow::{anyhow, bail, Context, Result},
    bincode::{deserialize, serialize},
    litesvm::{types::FailedTransactionMetadata, LiteSVM},
    solana_keypair::Keypair,
    solana_loader_v3_interface::{
        get_program_data_address, instruction as loader_instruction,
        instruction::UpgradeableLoaderInstruction, state::UpgradeableLoaderState,
    },
    solana_pubkey::{Pubkey, Pubkey as Address},
    solana_sdk_ids::bpf_loader_upgradeable,
    solana_signer::Signer,
    solana_system_interface::instruction as system_instruction,
    solana_transaction::{Instruction, Transaction},
    std::{fs, path::PathBuf},
};

const AIRDROP_LAMPORTS: u64 = 10_000_000_000;
const WRITE_CHUNK_LEN: usize = 700;

#[test]
fn closed_buffer_can_be_reused_after_close() -> Result<()> {
    let (mut svm, _) = test_svm()?;
    let payer = Keypair::new();
    let buffer = Keypair::new();
    let program_bytes = memo_program_bytes()?;

    airdrop(&mut svm, &payer, AIRDROP_LAMPORTS)?;

    create_buffer(
        &mut svm,
        &payer,
        &buffer,
        &payer.pubkey(),
        program_bytes.len(),
    )?;
    assert_buffer_authority(&svm, &buffer.pubkey(), Some(payer.pubkey()))?;

    let close_ix = loader_instruction::close(&buffer.pubkey(), &payer.pubkey(), &payer.pubkey());
    send_tx(&mut svm, &payer, &[&payer], vec![close_ix])?;

    create_buffer(
        &mut svm,
        &payer,
        &buffer,
        &payer.pubkey(),
        program_bytes.len(),
    )?;
    assert_buffer_authority(&svm, &buffer.pubkey(), Some(payer.pubkey()))?;

    Ok(())
}

#[test]
fn close_with_lamports_for_rent_exemption_tombstones_the_buffer() -> Result<()> {
    let (mut svm, _) = test_svm()?;
    let payer = Keypair::new();
    let buffer = Keypair::new();
    let program_bytes = memo_program_bytes()?;

    airdrop(&mut svm, &payer, AIRDROP_LAMPORTS)?;

    create_buffer(
        &mut svm,
        &payer,
        &buffer,
        &payer.pubkey(),
        program_bytes.len(),
    )?;
    let zero_authority = Address::default();
    let buffer_rent = svm.minimum_balance_for_rent_exemption(
        UpgradeableLoaderState::size_of_buffer(program_bytes.len()),
    );
    let close_ix = loader_instruction::close(&buffer.pubkey(), &payer.pubkey(), &payer.pubkey());
    let transfer_ix = system_instruction::transfer(&payer.pubkey(), &buffer.pubkey(), buffer_rent);

    send_tx(&mut svm, &payer, &[&payer], vec![close_ix, transfer_ix])?;

    let buffer_account = svm.get_account(&buffer.pubkey()).with_context(|| {
        format!(
            "fetch buffer after failed atomic reinitialize {}",
            buffer.pubkey()
        )
    })?;
    assert_eq!(buffer_account.data.len(), 4);
    match parse_loader_state(&buffer_account.data)? {
        UpgradeableLoaderState::Uninitialized => {}
        other => bail!("expected Uninitialized, found {other:?}"),
    }

    let initialize_ix = Instruction::new_with_bincode(
        bpf_loader_upgradeable::id(),
        &UpgradeableLoaderInstruction::InitializeBuffer,
        vec![
            solana_transaction::AccountMeta::new(buffer.pubkey(), false),
            solana_transaction::AccountMeta::new_readonly(zero_authority, false),
        ],
    );

    let err = send_tx_result(&mut svm, &payer, &[&payer], vec![initialize_ix])
        .expect_err("InitializeBuffer should fail on truncated account");
    assert_transaction_log_contains(&err, "account data too small for instruction");

    Ok(())
}

#[test]
fn trailing_system_transfer_keeps_upgraded_buffer_tombstoned() -> Result<()> {
    let (mut svm, upgrade_authority) = test_svm()?;
    let payer = Keypair::new();
    let upgrade_buffer = Keypair::new();
    let program = test_program_id();
    let program_bytes = memo_program_bytes()?;

    airdrop(&mut svm, &payer, AIRDROP_LAMPORTS)?;

    create_buffer(
        &mut svm,
        &payer,
        &upgrade_buffer,
        &upgrade_authority.pubkey(),
        program_bytes.len(),
    )?;
    write_buffer(
        &mut svm,
        &payer,
        &upgrade_buffer.pubkey(),
        &upgrade_authority,
        &program_bytes,
    )?;

    let upgrade_ix = loader_instruction::upgrade(
        &program,
        &upgrade_buffer.pubkey(),
        &upgrade_authority.pubkey(),
        &payer.pubkey(),
    );
    let reinstate_rent =
        svm.minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_buffer(0));
    let transfer_ix =
        system_instruction::transfer(&payer.pubkey(), &upgrade_buffer.pubkey(), reinstate_rent);

    send_tx(
        &mut svm,
        &payer,
        &[&payer, &upgrade_authority],
        vec![upgrade_ix, transfer_ix],
    )?;

    let tombstoned_buffer = svm
        .get_account(&upgrade_buffer.pubkey())
        .with_context(|| format!("fetch tombstoned buffer {}", upgrade_buffer.pubkey()))?;
    assert_eq!(tombstoned_buffer.lamports, reinstate_rent);
    assert_eq!(
        tombstoned_buffer.data.len(),
        UpgradeableLoaderState::size_of_buffer(0),
    );
    assert_buffer_authority(
        &svm,
        &upgrade_buffer.pubkey(),
        Some(upgrade_authority.pubkey()),
    )?;
    match parse_loader_state(&tombstoned_buffer.data)? {
        UpgradeableLoaderState::Buffer { authority_address } => {
            assert_eq!(authority_address, Some(upgrade_authority.pubkey()));
        }
        other => bail!("expected Buffer, found {other:?}"),
    }

    Ok(())
}

#[test]
fn upgrade_then_sets_buffer_authority_to_pda() -> Result<()> {
    let (mut svm, upgrade_authority) = test_svm()?;
    let payer = Keypair::new();
    let upgrade_buffer = Keypair::new();
    let program = test_program_id();
    let program_bytes = memo_program_bytes()?;
    let (pda_authority, _) =
        Pubkey::find_program_address(&[b"arbitrary-buffer-authority"], &program);

    airdrop(&mut svm, &payer, AIRDROP_LAMPORTS)?;

    create_buffer(
        &mut svm,
        &payer,
        &upgrade_buffer,
        &upgrade_authority.pubkey(),
        program_bytes.len(),
    )?;
    write_buffer(
        &mut svm,
        &payer,
        &upgrade_buffer.pubkey(),
        &upgrade_authority,
        &program_bytes,
    )?;

    let upgrade_ix = loader_instruction::upgrade(
        &program,
        &upgrade_buffer.pubkey(),
        &upgrade_authority.pubkey(),
        &payer.pubkey(),
    );
    let set_authority_ix = loader_instruction::set_buffer_authority(
        &upgrade_buffer.pubkey(),
        &upgrade_authority.pubkey(),
        &pda_authority,
    );

    send_tx(
        &mut svm,
        &payer,
        &[&payer, &upgrade_authority],
        vec![upgrade_ix, set_authority_ix],
    )?;

    assert!(
        svm.get_account(&upgrade_buffer.pubkey()).is_none(),
        "upgraded buffer should be gone after being drained to zero lamports"
    );

    Ok(())
}

fn test_svm() -> Result<(LiteSVM, Keypair)> {
    let mut svm = LiteSVM::new();
    let program_id = test_program_id();
    let upgrade_authority = Keypair::new();

    svm.add_program(program_id, &memo_program_bytes()?)
        .context("add upgradeable memo program")?;
    set_program_upgrade_authority(&mut svm, program_id, upgrade_authority.pubkey())?;
    svm.warp_to_slot(1);

    Ok((svm, upgrade_authority))
}

fn set_program_upgrade_authority(
    svm: &mut LiteSVM,
    program_id: Pubkey,
    authority: Pubkey,
) -> Result<()> {
    let programdata_address = get_program_data_address(&program_id);
    let mut programdata_account = svm
        .get_account(&programdata_address)
        .with_context(|| format!("fetch ProgramData account {programdata_address}"))?;
    let metadata_len = UpgradeableLoaderState::size_of_programdata_metadata();
    let metadata = parse_loader_state(&programdata_account.data)?;
    let slot = match metadata {
        UpgradeableLoaderState::ProgramData { slot, .. } => slot,
        other => bail!("expected ProgramData account, found {other:?}"),
    };

    let mut data = serialize(&UpgradeableLoaderState::ProgramData {
        slot,
        upgrade_authority_address: Some(authority),
    })
    .context("serialize ProgramData metadata")?;
    data.extend_from_slice(&programdata_account.data[metadata_len..]);
    programdata_account.data = data;

    svm.set_account(programdata_address, programdata_account)
        .context("set ProgramData authority")?;

    Ok(())
}

fn airdrop(svm: &mut LiteSVM, payer: &Keypair, lamports: u64) -> Result<()> {
    svm.airdrop(&payer.pubkey(), lamports)
        .map(|_| ())
        .map_err(|err| anyhow!("airdrop failed: {err:?}"))
}

fn create_buffer(
    svm: &mut LiteSVM,
    payer: &Keypair,
    buffer: &Keypair,
    authority: &Pubkey,
    program_len: usize,
) -> Result<()> {
    let lamports =
        svm.minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_buffer(program_len));
    let instructions = loader_instruction::create_buffer(
        &payer.pubkey(),
        &buffer.pubkey(),
        authority,
        lamports,
        program_len,
    )
    .context("build create_buffer instructions")?;
    send_tx(svm, payer, &[payer, buffer], instructions)?;
    Ok(())
}

fn write_buffer(
    svm: &mut LiteSVM,
    payer: &Keypair,
    buffer: &Pubkey,
    authority: &Keypair,
    program_bytes: &[u8],
) -> Result<()> {
    for (offset, chunk) in program_bytes.chunks(WRITE_CHUNK_LEN).enumerate() {
        let byte_offset = offset
            .checked_mul(WRITE_CHUNK_LEN)
            .ok_or_else(|| anyhow!("buffer write offset overflow"))?;
        let write_ix = loader_instruction::write(
            buffer,
            &authority.pubkey(),
            u32::try_from(byte_offset).context("buffer write offset exceeds u32")?,
            chunk.to_vec(),
        );
        let signers = if authority.pubkey() == payer.pubkey() {
            vec![payer]
        } else {
            vec![payer, authority]
        };
        send_tx(svm, payer, &signers, vec![write_ix])?;
    }
    Ok(())
}

fn send_tx(
    svm: &mut LiteSVM,
    payer: &Keypair,
    signers: &[&Keypair],
    instructions: Vec<Instruction>,
) -> Result<()> {
    send_tx_result(svm, payer, signers, instructions)
        .map_err(|err| anyhow!("send transaction failed: {err:?}"))
}

fn send_tx_result(
    svm: &mut LiteSVM,
    payer: &Keypair,
    signers: &[&Keypair],
    instructions: Vec<Instruction>,
) -> std::result::Result<(), FailedTransactionMetadata> {
    let blockhash = svm.latest_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &instructions,
        Some(&payer.pubkey()),
        signers,
        blockhash,
    );
    svm.send_transaction(tx).map(|_| {
        svm.expire_blockhash();
    })
}

fn assert_transaction_log_contains(err: &FailedTransactionMetadata, needle: &str) {
    assert!(
        err.meta.logs.iter().any(|log| log.contains(needle)),
        "missing log {needle:?}\nlogs:\n{}",
        err.meta.logs.join("\n"),
    );
}

fn assert_buffer_authority(svm: &LiteSVM, buffer: &Pubkey, expected: Option<Pubkey>) -> Result<()> {
    let account = svm
        .get_account(buffer)
        .with_context(|| format!("fetch buffer account {buffer}"))?;
    match parse_loader_state(&account.data)? {
        UpgradeableLoaderState::Buffer { authority_address } => {
            assert_eq!(authority_address, expected);
            Ok(())
        }
        other => bail!("expected Buffer, found {other:?}"),
    }
}

fn parse_loader_state(data: &[u8]) -> Result<UpgradeableLoaderState> {
    if data.len() < UpgradeableLoaderState::size_of_uninitialized() {
        bail!("loader account too small: {}", data.len());
    }

    let discriminant = u32::from_le_bytes(
        data[..UpgradeableLoaderState::size_of_uninitialized()]
            .try_into()
            .context("read loader discriminant")?,
    );
    let metadata_len = match discriminant {
        0 => UpgradeableLoaderState::size_of_uninitialized(),
        1 => UpgradeableLoaderState::size_of_buffer_metadata(),
        2 => UpgradeableLoaderState::size_of_program(),
        3 => UpgradeableLoaderState::size_of_programdata_metadata(),
        _ => bail!("unknown loader discriminant {discriminant}"),
    };

    deserialize(
        data.get(..metadata_len)
            .ok_or_else(|| anyhow!("loader metadata truncated at {metadata_len} bytes"))?,
    )
    .context("deserialize loader metadata")
}

fn test_program_id() -> Pubkey {
    Pubkey::from_str_const("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr")
}

fn memo_program_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("memo.so")
}

fn memo_program_bytes() -> Result<Vec<u8>> {
    fs::read(memo_program_path()).context("read memo.so")
}
