//! Reorgs against a live mint: one stack, then each queue the run loop rewinds.
//!
//! Zebra shortens the best chain with `invalidateblock`. The mint's run loop
//! walks to the common ancestor and then:
//!
//! - Registry, Name Notes, requests, and refunds drop what was above that
//!   height. A seen Name Note whose payment survived is authorized again
//!   and resubmitted.
//! - The OTP queue is replaced with an empty one, even when its payment is
//!   still canonical. The spent request is not re-decided, so the old code
//!   does nothing and a new payment is what starts a new challenge.
//!
//! `zcash` and `zebra` both have to be protected for the whole run. The
//! mint refunds a claim of a name that is not. `zcash` is the name the
//! mint e2e already claims.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use hmac::Mac;
use serde_json::json;

use crate::harness::Stack;
use crate::verify::{wait_for_verified_name_note, VerifiedNameNote};

/// The name a reorg is not allowed to drop.
const KEEP: &str = "zcash";
/// The name a later reorg drops. Must be protected, same as [`KEEP`].
const DROP: &str = "zebra";
/// Unprotected claim with no access code: admitted, then refunded.
const REFUND_NAME: &str = "reorgpay";

/// Above the withheld refund fee (10 marginal actions). Not a claim price.
const REFUND_ZEC: f64 = 0.1;

const SCAN_TIMEOUT: Duration = Duration::from_secs(120);
const DECIDE_TIMEOUT: Duration = Duration::from_secs(180);
const SETTLE_TIMEOUT: Duration = Duration::from_secs(600);

/// Smoke, refund, Name Note, a second name, then OTP, on one chain.
pub async fn run(stack: &mut Stack) -> Result<()> {
    smoke(stack).await.context("smoke")?;
    refunds(stack).await.context("refund")?;
    let (keep_pay, keep_note) = claim_protected(stack, KEEP).await.context("claim keep")?;
    let keep_note = reorg_confirmation(stack, &keep_pay, keep_note)
        .await
        .context("confirmation reorg")?;
    let (drop_pay, drop_note) = claim_protected(stack, DROP).await.context("claim drop")?;
    orphan_payment(stack, DROP, &drop_pay, &drop_note, &keep_note)
        .await
        .context("payment reorg")?;
    otp_wipe(stack, KEEP).await.context("otp")?;
    Ok(())
}

/// Invalidate an empty tip and mine a replacement. No queue is holding work.
async fn smoke(stack: &mut Stack) -> Result<()> {
    let tip = stack.zebra.tip_height().await?;
    wait_scanned(stack, tip, 0).await?;
    let old_hash = stack.zebra.block_hash(tip).await?;
    let mark = stack.mint.log_text().len();
    stack.zebra.invalidate_above(tip - 1).await?;
    let rewound = wait_rewind(stack, mark).await?;
    if rewound != tip - 1 {
        bail!("smoke rewound to {rewound}, wanted {}", tip - 1);
    }
    stack.zebra.generate_blocks(1).await?;
    if stack.zebra.tip_height().await? != tip {
        bail!("smoke did not restore height {tip}");
    }
    let new_hash = stack.zebra.block_hash(tip).await?;
    if new_hash == old_hash {
        bail!("replacement block at {tip} reused {old_hash}");
    }
    wait_scanned(stack, tip, mark).await?;
    eprintln!(
        "smoke: rewound to {} and replaced {old_hash} with {new_hash}",
        tip - 1
    );
    Ok(())
}

