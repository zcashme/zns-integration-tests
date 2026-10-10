//! Spawn `zns-mint` built with `--features regtest`.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tempfile::TempDir;

use crate::binaries::{cargo_build_bin_with, mint_bin_override, sibling_dir};
use crate::child::ChildProcess;

/// Halo2 in a debug mint binary is too slow for vault sweep. Regtest
/// selects non-tee, which release builds refuse, so raise opt-level on
/// the proving crates.
const MINT_DEV_OPT: &[&str] = &[
    "--config",
    "profile.dev.package.orchard.opt-level=3",
    "--config",
    "profile.dev.package.halo2_proofs.opt-level=3",
    "--config",
    "profile.dev.package.halo2_gadgets.opt-level=3",
];

pub struct Mint {
    child: ChildProcess,
    _dir: TempDir,
}

impl Mint {
    /// Build (unless `ZNS_MINT_BIN` is set) the regtest mint binary.
    pub fn build() -> Result<PathBuf> {
        if let Some(bin) = mint_bin_override() {
            return Ok(bin);
        }
        cargo_build_bin_with(
            &sibling_dir("zns-mint"),
            "zns-mint",
            &["--features", "regtest"],
            MINT_DEV_OPT,
        )
    }

    /// `birthday` is the ceremony tip; it lands in `zns_mint.conf`.
    pub async fn start(bin: PathBuf, birthday: u32) -> Result<Self> {
        let dir = tempfile::tempdir().context("create mint dir")?;
        let keys = dir.path().join("keys");
        // The dev keys/ contract, one writer.
        zns_canon::regtest::write_dev_keys(&keys, birthday).context("write dev keys/")?;
        let dest = keys.join("zns_seed.capsule");
        // An explicit capsule overrides the generated one; it must seal the dev seed.
        if let Some(capsule) = capsule_source() {
            let blob = zns_canon::capsule::read_capsule_file(&capsule)
                .with_context(|| format!("read capsule {}", capsule.display()))?;
            let sealed = zns_canon::capsule::parse_capsule(&blob)
                .with_context(|| format!("parse capsule {}", capsule.display()))?;
            if sealed.fingerprint != zns_canon::regtest::dev_seed_fingerprint().to_bytes() {
                bail!("override capsule does not seal the dev seed");
            }
            std::fs::copy(&capsule, &dest)
                .with_context(|| format!("copy capsule from {}", capsule.display()))?;
        }

        let log = dir.path().join("mint.stderr");
        let file = std::fs::File::create(&log).context("create mint.stderr")?;
        let file2 = file.try_clone().context("clone mint.stderr")?;

        let child = Command::new(&bin)
            .current_dir(dir.path())
            .env(
                "RUST_LOG",
                std::env::var("RUST_LOG").unwrap_or_else(|_| "zns_mint=debug".into()),
            )
            .stdout(Stdio::from(file))
            .stderr(Stdio::from(file2))
            .spawn()
            .with_context(|| format!("spawn zns-mint ({})", bin.display()))?;

        let mint = Mint {
            child: ChildProcess::new("zns-mint", child, Some(log)),
            _dir: dir,
        };

        // Mint has no boot probe until after `Boot::start` (metrics bind on
        // :9464). Give it time to sync ~104 blocks and hit genesis checks.
        tokio::time::sleep(Duration::from_secs(15)).await;
        Ok(mint)
    }

    pub fn is_running(&mut self) -> bool {
        self.child.is_running()
    }

    pub fn exit_detail(&self) -> String {
        self.child.exit_detail()
    }

    pub fn log_text(&self) -> String {
        self.child.log_text()
    }

    /// Boot finished and the run loop is waiting for tips (`live_from` is set).
    pub async fn wait_until_live(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let log = self.log_text();
            if log.contains("mint awaiting Zebra tips") {
                return Ok(());
            }
            if log.contains("FATAL") {
                bail!("mint fatal during boot:\n{}", self.exit_detail());
            }
            if !self.is_running() {
                bail!("mint exited before becoming live:\n{}", self.exit_detail());
            }
            if Instant::now() >= deadline {
                bail!(
                    "mint did not reach run loop within 300s:\n{}",
                    self.exit_detail()
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

fn capsule_source() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ZNS_SEED_CAPSULE") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    let sibling = sibling_dir("zns-mint").join("keys/zns_seed.capsule");
    sibling.is_file().then_some(sibling)
}
