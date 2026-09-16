use anyhow::{bail, Result};
use zns_integration_tests::{zebrad_bin, Stack};

#[tokio::test]
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
    assert!(
        info.get("blocks").and_then(|b| b.as_u64()).unwrap_or(0) >= 4,
        "zebra should be at NU6.3 (height 4+), got {info}"
    );

    let status = stack.resolver.status().await?;
    assert!(
        status.get("viewing_key").is_some(),
        "resolver status missing viewing_key: {status}"
    );

    assert!(stack.resolver.is_running(), "resolver process died");

    if !stack.mint.is_running() {
        // TODO: fail this test once the ceremony notes exist and mint stays up.
        eprintln!(
            "mint is not running (TODO: 40-note Registry ceremony): {}",
            stack.mint.exit_detail()
        );
    }

    Ok(())
}