/// A refund admitted for the tip payment is dropped with that block.
/// The same raw transaction, mined on the new chain, is refunded once.
async fn refunds(stack: &mut Stack) -> Result<()> {
    let memo = format!("ZNS:claim:forever:{REFUND_NAME}:{}", stack.user.ua);
    let txid = stack.user.pay_treasury_amount(&memo, REFUND_ZEC).await?;
    eprintln!("refund payment {txid}");
    stack.zebra.generate_blocks(1).await?;
    let height = wait_tx_height(stack, &txid).await?;
    wait_for(stack, DECIDE_TIMEOUT, "refund admit", false, |log| {
        log.contains("payment refund") && log.contains(&txid)
    })
    .await?;
    if refund_built(&stack.mint.log_text(), &txid) {
        bail!("refund for {txid} was built while its block was still the tip");
    }

    // Saved while the payment is still in a block. Rebroadcasting it spends
    // the same note; a second wallet send would have only the orphaned change.
    let raw = stack.zebra.raw_transaction(&txid).await?;
    let mark = stack.mint.log_text().len();
    let ancestor = height - 1;
    stack.zebra.invalidate_above(ancestor).await?;
    wait_rewind(stack, mark).await?;
    wait_scanned(stack, ancestor, mark).await?;
    if refund_built(tail(&stack.mint.log_text(), mark), &txid) {
        bail!("orphaned payment {txid} was refunded");
    }
    if stack.zebra.find_tx_height(&txid, height).await?.is_some() {
        bail!("orphaned payment {txid} is still on the best chain");
    }

    stack.zebra.submit_raw(&raw).await?;
    stack.zebra.generate_blocks(1).await?;
    let replayed = wait_tx_height(stack, &txid).await?;
    if replayed <= ancestor {
        bail!("rebroadcast {txid} mined at {replayed}, not above {ancestor}");
    }
    eprintln!("refund rebroadcast {txid} at {replayed}");
    wait_for(stack, DECIDE_TIMEOUT, "refund replay admit", false, |log| {
        tail(log, mark).contains("payment refund") && tail(log, mark).contains(&txid)
    })
    .await?;
    // Built on the pass after the payment's block, then left in the mempool
    // for the block after that.
    stack.zebra.generate_blocks(1).await?;
    wait_for(stack, DECIDE_TIMEOUT, "refund replay built", false, |log| {
        !refund_txids(tail(log, mark), &txid).is_empty()
    })
    .await?;
    let built = refund_txids(tail(&stack.mint.log_text(), mark), &txid);
    if built.len() != 1 {
        bail!(
            "refund for {txid} was built {} times: {built:?}",
            built.len()
        );
    }
    let refund_txid = built[0].clone();
    stack.zebra.generate_blocks(1).await?;
    let refund_height = wait_tx_height(stack, &refund_txid).await?;
    let tip = stack.zebra.tip_height().await?;
    wait_scanned(stack, tip, mark).await?;
    stack.zebra.generate_blocks(1).await?;
    let tip = stack.zebra.tip_height().await?;
    wait_scanned(stack, tip, mark).await?;
    let built_again = refund_txids(tail(&stack.mint.log_text(), mark), &txid);
    if built_again != vec![refund_txid.clone()] {
        bail!("refund for {txid} was built again: {built_again:?}");
    }
    if stack
        .zebra
        .find_tx_height(&refund_txid, refund_height)
        .await?
        != Some(refund_height)
    {
        bail!("refund {refund_txid} left height {refund_height}");
    }
    stack
        .user
        .zallet
        .wait_until_synced(tip, SCAN_TIMEOUT)
        .await?;
    eprintln!("refund: rebroadcast {txid} at {replayed} refunded once as {refund_txid}");
    Ok(())
}

/// Claim `name` with the dev access code and wait until the Name Note is verified.
async fn claim_protected(stack: &mut Stack, name: &str) -> Result<(String, VerifiedNameNote)> {
    let memo = format!(
        "ZNS:claim:{}:forever:{name}:{}",
        access_code(name),
        stack.user.ua
    );
    let mark = stack.mint.log_text().len();
    let scan_from = stack.zebra.tip_height().await?;
    let pay = stack.user.pay_treasury(&memo).await?;
    eprintln!("claim {name} payment {pay}");
    stack.zebra.generate_blocks(1).await?;
    let note = wait_name_note(stack, mark, scan_from, name, &pay).await?;
    eprintln!("claim {name} note {} at {}", note.txid, note.height);
    Ok((pay, note))
}

