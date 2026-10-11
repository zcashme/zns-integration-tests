//! Run the real `zns-keygen` binary against a regtest chain, end to end:
//! seal → attest (non-tee dev) → wait for funding → build anchor →
//! broadcast → confirmations → custody manifest.
//!
//! Requires:
//!   ZEBRAD_BIN      zebrad with regtest + `generate` RPC
//!   ZALLET_BIN      zallet (zebra backend)
//!   ZNS_KEYGEN_BIN  zns-keygen built with `--features regtest` (debug)

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::json;

use zns_integration_tests::{Zallet, Zebrad};

/// Coinbase maturity on this chain plus the NU6 family activation height.
const MATURE_BLOCKS: u32 = 104;

/// Skip outside CI when a required binary is missing; fail inside CI.
fn skip_or_bail(why: &str) -> Result<()> {
    if std::env::var_os("CI").is_some() {
        bail!("{why} required in CI");
    }
    eprintln!("skipping: {why}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn keygen_ceremony_runs_straight_through() -> Result<()> {
    use zns_integration_tests::{zallet_bin, zebrad_bin};

    if zebrad_bin().is_none() {
        return skip_or_bail("zebrad not found (set ZEBRAD_BIN)");
    }
    if zallet_bin().is_none() {
        return skip_or_bail("zallet not found (set ZALLET_BIN)");
    }
    let keygen_bin = match std::env::var_os("ZNS_KEYGEN_BIN") {
        Some(bin) => std::path::PathBuf::from(bin),
        None => {
            return skip_or_bail("set ZNS_KEYGEN_BIN to a zns-keygen built with --features regtest")
        }
    };
    if !keygen_bin.is_file() {
        bail!("ZNS_KEYGEN_BIN {} is not a file", keygen_bin.display());
    }

    // ── chain + wallet ────────────────────────────────────────────
    let mut zebra = Zebrad::start().await.context("start zebrad")?;
    let mut zallet = Zallet::init(&zebra).context("init zallet")?;
    zebra
        .restart_with_miner(&zallet.miner_address)
        .await
        .context("point miner at zallet")?;
    zallet.start_daemon().await.context("start zallet daemon")?;

    zebra.generate_blocks(MATURE_BLOCKS).await?;
    zallet
        .wait_until_sees_coinbase(200_000_000, Duration::from_secs(300))
        .await?;
    zallet.shield_coinbase().await?;
    zebra.generate_blocks(1).await?; // confirm the shield tx
    let tip = zebra.tip_height().await?;
    zallet
        .wait_until_synced(tip, Duration::from_secs(120))
        .await?;
    zallet
        .wait_until_shielded(150_000_000, Duration::from_secs(120))
        .await?;

    // ── the ceremony, by the real binary ──────────────────────────
    let dir = tempfile::tempdir().context("ceremony dir")?;
    let log_path = dir.path().join("keygen.log");
    let log = std::fs::File::create(&log_path).context("create keygen.log")?;
    let mut child = Command::new(&keygen_bin)
        .current_dir(dir.path())
        .stdout(Stdio::from(log.try_clone().context("clone log")?))
        .stderr(Stdio::from(log))
        .spawn()
        .context("spawn zns-keygen")?;

    // Wait for the funding address to appear.
    let taddr = wait_for_line(&log_path, "Treasury t-address: ", 60).await?;
    eprintln!("keygen treasury: {taddr}");

    // Fund it: 1.0 ZEC from the shielded account to the transparent address.
    let sent = zallet
        .send_from_account(
            "orchard",
            json!([{ "address": taddr, "amount": 1.0 }]),
            "AllowRevealedRecipients",
        )
        .await
        .context("fund treasury")?;
    eprintln!("funding txid: {sent}");

    // Confirm the funding, then STOP mining: regtest blocks are near-instant,
    // and racing the chain during the (slow) proof would blow the anchor's
    // 40-block expiry before it is broadcast. Real chains have 75s blocks.
    zebra.generate_blocks(1).await?;
    wait_for_line(&log_path, "anchor tx built", 1500).await?;

    // Mine the anchor in, then keep confirming until keygen exits.
    zebra.generate_blocks(1).await?;
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!(
                    "keygen failed: {status}\n---\n{}",
                    std::fs::read_to_string(&log_path)?
                );
            }
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            bail!(
                "keygen did not finish in 1500s\n---\n{}",
                std::fs::read_to_string(&log_path)?
            );
        }
        zebra.generate_blocks(1).await?;
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    eprintln!("full keygen log:\n{}", std::fs::read_to_string(&log_path)?);

    // ── assertions ────────────────────────────────────────────────
    let keys = dir.path().join("keys");
    let manifest_text =
        std::fs::read_to_string(keys.join("zns_custody_manifest.toml")).context("manifest")?;
    let manifest: serde_json::Value = toml_to_json(&manifest_text)?;
    assert_eq!(manifest["network"], "regtest");
    let txid = manifest["anchor_txid"]
        .as_str()
        .expect("anchor_txid")
        .to_string();
    let birthday = manifest["birthday"].as_u64().expect("birthday") as u32;
    let fingerprint = manifest["seed_fingerprint"]
        .as_str()
        .expect("fingerprint")
        .to_string();
    assert!(txid.len() == 64, "txid hex: {txid}");
    assert!(birthday > 0);

    assert!(keys.join("zns_seed.capsule").is_file(), "capsule written");
    let conf_text = std::fs::read_to_string(keys.join("zns_mint.conf")).context("mint conf")?;
    let conf = toml_to_json(&conf_text)?;
    assert_eq!(conf["network"], "regtest");
    assert_eq!(
        conf["expected_seed_fingerprint"].as_str().unwrap(),
        fingerprint,
        "conf pins the manifest fingerprint"
    );
    assert_eq!(conf["birthday"].as_u64().unwrap() as u32, birthday);

    // The anchor is really on chain at the claimed birthday.
    let tx = zebra
        .rpc("getrawtransaction", json!([txid, 1]))
        .await
        .with_context(|| format!("anchor {txid} not on chain"))?;
    let blockhash = tx["blockhash"].as_str().context("blockhash")?.to_string();
    let block = zebra.rpc("getblock", json!([blockhash, 1])).await?;
    let height = block["height"].as_u64().context("height")?;
    assert_eq!(
        height as u32, birthday,
        "birthday is the anchor's inclusion height"
    );

    Ok(())
}

async fn wait_for_line(path: &std::path::Path, prefix: &str, timeout_secs: u64) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Some(line) = text.lines().find(|l| l.contains(prefix)) {
                return Ok(line.split(prefix).nth(1).expect("split").trim().to_string());
            }
        }
        if Instant::now() > deadline {
            bail!("no '{prefix}' line within {timeout_secs}s");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn toml_to_json(text: &str) -> Result<serde_json::Value> {
    let value: toml::Value = toml::from_str(text)?;
    Ok(serde_json::to_value(value)?)
}
