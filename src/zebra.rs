//! Local `zebrad` in Regtest, pinned to the mint's NU schedule and ports.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::binaries::zebrad_bin;
use crate::child::ChildProcess;
use crate::rpc::json_rpc;

/// Matches `zns-mint` `MINT_BIRTHDAY` on `--features regtest` (first block after NU6.3).
pub const NU6_3_ACTIVATION_HEIGHT: u32 = 4;

/// Mint hardcodes these (mainnet/regtest): JSON-RPC 8232, indexer gRPC 8230.
pub const ZEBRA_JSON_RPC_PORT: u16 = 8232;
pub const ZEBRA_INDEXER_PORT: u16 = 8230;

const LOCKBOX_DISBURSEMENT_ADDR: &str = "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v";
const LOCKBOX_DISBURSEMENT_ZATS: u64 = 1;
const DEFAULT_MINER_ADDRESS: &str = "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v";

/// A running `zebrad` Regtest node.
pub struct Zebrad {
    child: ChildProcess,
    pub rpc_port: u16,
    pub indexer_port: u16,
    _dir: tempfile::TempDir,
}

impl Zebrad {
    pub async fn start() -> Result<Self> {
        let bin =
            zebrad_bin().context("zebrad not found — set ZEBRAD_BIN or put zebrad on PATH")?;
        Self::start_with_bin(&bin).await
    }

    pub async fn start_with_bin(bin: &Path) -> Result<Self> {
        let dir = tempfile::tempdir().context("create zebrad dir")?;
        let net_port = pick_port()?;
        let config_path = dir.path().join("zebrad.toml");
        let cache_dir = dir.path().join("state");
        std::fs::write(
            &config_path,
            zebrad_toml(
                net_port,
                ZEBRA_JSON_RPC_PORT,
                ZEBRA_INDEXER_PORT,
                DEFAULT_MINER_ADDRESS,
                &cache_dir.to_string_lossy(),
            ),
        )
        .context("write zebrad.toml")?;

        let log = dir.path().join("zebrad.stderr");
        let child = spawn_zebrad(bin, &config_path, &log)?;
        let mut zebrad = Zebrad {
            child: ChildProcess::new("zebrad", child, Some(log)),
            rpc_port: ZEBRA_JSON_RPC_PORT,
            indexer_port: ZEBRA_INDEXER_PORT,
            _dir: dir,
        };
        zebrad.wait_until_rpc_up().await?;
        Ok(zebrad)
    }

    fn rpc_url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.rpc_port)
    }

    async fn wait_until_rpc_up(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut last_err = anyhow!("no getblocktemplate attempt completed");
        loop {
            if !self.child.is_running() {
                bail!("zebrad exited during startup: {}", self.child.exit_detail());
            }
            match self.rpc("getblocktemplate", json!([])).await {
                Ok(_) => return Ok(()),
                Err(e) => last_err = anyhow!("{e}"),
            }
            if Instant::now() >= deadline {
                bail!("zebrad did not become mineable within 120s; last error: {last_err:#}");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    pub async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        json_rpc(&self.rpc_url(), method, params).await
    }

    pub async fn generate_blocks(&self, n: u32) -> Result<()> {
        let hashes = self.rpc("generate", json!([n])).await.context("generate")?;
        let mined = hashes.as_array().map(|a| a.len()).unwrap_or(0);
        if mined != n as usize {
            bail!("generate mined {mined} of {n} requested blocks: {hashes}");
        }
        Ok(())
    }
}

fn spawn_zebrad(bin: &Path, config_path: &Path, stderr_path: &Path) -> Result<Child> {
    let log = std::fs::File::create(stderr_path).context("create zebrad.stderr")?;
    let log2 = log.try_clone().context("clone zebrad.stderr")?;
    let mut cmd = Command::new(bin);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("ZEBRA_") {
            cmd.env_remove(key);
        }
    }
    cmd.args(["--config", config_path.to_str().unwrap(), "start"])
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2))
        .spawn()
        .with_context(|| format!("spawn zebrad ({})", bin.display()))
}

fn pick_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").context("bind ephemeral port")?;
    Ok(listener.local_addr()?.port())
}

/// Must stay aligned with `zns-mint/regtest-harness` and `boot.rs` `regtest_network()`.
pub(crate) fn zebrad_toml(
    net_port: u16,
    rpc_port: u16,
    indexer_port: u16,
    miner_address: &str,
    cache_dir: &str,
) -> String {
    let nu6_3 = NU6_3_ACTIVATION_HEIGHT;
    format!(
        r#"[network]
network = "Regtest"
listen_addr = "127.0.0.1:{net_port}"

[network.testnet_parameters]
disable_pow = true

[network.testnet_parameters.activation_heights]
NU5 = 1
NU6 = 1
"NU6.1" = {nu6_3}
"NU6.2" = {nu6_3}
"NU6.3" = {nu6_3}

[[network.testnet_parameters.funding_streams]]
[network.testnet_parameters.funding_streams.height_range]
start = 1
end = 1_000_000
[[network.testnet_parameters.funding_streams.recipients]]
receiver = "Deferred"
numerator = 12
addresses = []

[[network.testnet_parameters.lockbox_disbursements]]
address = "{LOCKBOX_DISBURSEMENT_ADDR}"
amount = {LOCKBOX_DISBURSEMENT_ZATS}

[mining]
miner_address = "{miner_address}"

[state]
ephemeral = false
cache_dir = "{cache_dir}"

[rpc]
listen_addr = "127.0.0.1:{rpc_port}"
indexer_listen_addr = "127.0.0.1:{indexer_port}"
enable_cookie_auth = false
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_pins_nu63_at_mint_birthday() {
        let toml = zebrad_toml(1, 8232, 8230, DEFAULT_MINER_ADDRESS, "/tmp/z");
        assert!(toml.contains("\"NU6.3\" = 4"), "{toml}");
        assert!(toml.contains("listen_addr = \"127.0.0.1:8232\""));
        assert!(toml.contains("indexer_listen_addr = \"127.0.0.1:8230\""));
    }
}