/// Orphan the Name Note's block and leave the payment. The mint resubmits.
async fn reorg_confirmation(
    stack: &mut Stack,
    payment: &str,
    note: VerifiedNameNote,
) -> Result<VerifiedNameNote> {
    let tip = stack.zebra.tip_height().await?;
    wait_scanned(stack, tip, 0).await?;
    let payment_height = stack
        .zebra
        .find_tx_height(payment, 1)
        .await?
        .context("claim payment is not on the best chain")?;
    if payment_height >= note.height {
        bail!(
            "payment height {payment_height} is not below the Name Note at {}",
            note.height
        );
    }
    let mark = stack.mint.log_text().len();
    stack.zebra.invalidate_above(note.height - 1).await?;
    wait_rewind(stack, mark).await?;
    if stack.zebra.find_tx_height(payment, payment_height).await? != Some(payment_height) {
        bail!("confirmation reorg dropped the payment {payment}");
    }
    if stack
        .zebra
        .find_tx_height(&note.txid, note.height)
        .await?
        .is_some()
    {
        bail!("orphaned Name Note {} is still canonical", note.txid);
    }
    // The rewind pass proves the resubmit on its own. Mining here would let
    // the still-valid original transaction confirm before that pass logs it.
    wait_for(stack, SETTLE_TIMEOUT, "name note resubmit", false, |log| {
        submitted_txid(tail(log, mark), note.name.as_str(), "claim").is_some()
    })
    .await?;
    let resubmitted = submitted_txid(
        tail(&stack.mint.log_text(), mark),
        note.name.as_str(),
        "claim",
    )
    .context("resubmit line lost")?;
    wait_state(stack, &note.name, Some(1), DECIDE_TIMEOUT).await?;
    let ancestor = note.height - 1;
    let fresh =
        wait_for_verified_name_note(&stack.zebra, &mut stack.mint, &note.name, ancestor + 1)
            .await?;
    if fresh.height <= ancestor {
        bail!(
            "{}'s note reappeared at {}, not above {ancestor}",
            note.name,
            fresh.height
        );
    }
    if fresh.txid != resubmitted && fresh.txid != note.txid {
        bail!(
            "on-chain note {} is neither the resubmit {resubmitted} nor the original {}",
            fresh.txid,
            note.txid
        );
    }
    eprintln!(
        "confirmation reorg: {} resubmitted as {resubmitted}, confirmed {} at {}",
        note.txid, fresh.txid, fresh.height
    );
    Ok(fresh)
}

/// Drop `name`'s payment and its Name Note. `keep` stays, and `name` can be claimed again.
async fn orphan_payment(
    stack: &mut Stack,
    name: &str,
    payment: &str,
    note: &VerifiedNameNote,
    keep: &VerifiedNameNote,
) -> Result<()> {
    let tip = stack.zebra.tip_height().await?;
    wait_scanned(stack, tip, 0).await?;
    let payment_height = stack
        .zebra
        .find_tx_height(payment, keep.height)
        .await?
        .context("drop payment is not on the best chain")?;
    if payment_height <= keep.height {
        bail!(
            "drop payment at {payment_height} is not above {}'s note at {}",
            keep.name,
            keep.height
        );
    }
    let mark = stack.mint.log_text().len();
    stack.zebra.invalidate_above(payment_height - 1).await?;
    wait_rewind(stack, mark).await?;
    if stack.zebra.find_tx_height(&keep.txid, keep.height).await? != Some(keep.height) {
        bail!("payment reorg dropped {}'s note {}", keep.name, keep.txid);
    }
    if stack
        .zebra
        .find_tx_height(&note.txid, payment_height)
        .await?
        .is_some()
    {
        bail!("orphaned {name} note {} is still canonical", note.txid);
    }
    wait_state(stack, name, None, DECIDE_TIMEOUT).await?;
    let (_pay, reclaimed) = claim_protected(stack, name).await?;
    if reclaimed.txid == note.txid {
        bail!("reclaimed {name} reused the orphaned note {}", note.txid);
    }
    eprintln!(
        "payment reorg: {name} at {} dropped; {} stayed; reclaimed at {}",
        note.height, keep.name, reclaimed.height
    );
    Ok(())
}

