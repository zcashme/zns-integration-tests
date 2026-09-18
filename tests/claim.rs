//! Happy-path claim: user pays Treasury `ZNS:claim:forever:alice:<ua>`, mint registers,
//! then `zns-verify` checks the on-chain Name Note.

use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use zns_integration_tests::{
    fund_user, miner_address, pay_claim, publish, registration_txid, registry_commitment_keys,
    wait_for_verified_name_note, zebrad_bin, Mint, User, Zebrad, FIXTURE_HEIGHT,
};

#[tokio::test(flavor = "multi_thread")]
async fn happy_path_claim_alice() -> Result<()> {
    if zebrad_bin().is_none() {
        if std::env::var_os("CI").is_some() {
            bail!("zebrad required in CI — set ZEBRAD_BIN");
        }
        eprintln!("skipping: zebrad not found (set ZEBRAD_BIN or put zebrad on PATH)");
        return Ok(());
    }

    let mint_build = tokio::task::spawn_blocking(Mint::build);

    let miner = miner_address()?;
    let mut zebra = Zebrad::start_with_miner(&miner).await?;
    zebra.generate_blocks(FIXTURE_HEIGHT).await?;
    publish(&mut zebra).await?;

    let user = User::new()?;
    eprintln!("user miner: {}", user.miner_address);
    eprintln!("user UA: {}", user.ua);
    fund_user(&mut zebra, &user).await?;

    let mint_bin = mint_build.await.expect("mint build task")?;
    let mut mint = Mint::start(mint_bin).await?;
    mint.wait_until_live().await?;
    // New tip so the run loop can fire. Sweep is optional: claim uses
    // the ceremony Treasury notes either way.
    zebra.generate_blocks(1).await?;

    let run_loop_deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if !mint.is_running() {
            bail!("mint died after becoming live:\n{}", mint.log_text());
        }
        let log = mint.log_text();
        if mint_submitted_vault_sweep(&log) {
            break;
        }
        if log.contains("vault sweep") && log.contains("failed") {
            eprintln!("mint vault sweep failed; continuing to claim");
            break;
        }
        if log.contains("mint rules applied") {
            break;
        }
        if Instant::now() >= run_loop_deadline {
            bail!(
                "mint did not apply rules after the post-live tip:\n{}",
                mint.log_text()
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    zebra.generate_blocks(1).await?;

    let claim_txid = pay_claim(&mut zebra, &user, "alice").await?;
    eprintln!("claim payment txid {claim_txid}");
    // Confirm the payment, then leave the tip still so mint's
    // `exact_tip == cursor` gate can run intake (a racing tip skips it).
    zebra.generate_blocks(1).await?;

    let deadline = Instant::now() + Duration::from_secs(600);
    let started = Instant::now();
    let mut poked = 0u32;
    loop {
        if !mint.is_running() {
            bail!("mint died while settling claim:\n{}", mint.log_text());
        }
        let log = mint.log_text();
        if mint_rejected_name_note(&log, "alice") {
            bail!("mint rejected alice registration:\n{log}");
        }
        if mint_name_note_in_flight(&log, "alice") {
            eprintln!("mint settled alice");
            let expected_txid = registration_txid(&log, "alice");
            let scan_from = zebra.tip_height().await?;
            let note = wait_for_verified_name_note(&zebra, &mut mint, "alice", scan_from).await?;
            eprintln!(
                "verified Name Note height={} txid={}",
                note.height, note.txid
            );
            if let Some(txid) = expected_txid {
                if note.txid != txid {
                    bail!(
                        "on-chain Name Note txid {} != mint in-flight txid {txid}",
                        note.txid
                    );
                }
            }
            assert_eq!(note.name, "alice");
            assert_eq!(note.action, "claim");
            assert_eq!(note.ua, user.ua);
            assert_eq!(note.expires_at.as_deref(), Some("none"));
            assert_eq!(note.value, 0);
            let (g_d, pk_d) = registry_commitment_keys()?;
            assert_eq!(note.g_d, g_d);
            assert_eq!(note.pk_d, pk_d);
            return Ok(());
        }
        if log.contains("non-request payment") && log.contains(&claim_txid) {
            bail!("mint saw claim tx {claim_txid} as a non-request payment:\n{log}");
        }
        if log.contains("claim not authorized") && log.contains("alice") {
            bail!("mint did not authorize alice:\n{log}");
        }
        if Instant::now() >= deadline {
            bail!("mint did not register alice within 600s:\n{log}");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
        if poked == 0 && started.elapsed() > Duration::from_secs(90) {
            zebra.generate_blocks(1).await?;
            poked = 1;
        }
    }
}

fn mint_submitted_vault_sweep(log: &str) -> bool {
    log.lines().any(|line| {
        line.contains("submitted") && line.contains("vault sweep") && !line.contains("failed")
    })
}

fn mint_name_note_in_flight(log: &str, name: &str) -> bool {
    log.contains(name)
        && (log.contains("NameNote order in flight") || log.contains("registration in flight"))
}

fn mint_rejected_name_note(log: &str, name: &str) -> bool {
    log.contains(name)
        && (log.contains("NameNote submission rejected") || log.contains("registration rejected"))
}
