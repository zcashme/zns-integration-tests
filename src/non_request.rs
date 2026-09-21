//! Unhappy Treasury memos: mint must not treat them as claims.
//!
//! These are intake-parser failures (`parse_request` → None), logged as
//! `non-request payment`. They do not produce a Name Note. They do not
//! check underpay, duplicate alice, or update/release.

use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use crate::claim::pay_treasury;
use crate::harness::Stack;

/// Three memos on the same chain after a successful alice claim.
///
/// - garbage: not a ZNS request at all
/// - missing term: `ZNS:claim:alice:<ua>` (term-first grammar)
/// - invalid name: `Alice` fails `Name::parse`
pub async fn pay_invalid_memos(stack: &mut Stack) -> Result<()> {
    let ua = stack.user.ua.clone();
    let cases = [
        ("garbage", "not-a-zns-request".to_string()),
        ("missing term", format!("ZNS:claim:alice:{ua}")),
        ("invalid name", format!("ZNS:claim:forever:Alice:{ua}")),
    ];
    for (label, memo) in cases {
        let txid = pay_treasury(&mut stack.zebra, &stack.user, memo.as_ref()).await?;
        eprintln!("unhappy {label} txid {txid}");
        stack.zebra.generate_blocks(1).await?;
        wait_non_request(stack, &txid, label).await?;
    }
    Ok(())
}

async fn wait_non_request(stack: &mut Stack, txid: &str, label: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(180);
    let started = Instant::now();
    let mut poked = 0u32;
    loop {
        if !stack.mint.is_running() {
            bail!("mint died during {label}:\n{}", stack.mint.exit_detail());
        }
        let log = stack.mint.log_text();
        if log.contains("non-request payment") && log.contains(txid) {
            eprintln!("mint treated {label} as non-request payment");
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "mint did not log non-request for {label} ({txid}) within 180s:\n{}",
                stack.mint.exit_detail()
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        if poked == 0 && started.elapsed() > Duration::from_secs(60) {
            stack.zebra.generate_blocks(1).await?;
            poked = 1;
        }
    }
}
