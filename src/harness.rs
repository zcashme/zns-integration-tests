//! Shared e2e bring-up: zebrad, dev ceremony, mint, funded user.

use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use crate::binaries::{zallet_bin, zebrad_bin};
use crate::ceremony::{miner_address, publish, FIXTURE_HEIGHT};
use crate::claim::User;
use crate::mint::Mint;
use crate::zebra::Zebrad;

pub struct Stack {
    pub zebra: Zebrad,
    pub mint: Mint,
    pub user: User,
}

impl Stack {
    /// Ceremony + mint live + `mature_user_coins` spendable user coinbases.
    ///
    /// `Ok(None)` when zebrad is missing locally (not in CI).
    pub async fn start(mature_user_coins: u32) -> Result<Option<Self>> {
        if zebrad_bin().is_none() {
            if std::env::var_os("CI").is_some() {
                bail!("zebrad required in CI — set ZEBRAD_BIN");
            }
            eprintln!("skipping: zebrad not found (set ZEBRAD_BIN or put zebrad on PATH)");
            return Ok(None);
        }
        if zallet_bin().is_none() {
            if std::env::var_os("CI").is_some() {
                bail!("zallet required in CI — set ZALLET_BIN");
            }
            eprintln!("skipping: zallet not found (set ZALLET_BIN or put zallet-zebra on PATH)");
            return Ok(None);
        }

        let mint_build = tokio::task::spawn_blocking(Mint::build);

        let miner = miner_address()?;
        let mut zebra = Zebrad::start_with_miner(&miner).await?;
        zebra.generate_blocks(FIXTURE_HEIGHT).await?;

        // Fund the user BEFORE the ceremony: the wallet bring-up restarts
        // zebrad, which drops zebra's volatile non-finalized blocks (~35
        // deep). Funding after publish would erase the freshly mined
        // ceremony block; funding first leaves the ceremony on finalized,
        // restart-safe history.
        let user = User::fund(&mut zebra, mature_user_coins).await?;
        eprintln!("user miner: {}", user.miner_address);
        eprintln!("user UA: {}", user.ua);
        publish(&mut zebra).await?;

        let mint_bin = mint_build.await.expect("mint build task")?;
        let mut mint = Mint::start(mint_bin).await?;
        mint.wait_until_live().await?;
        wait_for_first_run_loop(&mut zebra, &mut mint).await?;

        Ok(Some(Stack { zebra, mint, user }))
    }
}

/// Mine one block so the run loop can fire. Sweep is optional: claim uses
/// the ceremony Treasury notes either way.
async fn wait_for_first_run_loop(zebra: &mut Zebrad, mint: &mut Mint) -> Result<()> {
    zebra.generate_blocks(1).await?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if !mint.is_running() {
            bail!("mint died after becoming live:\n{}", mint.exit_detail());
        }
        let log = mint.log_text();
        if mint_submitted_vault_sweep(&log) {
            break;
        }
        if log.contains("vault sweep") && log.contains("failed") {
            eprintln!("mint vault sweep failed; continuing");
            break;
        }
        if log.contains("mint rules applied") {
            break;
        }
        if Instant::now() >= deadline {
            bail!(
                "mint did not apply rules after the post-live tip:\n{}",
                mint.exit_detail()
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    zebra.generate_blocks(1).await?;
    Ok(())
}

fn mint_submitted_vault_sweep(log: &str) -> bool {
    log.lines().any(|line| {
        line.contains("submitted") && line.contains("vault sweep") && !line.contains("failed")
    })
}
