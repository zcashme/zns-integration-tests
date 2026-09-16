//! Locate sibling checkouts and on-disk binaries.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Directory of this crate (the integration-tests repo root).
pub fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Sibling checkout next to this repo, or `$ZNS_MINT_DIR` / `$ZNS_RESOLVER_DIR`.
pub fn sibling_dir(name: &str) -> PathBuf {
    let env_key = match name {
        "zns-mint" => "ZNS_MINT_DIR",
        "zns-resolver" => "ZNS_RESOLVER_DIR",
        other => panic!("unknown sibling repo {other}"),
    };
    if let Ok(p) = std::env::var(env_key) {
        return PathBuf::from(p);
    }
    crate_dir().join("..").join(name)
}

/// `$<env_var>` if it points at a file, else `name` on `$PATH`.
pub fn resolve_bin(env_var: &str, name: &str) -> Option<PathBuf> {
    if let Ok(p) = std::env::var(env_var) {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    which(name)
}

pub fn zebrad_bin() -> Option<PathBuf> {
    resolve_bin("ZEBRAD_BIN", "zebrad")
}

pub fn mint_bin_override() -> Option<PathBuf> {
    resolve_bin("ZNS_MINT_BIN", "zns-mint")
}

pub fn resolver_bin_override() -> Option<PathBuf> {
    resolve_bin("ZNS_RESOLVER_BIN", "zns-resolver")
}

fn which(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|dir| {
        let candidate = dir.join(name);
        candidate.is_file().then_some(candidate)
    })
}

/// `cargo build --bin <bin>` in `dir`, returning `target/debug/<bin>`.
pub fn cargo_build_bin(dir: &Path, bin: &str, extra_args: &[&str]) -> Result<PathBuf> {
    if !dir.is_dir() {
        bail!(
            "{} is not a directory — clone the sibling repo or set the matching ZNS_*_DIR",
            dir.display()
        );
    }
    let mut cmd = Command::new("cargo");
    cmd.current_dir(dir)
        .arg("build")
        .arg("--bin")
        .arg(bin)
        .args(extra_args);
    let status = cmd
        .status()
        .with_context(|| format!("spawn cargo build for {bin} in {}", dir.display()))?;
    if !status.success() {
        bail!(
            "cargo build --bin {bin} failed in {} ({status})",
            dir.display()
        );
    }
    let path = dir.join("target/debug").join(bin);
    if !path.is_file() {
        bail!("cargo build succeeded but {} is missing", path.display());
    }
    Ok(path)
}
