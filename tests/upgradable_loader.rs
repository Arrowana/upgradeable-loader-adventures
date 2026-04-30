use {
    anyhow::{anyhow, bail, Context, Result},
    bincode::deserialize,
    solana_commitment_config::CommitmentConfig,
    solana_keypair::{read_keypair_file, Keypair},
    solana_loader_v3_interface::{
        instruction as loader_instruction, instruction::UpgradeableLoaderInstruction,
        state::UpgradeableLoaderState,
    },
    solana_pubkey::{Pubkey, Pubkey as Address},
    solana_rpc_client::rpc_client::RpcClient,
    solana_rpc_client_api::{
        client_error::{Error as RpcClientError, ErrorKind},
        request::{RpcError, RpcResponseErrorData},
    },
    solana_sdk_ids::bpf_loader_upgradeable,
    solana_signer::Signer,
    solana_system_interface::instruction as system_instruction,
    solana_transaction::{Instruction, Transaction},
    std::{
        fs,
        io::Read,
        net::TcpListener,
        path::PathBuf,
        process::{Child, Command, Stdio},
        thread::sleep,
        time::{Duration, Instant},
    },
    tempfile::TempDir,
};

const AIRDROP_LAMPORTS: u64 = 10_000_000_000;
const WRITE_CHUNK_LEN: usize = 700;

struct TestValidator {
    child: Child,
    _ledger_dir: TempDir,
    rpc_url: String,
}

impl TestValidator {
    fn start() -> Result<Self> {
        let ledger_dir = TempDir::new().context("create validator ledger dir")?;
        let rpc_listener = TcpListener::bind("127.0.0.1:0").context("bind ephemeral rpc port")?;
        let rpc_port = rpc_listener
            .local_addr()
            .context("read ephemeral rpc port")?
            .port();
        let rpc_ws_port = rpc_port
            .checked_add(1)
            .ok_or_else(|| anyhow!("rpc websocket port overflow for {rpc_port}"))?;
        drop(rpc_listener);

        let faucet_port = loop {
            let faucet_listener =
                TcpListener::bind("127.0.0.1:0").context("bind ephemeral faucet port")?;
            let port = faucet_listener
                .local_addr()
                .context("read ephemeral faucet port")?
                .port();
            if port != rpc_port && port != rpc_ws_port {
                drop(faucet_listener);
                break port;
            }
        };
        let gossip_port = loop {
            let gossip_listener =
                TcpListener::bind("127.0.0.1:0").context("bind ephemeral gossip port")?;
            let port = gossip_listener
                .local_addr()
                .context("read ephemeral gossip port")?
                .port();
            if port != rpc_port && port != rpc_ws_port && port != faucet_port {
                drop(gossip_listener);
                break port;
            }
        };
        let rpc_url = format!("http://127.0.0.1:{rpc_port}");
        let program_id = test_program_id();
        let memo_program_path = memo_program_path();
        let upgrade_authority_path = test_upgrade_authority_path();

        let mut child = Command::new("solana-test-validator")
            .arg("--reset")
            .arg("--quiet")
            .arg("--ledger")
            .arg(ledger_dir.path())
            .arg("--bind-address")
            .arg("127.0.0.1")
            .arg("--gossip-port")
            .arg(gossip_port.to_string())
            .arg("--rpc-port")
            .arg(rpc_port.to_string())
            .arg("--faucet-port")
            .arg(faucet_port.to_string())
            .arg("--upgradeable-program")
            .arg(program_id.to_string())
            .arg(&memo_program_path)
            .arg(&upgrade_authority_path)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn solana-test-validator")?;

        let client = RpcClient::new_with_commitment(rpc_url.clone(), CommitmentConfig::processed());
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if client.get_latest_blockhash().is_ok() {
                break;
            }

            if let Some(status) = child.try_wait().context("poll solana-test-validator")? {
                let mut stderr = String::new();
                if let Some(mut handle) = child.stderr.take() {
                    let _ = handle.read_to_string(&mut stderr);
                }
                bail!(
                    "solana-test-validator exited early with status {status}: {}",
                    stderr.trim()
                );
            }

            if Instant::now() >= deadline {
                bail!("timed out waiting for solana-test-validator at {rpc_url}");
            }

            sleep(Duration::from_millis(250));
        }

