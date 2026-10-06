//! Spawn `zns-mint` built with `--features regtest`.

use std::path::{Path, PathBuf};
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

    pub async fn start(bin: PathBuf) -> Result<Self> {
        let dir = tempfile::tempdir().context("create mint dir")?;
        let keys = dir.path().join("keys");
        std::fs::create_dir(&keys).context("create mint keys/")?;
        let dest = keys.join("zns_seed.capsule");
        if let Some(capsule) = capsule_source() {
            std::fs::copy(&capsule, &dest)
                .with_context(|| format!("copy capsule from {}", capsule.display()))?;
        } else {
            write_dev_capsule(&dest)?;
        }
        if !dest.is_file() {
            bail!("mint working dir has no {}", dest.display());
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

/// Seal the all-zero test seed with the public test key.
fn write_dev_capsule(dest: &Path) -> Result<()> {
    let key = zns_canon::sealing::dev_sealing_key(zns_canon::capsule::CAPSULE_KEY_CONTEXT);
    let capsule = zns_canon::capsule::seal_seed(
        &key,
        &secrecy::Secret::new([0u8; zns_canon::capsule::SEED_LEN]),
        &mut rand::rngs::OsRng,
    )
    .context("seal dev capsule")?;
    let bytes = zns_canon::capsule::serialize_capsule(&capsule).context("serialize dev capsule")?;
    std::fs::write(dest, bytes).with_context(|| format!("write {}", dest.display()))
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
