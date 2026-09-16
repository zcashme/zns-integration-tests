//! Spawn `zns-mint` built with `--features regtest,fake-tee`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tempfile::TempDir;

use crate::binaries::{cargo_build_bin, mint_bin_override, sibling_dir};
use crate::child::ChildProcess;

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
        cargo_build_bin(
            &sibling_dir("zns-mint"),
            "zns-mint",
            &["--features", "regtest,fake-tee"],
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
            write_fake_capsule(dir.path())?;
        }
        if !dest.is_file() {
            bail!("mint working dir has no {}", dest.display());
        }

        let log = dir.path().join("mint.stderr");
        let file = std::fs::File::create(&log).context("create mint.stderr")?;
        let file2 = file.try_clone().context("clone mint.stderr")?;

        let child = Command::new(&bin)
            .current_dir(dir.path())
            .stdout(Stdio::from(file))
            .stderr(Stdio::from(file2))
            .spawn()
            .with_context(|| format!("spawn zns-mint ({})", bin.display()))?;

        let mint = Mint {
            child: ChildProcess::new("zns-mint", child, Some(log)),
            _dir: dir,
        };

        // Mint has no boot probe until after `Boot::start` (metrics bind on
        // :9464). Settle long enough to catch immediate panics.
        // TODO: mint stays up only after the 40-note ceremony is on chain.
        tokio::time::sleep(Duration::from_secs(2)).await;
        Ok(mint)
    }

    pub fn is_running(&mut self) -> bool {
        self.child.is_running()
    }

    pub fn exit_detail(&self) -> String {
        self.child.exit_detail()
    }
}

/// Seal an all-zero seed with mint's `write_fake_capsule` example into `cwd/keys/`.
fn write_fake_capsule(cwd: &Path) -> Result<()> {
    let mint_dir = sibling_dir("zns-mint");
    let manifest = mint_dir.join("Cargo.toml");
    let status = Command::new("cargo")
        .current_dir(cwd)
        .args([
            "run",
            "--manifest-path",
            manifest.to_str().expect("utf-8 path"),
            "--example",
            "write_fake_capsule",
            "--features",
            "fake-tee",
        ])
        .status()
        .context("spawn cargo run --example write_fake_capsule")?;
    if !status.success() {
        bail!(
            "cargo run --example write_fake_capsule failed ({status}); \
             zns-mint at {} must include that example and --features fake-tee",
            mint_dir.display()
        );
    }
    Ok(())
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
