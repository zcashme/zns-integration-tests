//! Happy-path mint settlement: wait until mint submits alice's Name Note,
//! mine it, and check fields with `zns-verify`.

use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use crate::harness::Stack;
use crate::verify::{
    registration_txid, registry_commitment_keys, wait_for_verified_name_note, VerifiedNameNote,
};

/// Pay Treasury `ZNS:claim:forever:alice:<ua>`, wait for mint, verify on chain.
///
/// Checks: mint logs in-flight (not rejected / non-request / unauthorized);
/// `zns-verify` decrypt + `cmx`; fields alice/claim/user UA/`none`/0/local-test keys;
/// mint `txid=` matches the on-chain registration tx if present.
pub async fn claim_alice(stack: &mut Stack) -> Result<VerifiedNameNote> {
    let claim_txid = stack.user.pay_claim("alice").await?;
    eprintln!("claim payment txid {claim_txid}");
    // Confirm the payment, then leave the tip still so mint's
    // `exact_tip == cursor` gate can run intake (a racing tip skips it).
    stack.zebra.generate_blocks(1).await?;

    let deadline = Instant::now() + Duration::from_secs(600);
    let started = Instant::now();
    let mut poked = 0u32;
    loop {
        if !stack.mint.is_running() {
            bail!(
                "mint died while settling claim:\n{}",
                stack.mint.exit_detail()
            );
        }
        let log = stack.mint.log_text();
        if mint_rejected_name_note(&log, "alice") {
            bail!(
                "mint rejected alice registration:\n{}",
                stack.mint.exit_detail()
            );
        }
        if mint_name_note_submitted(&log, "alice") {
            eprintln!("mint settled alice");
            let expected_txid = registration_txid(&log, "alice");
            let scan_from = stack.zebra.tip_height().await?;
            let note =
                wait_for_verified_name_note(&stack.zebra, &mut stack.mint, "alice", scan_from)
                    .await?;
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
            assert_eq!(note.ua, stack.user.ua);
            assert_eq!(note.expires_at.as_deref(), Some("none"));
            assert_eq!(note.value, 0);
            let (g_d, pk_d) = registry_commitment_keys()?;
            assert_eq!(note.g_d, g_d);
            assert_eq!(note.pk_d, pk_d);
            return Ok(note);
        }
        if log.contains("non-request payment") && log.contains(&claim_txid) {
            bail!(
                "mint saw claim tx {claim_txid} as a non-request payment:\n{}",
                stack.mint.exit_detail()
            );
        }
        if log.contains("claim not authorized") && log.contains("alice") {
            bail!(
                "mint did not authorize alice:\n{}",
                stack.mint.exit_detail()
            );
        }
        if Instant::now() >= deadline {
            bail!(
                "mint did not register alice within 600s:\n{}",
                stack.mint.exit_detail()
            );
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
        if poked == 0 && started.elapsed() > Duration::from_secs(90) {
            stack.zebra.generate_blocks(1).await?;
            poked = 1;
        }
    }
}

fn mint_name_note_submitted(log: &str, name: &str) -> bool {
    log.contains(name)
        && (log.contains("NameNote order sent")
            || log.contains("NameNote order in flight")
            || log.contains("registration in flight"))
}

fn mint_rejected_name_note(log: &str, name: &str) -> bool {
    log.contains(name)
        && (log.contains("NameNote submission rejected") || log.contains("registration rejected"))
}
