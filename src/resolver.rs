//! Spawn `zns-resolver` and wait for JSON-RPC `status`.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tempfile::TempDir;

use crate::binaries::{cargo_build_bin, resolver_bin_override, sibling_dir};
use crate::child::ChildProcess;
use crate::rpc::json_rpc;

/// Resolver hardcodes this listen address.
pub const RESOLVER_RPC_ADDR: &str = "127.0.0.1:8080";

pub struct Resolver {
    child: ChildProcess,
    _dir: TempDir,
}

impl Resolver {
    /// Testnet build: the compiled-in UFVK actually decodes, so the RPC
    /// server starts. Indexer will retry lightwalletd in the background.
    pub fn build() -> Result<PathBuf> {
        if let Some(bin) = resolver_bin_override() {
            return Ok(bin);
        }
        cargo_build_bin(
            &sibling_dir("zns-resolver"),
            "zns-resolver",
            &["--no-default-features", "--features", "testnet"],
        )
    }

    pub async fn start(bin: PathBuf) -> Result<Self> {
        let dir = tempfile::tempdir().context("create resolver dir")?;
        let log = dir.path().join("resolver.stderr");
        let file = std::fs::File::create(&log).context("create resolver.stderr")?;
        let file2 = file.try_clone().context("clone resolver.stderr")?;

        let child = Command::new(&bin)
            .current_dir(dir.path())
            .stdout(Stdio::from(file))
            .stderr(Stdio::from(file2))
            .spawn()
            .with_context(|| format!("spawn zns-resolver ({})", bin.display()))?;

        let mut resolver = Resolver {
            child: ChildProcess::new("zns-resolver", child, Some(log)),
            _dir: dir,
        };
        resolver.wait_until_rpc_up().await?;
        Ok(resolver)
    }

    pub async fn status(&self) -> Result<Value> {
        json_rpc(&format!("http://{RESOLVER_RPC_ADDR}/"), "status", json!([])).await
    }

    async fn wait_until_rpc_up(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut last_err = anyhow!("no status call completed");
        loop {
            if !self.child.is_running() {
                bail!(
                    "zns-resolver exited during startup: {}",
                    self.child.exit_detail()
                );
            }
            match self.status().await {
                Ok(_) => return Ok(()),
                Err(e) => last_err = anyhow!("{e}"),
            }
            if Instant::now() >= deadline {
                bail!(
                    "zns-resolver JSON-RPC did not answer status within 30s; last error: {last_err:#}\n{}",
                    self.child.exit_detail()
                );
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    pub fn is_running(&mut self) -> bool {
        self.child.is_running()
    }
}
