//! Happy-path claim: user pays Treasury `ZNS:claim:alice:<ua>`, mint registers.

use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use zns_integration_tests::{
    fund_user, miner_address, pay_claim, publish, zebrad_bin, Mint, User, Zebrad, FIXTURE_HEIGHT,
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

    let sweep_deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if !mint.is_running() {
            bail!("mint died during vault sweep:\n{}", mint.log_text());
        }
        if mint.log_text().contains("Ironwood vault sweep") {
            break;
        }
        if Instant::now() >= sweep_deadline {
            bail!(
                "mint did not submit the ceremony vault sweep:\n{}",
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
        if log.contains("registration rejected") && log.contains("alice") {
            bail!("mint rejected alice registration:\n{log}");
        }
        if log.contains("registration in flight") && log.contains("alice") {
            eprintln!("mint settled alice");
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
