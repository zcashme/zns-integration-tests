//! Independent Name Note scan: `zns-verify` trial-decrypt + `verify_name_note`
//! (not mint's decoder).

use std::io::Cursor;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use zcash_primitives::block::Block;

use crate::ceremony::{self, account_usk, DEV_SEED};
use crate::mint::Mint;
use crate::zebra::Zebrad;

/// Whitepaper §3.5 Registry `(g_d, pk_d)` for the all-zero seed (ZIP-32 is
/// network-independent, so FakeTee regtest matches the mainnet vector).
pub const VECTOR_G_D: &str = "de4338f2ab9fd8300a3a1c20dd690ce27026c6001c295d7c641a067ce809b11e";
pub const VECTOR_PK_D: &str = "6df609f5710f3b5deecd4ee4b8f0173b44af6cf8918ac00269526031ba628996";

/// A Name Note that decrypted under the Registry FVK and whose memo fields
/// reproduce the on-chain `cmx`.
pub struct VerifiedNameNote {
    pub height: u32,
    pub txid: String,
    pub name: String,
    pub action: String,
    pub ua: String,
    pub expires_at: Option<String>,
    pub value: u64,
    pub g_d: [u8; 32],
    pub pk_d: [u8; 32],
}

fn registry_fvk() -> Result<orchard::keys::FullViewingKey> {
    let usk = account_usk(&ceremony::regtest_network(), &DEV_SEED, 1)?;
    Ok(orchard::keys::FullViewingKey::from(usk.orchard()))
}

fn commitment_keys_for_fvk(fvk: &orchard::keys::FullViewingKey) -> ([u8; 32], [u8; 32]) {
    fvk.address_at(0u32, orchard::keys::Scope::External)
        .zns_commitment_keys()
}

/// ZIP-32 `g_d` / `pk_d` of the FakeTee Registry address (account 1, j=0).
pub fn registry_commitment_keys() -> Result<([u8; 32], [u8; 32])> {
    Ok(commitment_keys_for_fvk(&registry_fvk()?))
}

