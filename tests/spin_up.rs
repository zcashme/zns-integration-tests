use anyhow::{bail, Result};
use zns_integration_tests::{zebrad_bin, Stack};

#[tokio::test(flavor = "multi_thread")]
async fn spin_up_zebra_mint_resolver() -> Result<()> {
    if zebrad_bin().is_none() {
        if std::env::var_os("CI").is_some() {
            bail!("zebrad required in CI — set ZEBRAD_BIN");
        }
        eprintln!("skipping: zebrad not found (set ZEBRAD_BIN or put zebrad on PATH)");
        return Ok(());
    }

    let mut stack = Stack::start().await?;

    let info = stack
        .zebra
        .rpc("getblockchaininfo", serde_json::json!([]))
        .await?;
    let height = info.get("blocks").and_then(|b| b.as_u64()).unwrap_or(0);
    assert!(
        height >= 105,
        "zebra should be past ceremony confirm (height 105+), got {info}"
    );

    let status = stack.resolver.status().await?;
    assert!(
        status.get("viewing_key").is_some(),
        "resolver status missing viewing_key: {status}"
    );

    assert!(stack.resolver.is_running(), "resolver process died");

    let log = stack.mint.log_text();
    if log.contains("anchor lineage pool expected") || log.contains("Treasury balance") {
        bail!(
            "ceremony fixture failed; mint still missing anchors or treasury:\n{}",
            stack.mint.exit_detail()
        );
    }

    if !stack.mint.is_running() {
        if std::env::var_os("CI").is_some() {
            bail!(
                "mint exited in CI (ceremony, params, and price should succeed):\n{}",
                stack.mint.exit_detail()
            );
        }
        eprintln!(
            "mint is not running (need sapling params in ~/.zcash-params): {}",
            stack.mint.exit_detail()
        );
    }

    Ok(())
}
