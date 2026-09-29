//! A real `zallet` wallet as the claim user: regtest provisioning, daemon,
//! and JSON-RPC.
//!
//! The user is a genuine wallet — its own mnemonic, its own scan — instead
//! of in-process signing from a harness-held seed. Provisioning uses only
//! supported non-interactive paths (`generate-encryption-identity`,
//! `init-wallet-encryption`, `generate-mnemonic`, the regtest account
//! helper); the backend binary shares zallet's full CLI surface, so the
//! harness points `ZALLET_BIN` at `zallet-zebra` directly.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tempfile::TempDir;

use crate::binaries::zallet_bin;
use crate::child::ChildProcess;
use crate::zebra::{pick_port, Zebrad};

const RPC_UP_TIMEOUT: Duration = Duration::from_secs(120);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(600);

/// Must match `[[rpc.auth]]` in [`zallet_toml`].
const RPC_USER: &str = "user";
const RPC_PASS: &str = "pass";

pub struct Zallet {
    child: Option<ChildProcess>,
    bin: PathBuf,
    datadir: TempDir,
    log_path: PathBuf,
    rpc_port: u16,
    /// Transparent address zebrad mines to so this wallet holds coinbase.
    pub miner_address: String,
}

impl Zallet {
    /// Provision a fresh regtest wallet: encryption identity, mnemonic, and
    /// account 0 with its miner address. The daemon is not running yet —
    /// zallet's regtest helper requires a fresh single-seed wallet, so this
    /// must run before any other wallet use. Follow with
    /// [`Zallet::start_daemon`].
    pub fn init(zebra: &Zebrad) -> Result<Self> {
        let bin = zallet_bin()
            .context("zallet not found — set ZALLET_BIN or put zallet-zebra on PATH")?;
        let datadir = tempfile::tempdir().context("create zallet datadir")?;
        let rpc_port = pick_port()?;
        let log_path = datadir.path().join("zallet.log");
        std::fs::write(
            datadir.path().join("zallet.toml"),
            zallet_toml(
                zebra.rpc_port,
                zebra.indexer_port,
                &zebra.state_dir(),
                rpc_port,
            ),
        )
        .context("write zallet.toml")?;

        run(
            &bin,
            datadir.path(),
            &log_path,
            &["generate-encryption-identity"],
        )?;
        run(&bin, datadir.path(), &log_path, &["init-wallet-encryption"])?;
        run(&bin, datadir.path(), &log_path, &["generate-mnemonic"])?;
        let out = run_with_stdout(
            &bin,
            datadir.path(),
            &log_path,
            &["regtest", "generate-account-and-miner-address"],
        )?;
        let miner_address = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if miner_address.is_empty() {
            bail!("zallet regtest helper printed no miner address");
        }

        Ok(Self {
            child: None,
            bin,
            datadir,
            log_path,
            rpc_port,
            miner_address,
        })
    }