/// Mine until a Name Note for `name` is in a block and `zns-verify` accepts it.
pub async fn wait_for_verified_name_note(
    zebra: &Zebrad,
    mint: &mut Mint,
    name: &str,
    from_height: u32,
) -> Result<VerifiedNameNote> {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if !mint.is_running() {
            bail!(
                "mint died while waiting to mine the Name Note:\n{}",
                mint.log_text()
            );
        }
        zebra.generate_blocks(1).await?;
        if let Some(note) = find_verified_name_note(zebra, from_height, name).await? {
            return Ok(note);
        }
        if Instant::now() >= deadline {
            bail!("no verified {name} Name Note from height {from_height} within 180s");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn find_verified_name_note(
    zebra: &Zebrad,
    from_height: u32,
    name: &str,
) -> Result<Option<VerifiedNameNote>> {
    let tip = zebra.tip_height().await?;
    for height in from_height..=tip {
        for note in scan_block(zebra, height).await? {
            if note.name == name {
                return Ok(Some(note));
            }
        }
    }
    Ok(None)
}

async fn scan_block(zebra: &Zebrad, height: u32) -> Result<Vec<VerifiedNameNote>> {
    let network = ceremony::regtest_network();
    let hex = zebra
        .rpc("getblock", serde_json::json!([height.to_string(), 0]))
        .await
        .with_context(|| format!("getblock {height}"))?;
    let hex = hex
        .as_str()
        .ok_or_else(|| anyhow!("getblock {height} was not hex"))?;
    let bytes = hex::decode(hex).with_context(|| format!("decode block {height}"))?;
    let block = Block::read(Cursor::new(bytes), &network)
        .with_context(|| format!("parse block {height}"))?;

    let fvk = registry_fvk()?;
    let registry_addr = fvk.address_at(0u32, orchard::keys::Scope::External);

    let mut found = Vec::new();
    for tx in block.vtx() {
        let Some(bundle) = tx.ironwood_bundle() else {
            continue;
        };
        if bundle.bundle_version() != orchard::bundle::BundleVersion::ironwood_v3() {
            continue;
        }
        if !bundle.flags().outputs_enabled() {
            continue;
        }
        for action in bundle.actions() {
            let Some((note, recipient, memo_bytes, cmx)) =
                zns_verify::decrypt::try_decrypt_ironwood(action, &fvk)
            else {
                continue;
            };
            if recipient != registry_addr {
                continue;
            }
            let memo = zns_verify::Memo::from_array(*memo_bytes.as_array());
            let payload = match zns_verify::NameNote::parse(&memo) {
                Ok(n) => n,
                Err(_) => continue,
            };
            let (g_d, pk_d) = recipient.zns_commitment_keys();
            let value = note.value().inner();
            let rho = zns_verify::Rho::from_bytes(&action.rho().to_bytes())
                .ok_or_else(|| anyhow!("non-canonical rho at height {height}"))?;
            if !zns_verify::verify_name_note(&payload, g_d, pk_d, value, rho, cmx) {
                bail!(
                    "Registry-decryptable memo at height {height} tx {} failed zns-verify",
                    tx.txid()
                );
            }
            found.push(VerifiedNameNote {
                height,
                txid: tx.txid().to_string(),
                name: payload.name().as_str().to_string(),
                action: String::from_utf8_lossy(payload.action().as_bytes()).into_owned(),
                ua: payload.ua().as_str().to_string(),
                expires_at: payload.expires_at().map(|e| e.field_bytes().to_string()),
                value,
                g_d,
                pk_d,
            });
        }
    }
    Ok(found)
}

/// `txid=` on the mint line that reports the Name Note is in flight.
pub fn registration_txid(log: &str, name: &str) -> Option<String> {
    for line in log.lines() {
        let in_flight = line.contains("NameNote order in flight")
            || line.contains("registration in flight")
            || line.contains("NameNote order sent");
        if !(in_flight && line.contains(name)) {
            continue;
        }
        let rest = line.split("txid=").nth(1)?;
        let txid = rest
            .split(|c: char| c.is_whitespace() || c == ',')
            .next()?
            .trim_matches('"');
        if txid.len() == 64 && txid.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some(txid.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitepaper_vector_is_mainnet_all_zero_registry() {
        let usk = zcash_keys::keys::UnifiedSpendingKey::from_seed(
            &zcash_protocol::consensus::MAIN_NETWORK,
            &DEV_SEED,
            zip32::AccountId::try_from(1).expect("account 1"),
        )
        .expect("derive");
        let fvk = orchard::keys::FullViewingKey::from(usk.orchard());
        let (g_d, pk_d) = commitment_keys_for_fvk(&fvk);
        assert_eq!(hex::encode(g_d), VECTOR_G_D);
        assert_eq!(hex::encode(pk_d), VECTOR_PK_D);
    }

    #[test]
    fn fake_tee_registry_keys_are_stable() {
        let (g_d, pk_d) = registry_commitment_keys().expect("derive");
        // LocalNetwork coin_type is 1, not mainnet 133, so these are not the
        // whitepaper vector. Pin the FakeTee identity ceremony and mint share.
        assert_eq!(
            hex::encode(g_d),
            "ce684b50f15484a2a7f4a6625bffc14f7181940b378b467b0dcf583948386da4"
        );
        assert_eq!(
            hex::encode(pk_d),
            "4299a489d4bbe399956ed2c8dcf9c7f386634acd2d1b5027c325354a503f2a91"
        );
    }

    #[test]
    fn registration_txid_from_tracing_line() {
        let log = "2026-09-17T11:28:01Z  INFO zns_mint: NameNote order in flight name=alice action=claim txid=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert_eq!(
            registration_txid(log, "alice").as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn registration_txid_from_order_sent_line() {
        // zns-mint e8bbff7 reworded the order log; match both spellings.
        let log = "2026-09-21T17:19:58Z  INFO zns_mint: NameNote order sent — the wallet holds it until the chain answers txid=4a2bf4f67b027357f34600c0b108d4fa9b1177338c72ad2267d0c4a5c4af3666 name=alice action=\"claim\"";
        assert_eq!(
            registration_txid(log, "alice").as_deref(),
            Some("4a2bf4f67b027357f34600c0b108d4fa9b1177338c72ad2267d0c4a5c4af3666")
        );
    }
}