        Ok(Self {
            child,
            _ledger_dir: ledger_dir,
            rpc_url,
        })
    }

    fn rpc_client(&self) -> RpcClient {
        RpcClient::new_with_commitment(self.rpc_url.clone(), CommitmentConfig::processed())
    }
}

impl Drop for TestValidator {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn closed_buffer_can_be_reused_after_close() -> Result<()> {
    let validator = TestValidator::start()?;
    let client = validator.rpc_client();
    let payer = Keypair::new();
    let buffer = Keypair::new();
    let program_bytes = memo_program_bytes()?;

    airdrop(&client, &payer, AIRDROP_LAMPORTS)?;

    create_buffer(
        &client,
        &payer,
        &buffer,
        &payer.pubkey(),
        program_bytes.len(),
    )?;
    assert_buffer_authority(&client, &buffer.pubkey(), Some(payer.pubkey()))?;

    let close_ix = loader_instruction::close(&buffer.pubkey(), &payer.pubkey(), &payer.pubkey());
    send_tx(&client, &payer, &[&payer], vec![close_ix])?;

    create_buffer(
        &client,
        &payer,
        &buffer,
        &payer.pubkey(),
        program_bytes.len(),
    )?;
    assert_buffer_authority(&client, &buffer.pubkey(), Some(payer.pubkey()))?;

    Ok(())
}

#[test]
fn close_with_lamports_for_rent_exemption_tombstones_the_buffer() -> Result<()> {
    let validator = TestValidator::start()?;
    let client = validator.rpc_client();
    let payer = Keypair::new();
    let buffer = Keypair::new();
    let program_bytes = memo_program_bytes()?;

    airdrop(&client, &payer, AIRDROP_LAMPORTS)?;

    create_buffer(
        &client,
        &payer,
        &buffer,
        &payer.pubkey(),
        program_bytes.len(),
    )?;
    let zero_authority = Address::default();
    let buffer_rent = client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_buffer(
            program_bytes.len(),
        ))
        .context("fetch recreated buffer rent exemption")?;
    let close_ix = loader_instruction::close(&buffer.pubkey(), &payer.pubkey(), &payer.pubkey());
    let transfer_ix = system_instruction::transfer(&payer.pubkey(), &buffer.pubkey(), buffer_rent);

    send_tx(&client, &payer, &[&payer], vec![close_ix, transfer_ix])?;