/// An empty-block reorg above a relayed update wipes the challenge. The old
/// code is refunded. A second update payment is challenged again.
async fn otp_wipe(stack: &mut Stack, name: &str) -> Result<()> {
    let request = format!("ZNS:update:none:{name}:{}", stack.user.ua);
    let mark = stack.mint.log_text().len();
    let pay = stack.user.pay_treasury(&request).await?;
    eprintln!("update payment {pay}");
    stack.zebra.generate_blocks(1).await?;
    let challenge = wait_challenge(stack, mark, name, &pay).await?;
    eprintln!("challenge {challenge}");
    wait_state(stack, name, Some(2), DECIDE_TIMEOUT).await?;
    let code = read_otp(stack, &challenge).await?;

    let stable = stack.zebra.tip_height().await?;
    wait_scanned(stack, stable, 0).await?;
    stack.zebra.generate_blocks(1).await?;
    let empty = stack.zebra.tip_height().await?;
    wait_scanned(stack, empty, 0).await?;
    let pay_height = stack
        .zebra
        .find_tx_height(&pay, 1)
        .await?
        .context("update payment left the chain")?;
    if pay_height >= empty {
        bail!("update payment at {pay_height} is not under the empty block {empty}");
    }

    let rewind_mark = stack.mint.log_text().len();
    stack.zebra.invalidate_above(empty - 1).await?;
    wait_rewind(stack, rewind_mark).await?;
    if stack.zebra.find_tx_height(&pay, pay_height).await? != Some(pay_height) {
        bail!("otp reorg dropped the update payment {pay}");
    }
    wait_state(stack, name, None, DECIDE_TIMEOUT).await?;

    let echo = format!("ZNS:otp:{code}:{name}:update:{}", stack.user.ua);
    let echo_mark = stack.mint.log_text().len();
    let echo_txid = stack.user.pay_treasury_amount(&echo, REFUND_ZEC).await?;
    eprintln!("stale otp echo {echo_txid}");
    stack.zebra.generate_blocks(1).await?;
    let echo_height = wait_tx_height(stack, &echo_txid).await?;
    wait_for(stack, DECIDE_TIMEOUT, "stale otp refund", false, |log| {
        tail(log, echo_mark).contains("payment refund") && tail(log, echo_mark).contains(&echo_txid)
    })
    .await?;
    // `scanned to tip` is logged before a Name Note is submitted. The next
    // block is scanned only after that submission attempt has finished.
    wait_scanned(stack, echo_height, echo_mark).await?;
    stack.zebra.generate_blocks(1).await?;
    let after = stack.zebra.tip_height().await?;
    wait_scanned_within(stack, after, echo_mark, SETTLE_TIMEOUT).await?;
    if submitted_txid(tail(&stack.mint.log_text(), echo_mark), name, "update").is_some() {
        bail!("stale OTP authorized an update of {name}");
    }

    let fresh_mark = stack.mint.log_text().len();
    let fresh_pay = stack.user.pay_treasury(&request).await?;
    eprintln!("fresh update payment {fresh_pay}");
    stack.zebra.generate_blocks(1).await?;
    let fresh = wait_challenge(stack, fresh_mark, name, &fresh_pay).await?;
    if fresh == challenge {
        bail!("fresh update reused wiped challenge {challenge}");
    }
    wait_state(stack, name, Some(2), DECIDE_TIMEOUT).await?;
    eprintln!("otp: wiped {challenge}; stale code refunded; fresh challenge {fresh}");
    Ok(())
}