    /// Spawn the daemon and wait for its JSON-RPC. Zebrad must be running.
    pub async fn start_daemon(&mut self) -> Result<()> {
        let datadir = self
            .datadir
            .path()
            .to_str()
            .ok_or_else(|| anyhow!("zallet datadir path is not utf-8"))?;
        let log = std::fs::File::create(&self.log_path).context("create zallet.log")?;
        let log2 = log.try_clone().context("clone zallet.log")?;
        let child = Command::new(&self.bin)
            .args(["--datadir", datadir])
            .arg("start")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log2))
            .spawn()
            .with_context(|| format!("spawn zallet ({})", self.bin.display()))?;
        self.child = Some(ChildProcess::new(
            "zallet",
            child,
            Some(self.log_path.clone()),
        ));
        self.wait_until_rpc_up().await
    }

    fn rpc_url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.rpc_port)
    }

    fn running(&mut self) -> bool {
        self.child.as_mut().is_some_and(|c| c.is_running())
    }

    async fn wait_until_rpc_up(&mut self) -> Result<()> {
        let deadline = Instant::now() + RPC_UP_TIMEOUT;
        let mut last_err = anyhow!("no getwalletstatus call completed");
        loop {
            if !self.running() {
                bail!("zallet exited during startup: {}", self.exit_detail());
            }
            match self.call("getwalletstatus", json!([])).await {
                Ok(_) => return Ok(()),
                Err(e) => last_err = e,
            }
            if Instant::now() >= deadline {
                bail!("zallet RPC did not come up within {RPC_UP_TIMEOUT:?}; last error: {last_err:#}\n{}", self.exit_detail());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Issue a JSON-RPC call, returning the `result` on success.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({ "jsonrpc": "1.0", "id": "harness", "method": method, "params": params });
        let resp = reqwest::Client::new()
            .post(self.rpc_url())
            .basic_auth(RPC_USER, Some(RPC_PASS))
            .json(&body)
            .send()
            .await
            .context("zallet rpc request")?;
        let envelope: Value = resp.json().await.context("decode zallet rpc response")?;
        if let Some(err) = envelope.get("error").filter(|e| !e.is_null()) {
            bail!("zallet rpc error from {method}: {err}");
        }
        Ok(envelope.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Poll until the wallet has scanned to `target` and is unlocked.
    ///
    /// `getwalletstatus.locked` is true while the sync engine has not fully
    /// synced; spend and balance RPCs refuse to operate in that state.
    pub async fn wait_until_synced(&self, target: u32, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(status) = self.call("getwalletstatus", json!([])).await {
                let height = status
                    .pointer("/wallet_tip/height")
                    .and_then(|h| h.as_u64())
                    .unwrap_or(0);
                let locked = status
                    .get("locked")
                    .and_then(|l| l.as_bool())
                    .unwrap_or(true);
                if height >= u64::from(target) && !locked {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                bail!("zallet did not sync to height {target} within {timeout:?}");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Account 0's Orchard-only UA — the claim payout address.
    pub async fn orchard_ua(&self) -> Result<String> {
        let ua = self
            .call("z_getaddressforaccount", json!([0, ["orchard"]]))
            .await?;
        ua.get("address")
            .and_then(|a| a.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("z_getaddressforaccount returned no address: {ua}"))
    }

    /// The UUID `z_sendfromaccount` keys accounts by.
    pub async fn account_uuid(&self) -> Result<String> {
        let accounts = self.call("z_listaccounts", json!([])).await?;
        accounts
            .get(0)
            .and_then(|a| a.get("account_uuid"))
            .and_then(|u| u.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("z_listaccounts returned no account uuid: {accounts}"))
    }

    /// Shield all mature coinbase into account 0's Orchard pool.
    pub async fn shield_coinbase(&self) -> Result<()> {
        let ua = self.orchard_ua().await?;
        let op = self
            .call("z_shieldcoinbase", json!([self.miner_address, ua]))
            .await?;
        let opid = extract_opid(&op)
            .ok_or_else(|| anyhow!("z_shieldcoinbase returned no operation id: {op}"))?
            .to_string();
        self.wait_for_operation(&opid).await?;
        Ok(())
    }

    /// Spend from account 0 through `fund_source` (e.g. `"orchard"`),
    /// waiting for the operation and returning the broadcast txid.
    pub async fn send_from_account(
        &self,
        fund_source: &str,
        recipients: Value,
        privacy_policy: &str,
    ) -> Result<String> {
        let uuid = self.account_uuid().await?;
        let op = self
            .call(
                "z_sendfromaccount",
                json!([uuid, fund_source, recipients, 1, privacy_policy]),
            )
            .await?;
        // This zallet sends synchronously: built, proved, signed, and
        // broadcast within the call (the in-call PCZT pipeline), answering
        // with a SendResult. A bare-string body would be an async operation
        // id — poll it if one shows up.
        if let Some(txid) = op.get("txid").and_then(|t| t.as_str()) {
            if op.get("broadcast").and_then(|b| b.as_bool()) != Some(true) {
                bail!("zallet recorded but did not broadcast the send: {op}");
            }
            return Ok(txid.to_string());
        }
        let opid = extract_opid(&op)
            .ok_or_else(|| anyhow!("z_sendfromaccount returned no operation id: {op}"))?
            .to_string();
        let entry = self.wait_for_operation(&opid).await?;
        entry
            .pointer("/result/txid")
            .and_then(|t| t.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("operation result carries no txid: {entry}"))
    }

    async fn wait_for_operation(&self, opid: &str) -> Result<Value> {
        let deadline = Instant::now() + OPERATION_TIMEOUT;
        loop {
            let status = self
                .call("z_getoperationstatus", json!([[opid]]))
                .await?
                .as_array()
                .cloned()
                .unwrap_or_default();
            if let Some(entry) = status.into_iter().next() {
                match entry.get("status").and_then(|s| s.as_str()) {
                    Some("success") => return Ok(entry),
                    Some("failed") => bail!("zallet operation {opid} failed: {entry}"),
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                bail!("zallet operation {opid} did not finish within {OPERATION_TIMEOUT:?}");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    fn exit_detail(&self) -> String {
        match &self.child {
            Some(child) => child.exit_detail(),
            None => "zallet: not started".to_string(),
        }
    }
}

impl Drop for Zallet {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            child.kill_and_reap();
        }
    }
}

/// Operation ids come back either as a bare JSON string (`z_sendfromaccount`)
/// or as an object carrying `opid` plus shielding stats (`z_shieldcoinbase`).
fn extract_opid(op: &Value) -> Option<&str> {
    op.as_str()
        .or_else(|| op.get("opid").and_then(|v| v.as_str()))
}

fn run(bin: &Path, datadir: &Path, log_path: &Path, args: &[&str]) -> Result<()> {
    run_with_stdout(bin, datadir, log_path, args).map(|_| ())
}

fn run_with_stdout(
    bin: &Path,
    datadir: &Path,
    log_path: &Path,
    args: &[&str],
) -> Result<std::process::Output> {
    let datadir = datadir
        .to_str()
        .ok_or_else(|| anyhow!("zallet datadir path is not utf-8"))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .context("open zallet cli log")?;
    let log2 = log.try_clone().context("clone zallet cli log")?;
    Command::new(bin)
        .args(["--datadir", datadir])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log2))
        .output()
        .with_context(|| format!("spawn zallet {}", args.join(" ")))
        .and_then(|out| {
            if out.status.success() {
                return Ok(out);
            }
            let mut msg = format!(
                "zallet {} failed ({}); log {}",
                args.join(" "),
                out.status,
                log_path.display()
            );
            if let Ok(log) = std::fs::read_to_string(log_path) {
                let tail: Vec<&str> = log.lines().rev().take(20).collect();
                msg.push_str(&format!(
                    "\n{}",
                    tail.into_iter().rev().collect::<Vec<_>>().join("\n")
                ));
            }
            Err(anyhow::Error::msg(msg))
        })
}

/// Must stay aligned with the mint's regtest NU schedule: pre-NU6 at 1,
/// the NU6.1/6.2/6.3 family at 4, and zebrad's fixed 8232/8230 ports.
fn zallet_toml(
    zebra_rpc_port: u16,
    zebra_indexer_port: u16,
    zebra_state_dir: &Path,
    zallet_rpc_port: u16,
) -> String {
    format!(
        r#"backend = "zebra"

[builder]
[builder.limits]

[consensus]
network = "regtest"
regtest_nuparams = [
    "c2d6d0b4:1",
    "c8e71055:1",
    "4dec4df0:4",
    "5437f330:4",
    "37a5165b:4",
]

[database]

[external]

[features]
as_of_version = "0.1.0-beta.1"

[features.deprecated]

[features.experimental]

[indexer]
validator_address = "127.0.0.1:{zebra_rpc_port}"

[indexer.read_state_service]
grpc_address = "127.0.0.1:{zebra_indexer_port}"
zebra_state_path = "{}"

[keystore]

[note_management]

[rpc]
bind = ["127.0.0.1:{zallet_rpc_port}"]

[[rpc.auth]]
user = "{RPC_USER}"
password = "{RPC_PASS}"
"#,
        zebra_state_dir.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_pins_nu_family_at_the_mint_schedule() {
        let toml = zallet_toml(8232, 8230, Path::new("/tmp/z"), 8234);
        assert!(toml.contains("backend = \"zebra\""), "{toml}");
        for nuparam in [
            "c2d6d0b4:1",
            "c8e71055:1",
            "4dec4df0:4",
            "5437f330:4",
            "37a5165b:4",
        ] {
            assert!(toml.contains(nuparam), "missing {nuparam} in {toml}");
        }
        assert!(toml.contains("validator_address = \"127.0.0.1:8232\""));
        assert!(toml.contains("grpc_address = \"127.0.0.1:8230\""));
        assert!(toml.contains("zebra_state_path = \"/tmp/z\""));
        assert!(toml.contains("bind = [\"127.0.0.1:8234\"]"));
    }
}