    let buffer_account = client.get_account(&buffer.pubkey()).with_context(|| {
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

    let err = send_tx(&client, &payer, &[&payer], vec![initialize_ix])
        .expect_err("InitializeBuffer should fail on truncated account");
    assert_preflight_log_contains(&err, "account data too small for instruction");

    Ok(())
}

#[test]
fn trailing_system_transfer_keeps_upgraded_buffer_tombstoned() -> Result<()> {
    let validator = TestValidator::start()?;
    let client = validator.rpc_client();
    let payer = Keypair::new();
    let upgrade_buffer = Keypair::new();
    let upgrade_authority = test_upgrade_authority()?;
    let program = test_program_id();
    let program_bytes = memo_program_bytes()?;

    airdrop(&client, &payer, AIRDROP_LAMPORTS)?;

    create_buffer(
        &client,
        &payer,
        &upgrade_buffer,
        &upgrade_authority.pubkey(),
        program_bytes.len(),
    )?;
    write_buffer(
        &client,
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
    let reinstate_rent = client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_buffer(0))
        .context("fetch zero-length buffer rent exemption")?;
    let transfer_ix =
        system_instruction::transfer(&payer.pubkey(), &upgrade_buffer.pubkey(), reinstate_rent);

    send_tx(
        &client,
        &payer,
        &[&payer, &upgrade_authority],
        vec![upgrade_ix, transfer_ix],
    )?;

    let tombstoned_buffer = client
        .get_account(&upgrade_buffer.pubkey())
        .with_context(|| format!("fetch tombstoned buffer {}", upgrade_buffer.pubkey()))?;
    assert_eq!(tombstoned_buffer.lamports, reinstate_rent);
    assert_eq!(
        tombstoned_buffer.data.len(),
        UpgradeableLoaderState::size_of_buffer(0),
    );
    assert_buffer_authority(
        &client,
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
    let validator = TestValidator::start()?;
    let client = validator.rpc_client();
    let payer = Keypair::new();
    let upgrade_buffer = Keypair::new();
    let upgrade_authority = test_upgrade_authority()?;
    let program = test_program_id();
    let program_bytes = memo_program_bytes()?;
    let (pda_authority, _) =
        Pubkey::find_program_address(&[b"arbitrary-buffer-authority"], &program);

    airdrop(&client, &payer, AIRDROP_LAMPORTS)?;

    create_buffer(
        &client,
        &payer,
        &upgrade_buffer,
        &upgrade_authority.pubkey(),
        program_bytes.len(),
    )?;
    write_buffer(
        &client,
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
        &client,
        &payer,
        &[&payer, &upgrade_authority],
        vec![upgrade_ix, set_authority_ix],
    )?;

    client
        .get_account(&upgrade_buffer.pubkey())
        .expect_err("upgraded buffer should be gone after being drained to zero lamports");

    Ok(())
}

fn airdrop(client: &RpcClient, payer: &Keypair, lamports: u64) -> Result<()> {
    let signature = client
        .request_airdrop(&payer.pubkey(), lamports)
        .context("request airdrop")?;
    client
        .poll_for_signature(&signature)
        .context("confirm airdrop")?;
    Ok(())
}

fn create_buffer(
    client: &RpcClient,
    payer: &Keypair,
    buffer: &Keypair,
    authority: &Pubkey,
    program_len: usize,
) -> Result<()> {
    let lamports = client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_buffer(program_len))
        .context("fetch buffer rent exemption")?;
    let instructions = loader_instruction::create_buffer(
        &payer.pubkey(),
        &buffer.pubkey(),
        authority,
        lamports,
        program_len,
    )
    .context("build create_buffer instructions")?;
    send_tx(client, payer, &[payer, buffer], instructions)?;
    Ok(())
}

fn write_buffer(
    client: &RpcClient,
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
        send_tx(client, payer, &signers, vec![write_ix])?;
    }
    Ok(())
}

fn send_tx(
    client: &RpcClient,
    payer: &Keypair,
    signers: &[&Keypair],
    instructions: Vec<Instruction>,
) -> Result<()> {
    let blockhash = client
        .get_latest_blockhash()
        .context("fetch latest blockhash")?;
    let tx = Transaction::new_signed_with_payer(
        &instructions,
        Some(&payer.pubkey()),
        signers,
        blockhash,
    );
    client
        .send_and_confirm_transaction(&tx)
        .map(|_| ())
        .context("send transaction")
}

fn assert_preflight_log_contains(err: &anyhow::Error, needle: &str) {
    let rpc_err = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<RpcClientError>())
        .expect("missing rpc client error");

    let logs = match rpc_err.kind() {
        ErrorKind::RpcError(RpcError::RpcResponseError {
            data: RpcResponseErrorData::SendTransactionPreflightFailure(result),
            ..
        }) => result.logs.as_deref().unwrap_or(&[]),
        other => panic!("expected preflight failure, found {other:?}"),
    };

    assert!(
        logs.iter().any(|log| log.contains(needle)),
        "missing log {needle:?}\nlogs:\n{}",
        logs.join("\n"),
    );
}

fn assert_buffer_authority(
    client: &RpcClient,
    buffer: &Pubkey,
    expected: Option<Pubkey>,
) -> Result<()> {
    let account = client
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

fn test_upgrade_authority_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-upgrade-authority.json")
}

fn test_upgrade_authority() -> Result<Keypair> {
    read_keypair_file(test_upgrade_authority_path())
        .map_err(|err| anyhow!("read test upgrade authority: {err}"))
}

fn memo_program_bytes() -> Result<Vec<u8>> {
    fs::read(memo_program_path()).context("read memo.so")
}