fn access_code(name: &str) -> String {
    let key = zns_canon::access_code::purpose_key(&zns_canon::regtest::DEV_SEED);
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&*key)
        .expect("HMAC-SHA256 accepts any key length");
    mac.update(name.as_bytes());
    let digest = mac.finalize().into_bytes();
    let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % 1_000_000;
    format!("{n:06}")
}

async fn wait_name_note(
    stack: &mut Stack,
    mark: usize,
    scan_from: u32,
    name: &str,
    pay: &str,
) -> Result<VerifiedNameNote> {
    wait_for(stack, SETTLE_TIMEOUT, "name note submit", true, |log| {
        let tail = tail(log, mark);
        if tail.contains("payment refund") && tail.contains(pay) {
            return true;
        }
        if tail.contains("non-request payment") && tail.contains(pay) {
            return true;
        }
        submitted_txid(tail, name, "claim").is_some()
    })
    .await?;
    let log = stack.mint.log_text();
    let tail = tail(&log, mark);
    if tail.contains("payment refund") && tail.contains(pay) {
        bail!(
            "{name} was refunded; it has to be a protected pre-sale name and the dev access code \
             has to match:\n{}",
            stack.mint.exit_detail()
        );
    }
    if tail.contains("non-request payment") && tail.contains(pay) {
        bail!(
            "mint saw the {name} claim as a non-request:\n{}",
            stack.mint.exit_detail()
        );
    }
    let expected = submitted_txid(tail, name, "claim").context("submit line lost")?;
    let note = wait_for_verified_name_note(&stack.zebra, &mut stack.mint, name, scan_from).await?;
    if note.txid != expected {
        bail!("on-chain Name Note {} != mint txid {expected}", note.txid);
    }
    Ok(note)
}

async fn wait_challenge(stack: &mut Stack, mark: usize, name: &str, pay: &str) -> Result<String> {
    wait_for(stack, SETTLE_TIMEOUT, "controller challenge", true, |log| {
        let tail = tail(log, mark);
        challenge_txid(tail, name).is_some()
            || (tail.contains("non-request payment") && tail.contains(pay))
    })
    .await?;
    let log = stack.mint.log_text();
    let tail = tail(&log, mark);
    if tail.contains("non-request payment") && tail.contains(pay) {
        bail!(
            "mint saw the {name} update as a non-request:\n{}",
            stack.mint.exit_detail()
        );
    }
    challenge_txid(tail, name).context("challenge line lost")
}

async fn read_otp(stack: &mut Stack, challenge_txid: &str) -> Result<String> {
    let ua = stack.user.ua.clone();
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    let mut from = stack.zebra.tip_height().await?;
    let confirmed = loop {
        ensure_running(stack)?;
        stack.zebra.generate_blocks(1).await?;
        if let Some(height) = stack.zebra.find_tx_height(challenge_txid, from).await? {
            break height;
        }
        if Instant::now() >= deadline {
            bail!("challenge {challenge_txid} was not mined within {SETTLE_TIMEOUT:?}");
        }
        from = stack.zebra.tip_height().await?.saturating_add(1);
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    // A challenge mined just after broadcast can miss the wallet's mempool
    // stream. The scan queues the block; the memo arrives on a later tip.
    let mut synced_to = confirmed;
    let view = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        stack.user.zallet.wait_until_synced(synced_to, left).await?;
        match stack
            .user
            .zallet
            .call("z_viewtransaction", json!([challenge_txid]))
            .await
        {
            Ok(view) => break view,
            Err(error) if Instant::now() < deadline => {
                eprintln!("challenge {challenge_txid} not viewable yet ({error}); mining");
                stack.zebra.generate_blocks(1).await?;
                synced_to += 1;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(error) => return Err(error).context("z_viewtransaction challenge"),
        }
    };
    otp_from_view(&view, &ua)
        .with_context(|| format!("no OTP memo for {ua} in challenge {challenge_txid}"))
}

fn otp_from_view(view: &serde_json::Value, ua: &str) -> Option<String> {
    let outputs = view.get("outputs")?.as_array()?;
    for output in outputs {
        if output.get("address").and_then(|a| a.as_str()) != Some(ua) {
            continue;
        }
        let text = output
            .get("memoStr")
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .or_else(|| {
                output
                    .get("memo")
                    .and_then(|m| m.as_str())
                    .and_then(|hex_memo| hex::decode(hex_memo).ok())
                    .and_then(|bytes| String::from_utf8(bytes).ok())
            })?;
        let parts: Vec<&str> = text.split(':').collect();
        if parts.len() == 6
            && parts[0] == "ZNS"
            && parts[1] == "otp"
            && parts[2].len() == 6
            && parts[2].bytes().all(|b| b.is_ascii_digit())
        {
            return Some(parts[2].to_string());
        }
    }
    None
}

async fn wait_tx_height(stack: &mut Stack, txid: &str) -> Result<u32> {
    let from = stack.zebra.tip_height().await?.saturating_sub(5);
    stack
        .zebra
        .find_tx_height(txid, from)
        .await?
        .with_context(|| format!("{txid} was not mined"))
}

async fn wait_rewind(stack: &mut Stack, mark: usize) -> Result<u32> {
    wait_for(stack, SCAN_TIMEOUT, "rewind", false, |log| {
        rewind_height(tail(log, mark)).is_some()
    })
    .await?;
    rewind_height(tail(&stack.mint.log_text(), mark)).context("rewind line lost")
}

/// Wait until `scanned to tip` reaches `height` in the log after `since`.
/// A replacement block reuses the old height, so post-rewind waits pass
/// the mark taken before `invalidate_above`.
async fn wait_scanned(stack: &mut Stack, height: u32, since: usize) -> Result<()> {
    wait_scanned_within(stack, height, since, SCAN_TIMEOUT).await
}

/// [`wait_scanned`] with an explicit timeout. The stale-OTP check waits
/// through a Name Note proof that would run if the code still authorized.
async fn wait_scanned_within(
    stack: &mut Stack,
    height: u32,
    since: usize,
    timeout: Duration,
) -> Result<()> {
    wait_for(stack, timeout, "scan", false, |log| {
        last_height(tail(log, since), "scanned to tip").is_some_and(|scanned| scanned >= height)
    })
    .await
}

async fn wait_state(
    stack: &mut Stack,
    name: &str,
    want: Option<u8>,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        ensure_running(stack)?;
        let got = stack.mint.request_state(name).await?;
        if got == want {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "{name} request state is {got:?}, wanted {want:?}:\n{}",
                stack.mint.exit_detail()
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn wait_for(
    stack: &mut Stack,
    timeout: Duration,
    what: &str,
    poke: bool,
    ready: impl Fn(&str) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let started = Instant::now();
    let mut poked = false;
    loop {
        ensure_running(stack)?;
        let log = stack.mint.log_text();
        if log_fatal(&log) {
            bail!("mint fatal during {what}:\n{}", stack.mint.exit_detail());
        }
        if ready(&log) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for {what}:\n{}",
                stack.mint.exit_detail()
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        if poke && !poked && started.elapsed() > Duration::from_secs(90) {
            stack.zebra.generate_blocks(1).await?;
            poked = true;
        }
    }
}

fn ensure_running(stack: &mut Stack) -> Result<()> {
    if stack.mint.is_running() {
        Ok(())
    } else {
        bail!("mint exited:\n{}", stack.mint.exit_detail())
    }
}

fn refund_built(log: &str, payment: &str) -> bool {
    !refund_txids(log, payment).is_empty()
}

/// Refund transaction ids logged for `payment`, one per `refund built` line.
fn refund_txids(log: &str, payment: &str) -> Vec<String> {
    log.lines()
        .filter_map(|line| {
            let line = strip_ansi(line);
            (line.contains("refund built") && line.contains(payment))
                .then(|| field(&line, "txid"))
                .flatten()
        })
        .collect()
}

fn submitted_txid(log: &str, name: &str, action: &str) -> Option<String> {
    let mut found = None;
    for line in log.lines() {
        let line = strip_ansi(line);
        if line.contains("NameNote order sent") && line.contains(name) && has_action(&line, action)
        {
            found = field(&line, "txid");
        }
    }
    found
}

fn challenge_txid(log: &str, name: &str) -> Option<String> {
    let mut found = None;
    for line in log.lines() {
        let line = strip_ansi(line);
        if line.contains("controller challenged")
            && line.contains(name)
            && has_action(&line, "update")
        {
            found = field(&line, "txid");
        }
    }
    found
}

fn rewind_height(log: &str) -> Option<u32> {
    log.lines().rev().find_map(|line| {
        let line = strip_ansi(line);
        line.contains("mint rewound to canonical ancestor")
            .then(|| field_u32(&line, "height"))
            .flatten()
    })
}

fn last_height(log: &str, marker: &str) -> Option<u32> {
    log.lines().rev().find_map(|line| {
        let line = strip_ansi(line);
        line.contains(marker)
            .then(|| field_u32(&line, "height"))
            .flatten()
    })
}

fn log_fatal(log: &str) -> bool {
    log.lines().any(|line| strip_ansi(line).contains("FATAL"))
}

fn has_action(line: &str, action: &str) -> bool {
    line.contains(&format!("action={action}")) || line.contains(&format!("action=\"{action}\""))
}

fn field(line: &str, key: &str) -> Option<String> {
    let rest = line.split(&format!("{key}=")).nth(1)?;
    let value = rest
        .split(|c: char| c.is_whitespace() || c == ',')
        .next()?
        .trim_matches('"');
    (value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())).then(|| value.to_string())
}

fn field_u32(line: &str, key: &str) -> Option<u32> {
    let rest = line.split(&format!("{key}=")).nth(1)?;
    let value = rest
        .split(|c: char| c.is_whitespace() || c == ',')
        .next()?
        .trim_matches('"');
    value.parse().ok()
}

fn tail(log: &str, mark: usize) -> &str {
    log.get(mark..).unwrap_or("")
}

fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c2 in chars.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_access_code_is_six_digits_and_name_scoped() {
        let zcash = access_code("zcash");
        assert_eq!(zcash.len(), 6);
        assert!(zcash.bytes().all(|b| b.is_ascii_digit()));
        assert_eq!(zcash, access_code("zcash"));
        assert_ne!(zcash, access_code("zebra"));
    }

    #[test]
    fn parsers_read_the_run_loop_lines() {
        let log = "\
INFO mint rewound to canonical ancestor height=214 hash=abcd\n\
INFO scanned to tip height=220\n\
INFO controller challenged txid=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb name=zcash action=\"update\"\n\
INFO NameNote order sent txid=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa name=zcash action=\"claim\"\n\
INFO refund built txid=cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc payment=dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd\n";
        assert_eq!(rewind_height(log), Some(214));
        assert_eq!(last_height(log, "scanned to tip"), Some(220));
        let stale = "\
INFO scanned to tip height=220\n\
INFO mint rewound to canonical ancestor height=219\n";
        let mark = stale.find("mint rewound").expect("rewind line");
        assert_eq!(last_height(tail(stale, mark), "scanned to tip"), None);
        assert_eq!(
            challenge_txid(log, "zcash").as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
        assert_eq!(
            submitted_txid(log, "zcash", "claim").as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert!(submitted_txid(log, "zcash", "update").is_none());
        let payment = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
        let refund = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
        assert_eq!(refund_txids(log, payment), vec![refund.to_string()]);
        let twice = format!("{log}{log}");
        assert_eq!(refund_txids(&twice, payment).len(), 2);
    }
}
