//! Dev-only Registry ceremony + Treasury funding for FakeTee mint boot.
//!
//! Same all-zero ZIP-32 seed as mint's `write_fake_capsule`. Not keygen:
//! no SNP, no loader, no production seed. Keep `ANCHOR_POOL_SIZE` and
//! `MIN_TREASURY_ZATS` aligned with `zns-mint`.

use std::io::Cursor;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use rand::rngs::OsRng;
use sapling::prover::{OutputProver, SpendProver};
use transparent::address::TransparentAddress;
use transparent::builder::TransparentSigningSet;
use transparent::bundle::OutPoint;
use transparent::keys::{IncomingViewingKey, NonHardenedChildIndex};
use zcash_keys::encoding::encode_transparent_address_p;
use zcash_keys::keys::UnifiedSpendingKey;
use zcash_primitives::block::Block;
use zcash_primitives::transaction::builder::{BuildConfig, Builder, BundlePadding};
use zcash_primitives::transaction::fees::transparent::InputSize;
use zcash_primitives::transaction::fees::zip317::{self, FeeRule as Zip317};
use zcash_primitives::transaction::fees::FeeRule as _;
use zcash_primitives::transaction::Transaction;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_protocol::memo::MemoBytes;
use zcash_protocol::value::Zatoshis;
use zip32::AccountId;

use crate::zebra::{Zebrad, NU6_3_ACTIVATION_HEIGHT};

/// Matches `zns-mint::mint::registry::ANCHOR_POOL_SIZE`.
pub const ANCHOR_POOL_SIZE: usize = 40;

/// Matches `zns-mint::mint::MIN_TREASURY_BALANCE` (0.002 ZEC).
pub const MIN_TREASURY_ZATS: u64 = 200_000;

/// Zebra `MIN_TRANSPARENT_COINBASE_MATURITY`.
pub const COINBASE_MATURITY: u32 = 100;

/// Blocks to mine so NU6.3 is active and at least one coinbase is mature.
pub const FIXTURE_HEIGHT: u32 = NU6_3_ACTIVATION_HEIGHT + COINBASE_MATURITY;

pub(crate) const DEV_SEED: [u8; 32] = [0u8; 32];

/// Same LocalNetwork as mint `boot.rs` `regtest_network()`.
pub fn regtest_network() -> LocalNetwork {
    let one = BlockHeight::from_u32(1);
    let four = BlockHeight::from_u32(4);
    LocalNetwork {
        overwinter: Some(one),
        sapling: Some(one),
        blossom: Some(one),
        heartwood: Some(one),
        canopy: Some(one),
        nu5: Some(one),
        nu6: Some(one),
        nu6_1: Some(four),
        nu6_2: Some(four),
        nu6_3: Some(four),
    }
}

/// Transparent P2PKH for the all-zero Treasury account (ZIP-32 account 0).
/// Point zebrad `miner_address` here so coinbase is spendable.
pub fn miner_address() -> Result<String> {
    let network = regtest_network();
    let (addr, _) = taddr_for_seed(&network, &DEV_SEED)?;
    Ok(encode_transparent_address_p(&network, &addr))
}

/// Mine-mature coinbase, then one Ironwood shielding tx: 40 zero-value
/// Registry outputs + a Treasury note covering the remainder after ZIP-317.
pub async fn publish(zebra: &mut Zebrad) -> Result<()> {
    let network = regtest_network();
    let (taddr, child) = taddr_for_seed(&network, &DEV_SEED)?;
    let treasury_usk = account_usk(&network, &DEV_SEED, 0)?;
    let registry_usk = account_usk(&network, &DEV_SEED, 1)?;

    let tip = zebra.tip_height().await?;
    if tip < FIXTURE_HEIGHT {
        bail!(
            "ceremony needs height {FIXTURE_HEIGHT}+ (NU6.3 + {COINBASE_MATURITY} maturity), tip={tip}"
        );
    }

    let coins = collect_mature_coinbase(zebra, &network, &taddr, tip).await?;
    if coins.is_empty() {
        bail!("no mature coinbase to {taddr:?}; miner_address must be the FakeTee treasury t-addr");
    }

    if try_cached_tx(zebra).await? {
        return Ok(());
    }

    let sk = treasury_usk
        .transparent()
        .derive_external_secret_key(child)
        .map_err(|e| anyhow!("derive miner secret key: {e}"))?;

    let target = BlockHeight::from_u32(tip + 1);
    eprintln!(
        "ceremony: proving {} Ironwood actions at target {target} (empty-tree bundle)",
        ANCHOR_POOL_SIZE + 2
    );
    let network_for_build = network;
    let tx = tokio::task::spawn_blocking(move || {
        build_ceremony_tx(
            &network_for_build,
            &treasury_usk,
            &registry_usk,
            sk,
            coins,
            target,
        )
    })
    .await
    .context("ceremony proving task")??;
    eprintln!("ceremony: broadcast {}", tx.txid());

    zebra.ensure_rpc().await.context("zebrad after proving")?;

    let mut raw = Vec::new();
    tx.write(&mut raw).context("serialize ceremony tx")?;
    let hex = hex::encode(&raw);
    zebra
        .rpc("sendrawtransaction", serde_json::json!([hex]))
        .await
        .context("sendrawtransaction ceremony")?;
    zebra.generate_blocks(1).await?;
    save_cached_tx(&hex);
    Ok(())
}

/// Broadcast the ceremony tx cached at `ZNS_CEREMONY_TX_CACHE`, if any.
///
/// The regtest chain up to `FIXTURE_HEIGHT` is reproducible (disable_pow,
/// coinbase paying the deterministic all-zero-seed Treasury t-addr, fixed
/// funding streams), so a ceremony tx signed on an earlier run spends the
/// same mature coinbase outpoint and stays valid on a fresh chain — its
/// proofs do not need to be reproduced. Broadcast-first with a proving
/// fallback: a missing, stale, or rejected cache only costs time, never
/// correctness.
async fn try_cached_tx(zebra: &mut Zebrad) -> Result<bool> {
    let Some(path) = std::env::var_os("ZNS_CEREMONY_TX_CACHE") else {
        return Ok(false);
    };
    let display = || Path::new(&path).display();
    let cached = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            eprintln!("ceremony: cannot read {}: {e}; re-proving", display());
            return Ok(false);
        }
    };
    let Some(hex) = cached.lines().find(|l| !l.trim().is_empty()) else {
        eprintln!("ceremony: cache file empty; re-proving");
        return Ok(false);
    };
    let hex = hex.trim();
    zebra.ensure_rpc().await?;
    match zebra
        .rpc("sendrawtransaction", serde_json::json!([hex]))
        .await
    {
        Ok(v) => {
            let txid = v.get("txid").and_then(|t| t.as_str()).unwrap_or("?");
            eprintln!("ceremony: broadcast cached {txid}");
            zebra.generate_blocks(1).await?;
            Ok(true)
        }
        Err(e) => {
            eprintln!("ceremony: cached tx rejected ({e}); re-proving");
            Ok(false)
        }
    }
}

fn save_cached_tx(hex: &str) {
    let Some(path) = std::env::var_os("ZNS_CEREMONY_TX_CACHE") else {
        return;
    };
    match std::fs::write(&path, hex) {
        Ok(()) => eprintln!("ceremony: saved tx to {}", Path::new(&path).display()),
        Err(e) => eprintln!(
            "ceremony: could not save tx to {}: {e}",
            Path::new(&path).display()
        ),
    }
}

pub(crate) fn account_usk(
    network: &LocalNetwork,
    seed: &[u8; 32],
    account: u32,
) -> Result<UnifiedSpendingKey> {
    UnifiedSpendingKey::from_seed(
        network,
        seed,
        AccountId::try_from(account).expect("account 0/1"),
    )
    .map_err(|e| anyhow!("ZIP-32 USK account {account}: {e}"))
}

pub(crate) fn taddr_for_seed(
    network: &LocalNetwork,
    seed: &[u8; 32],
) -> Result<(TransparentAddress, NonHardenedChildIndex)> {
    let usk = account_usk(network, seed, 0)?;
    Ok(usk
        .transparent()
        .to_account_pubkey()
        .derive_external_ivk()
        .map_err(|e| anyhow!("external IVK: {e}"))?
        .default_address())
}

/// Treasury orchard UA (all-zero seed, account 0, j=0 external) on regtest.
pub fn treasury_ua() -> Result<String> {
    orchard_ua(&regtest_network(), &DEV_SEED)
}

pub(crate) fn orchard_ua(network: &LocalNetwork, seed: &[u8; 32]) -> Result<String> {
    let usk = account_usk(network, seed, 0)?;
    let fvk = orchard::keys::FullViewingKey::from(usk.orchard());
    zcash_keys::address::UnifiedAddress::from_receivers(
        Some(fvk.address_at(0u32, orchard::keys::Scope::External)),
        None,
        None,
    )
    .ok_or_else(|| anyhow!("orchard-only UA"))
    .map(|ua| ua.encode(network))
}

pub(crate) struct Coin {
    pub outpoint: OutPoint,
    pub coin: transparent::bundle::TxOut,
}

pub(crate) async fn collect_mature_coinbase(
    zebra: &Zebrad,
    network: &LocalNetwork,
    miner: &TransparentAddress,
    tip: u32,
) -> Result<Vec<Coin>> {
    let next_height = tip.saturating_add(1);
    let mut coins = Vec::new();
    // Block::read cannot parse genesis.
    for height in 1..=tip {
        if height + COINBASE_MATURITY > next_height {
            continue;
        }
        let hex = zebra
            .rpc("getblock", serde_json::json!([height.to_string(), 0]))
            .await
            .with_context(|| format!("getblock {height}"))?;
        let hex = hex
            .as_str()
            .ok_or_else(|| anyhow!("getblock {height} was not hex"))?;
        let bytes = hex::decode(hex).with_context(|| format!("decode block {height}"))?;
        let block = Block::read(Cursor::new(bytes), network)
            .with_context(|| format!("parse block {height}"))?;
        for tx in block.vtx() {
            let Some(bundle) = tx.transparent_bundle() else {
                continue;
            };
            for (n, out) in bundle.vout.iter().enumerate() {
                if out.recipient_address().as_ref() == Some(miner) && out.value() > Zatoshis::ZERO {
                    coins.push(Coin {
                        outpoint: OutPoint::new(*tx.txid().as_ref(), n as u32),
                        coin: out.clone(),
                    });
                }
            }
        }
    }
    coins.sort_by_key(|c| std::cmp::Reverse(c.coin.value()));
    Ok(coins)
}

fn build_ceremony_tx(
    network: &LocalNetwork,
    treasury_usk: &UnifiedSpendingKey,
    registry_usk: &UnifiedSpendingKey,
    miner_sk: secp256k1::SecretKey,
    coins: Vec<Coin>,
    target: BlockHeight,
) -> Result<Transaction> {
    // One P2PKH coinbase is enough; keep the bundle small.
    let coin = coins
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no coinbase inputs"))?;
    let input_value = coin.coin.value();

    // 40 zero Registry + Treasury change + one dummy zero so Ironwood
    // action count is even (42).
    let ironwood_actions = ANCHOR_POOL_SIZE + 2;
    let fee = Zip317::standard()
        .fee_required(
            network,
            target,
            std::iter::once(InputSize::STANDARD_P2PKH),
            std::iter::empty::<usize>(),
            0,
            0,
            0,
            ironwood_actions,
        )
        .map_err(|e| anyhow!("ZIP-317 fee: {e}"))?;
    if input_value.into_u64() <= fee.into_u64() {
        bail!(
            "coinbase {} zats cannot cover fee {}",
            input_value.into_u64(),
            fee.into_u64()
        );
    }
    let treasury_value = Zatoshis::from_u64(input_value.into_u64() - fee.into_u64())
        .expect("input minus fee is in range");
    if treasury_value.into_u64() < MIN_TREASURY_ZATS {
        bail!(
            "Treasury would be {} zats; mint needs {MIN_TREASURY_ZATS}",
            treasury_value.into_u64()
        );
    }

    let mut signing = TransparentSigningSet::new();
    let pubkey = signing.add_key(miner_sk);

    let registry_fvk = orchard::keys::FullViewingKey::from(registry_usk.orchard());
    let treasury_fvk = orchard::keys::FullViewingKey::from(treasury_usk.orchard());

    let mut builder = Builder::new(
        *network,
        target,
        BuildConfig::Standard {
            sapling_anchor: None,
            orchard_anchor: None,
            ironwood_anchor: Some(orchard::Anchor::empty_tree()),
            orchard_padding: BundlePadding::UNPADDED,
            ironwood_padding: BundlePadding::UNPADDED,
        },
    );
    builder
        .add_transparent_p2pkh_input(pubkey, coin.outpoint, coin.coin)
        .map_err(|e| anyhow!("transparent input: {e}"))?;

    let registry_addr = registry_fvk.address_at(0u32, orchard::keys::Scope::External);
    let registry_ovk = Some(registry_fvk.to_ovk(orchard::keys::Scope::External));
    for _ in 0..ANCHOR_POOL_SIZE {
        builder
            .add_ironwood_output::<zip317::FeeError>(
                registry_ovk.clone(),
                registry_addr,
                Zatoshis::ZERO,
                MemoBytes::empty(),
            )
            .map_err(|e| anyhow!("registry anchor output: {e}"))?;
    }

    let treasury_addr = treasury_fvk.address_at(0u32, orchard::keys::Scope::External);
    let treasury_ovk = Some(treasury_fvk.to_ovk(orchard::keys::Scope::External));
    builder
        .add_ironwood_output::<zip317::FeeError>(
            treasury_ovk.clone(),
            treasury_addr,
            treasury_value,
            MemoBytes::empty(),
        )
        .map_err(|e| anyhow!("treasury output: {e}"))?;
    builder
        .add_ironwood_output::<zip317::FeeError>(
            treasury_ovk,
            treasury_addr,
            Zatoshis::ZERO,
            MemoBytes::empty(),
        )
        .map_err(|e| anyhow!("ironwood padding output: {e}"))?;

    let built = builder
        .build(
            &signing,
            &[],
            &[],
            OsRng,
            &NoSapling,
            &NoSapling,
            &Zip317::standard(),
        )
        .map_err(|e| anyhow!("prove/sign ceremony tx: {e}"))?;
    Ok(built.transaction().clone())
}

/// Sapling proving keys are unused: this tx is transparent + Ironwood only.
pub(crate) struct NoSapling;

impl SpendProver for NoSapling {
    type Proof = ();

    fn prepare_circuit(
        _proof_generation_key: sapling::ProofGenerationKey,
        _diversifier: sapling::Diversifier,
        _rseed: sapling::Rseed,
        _value: sapling::value::NoteValue,
        _alpha: jubjub::Fr,
        _rcv: sapling::value::ValueCommitTrapdoor,
        _anchor: bls12_381::Scalar,
        _merkle_path: sapling::MerklePath,
    ) -> Option<sapling::circuit::Spend> {
        unreachable!("ceremony tx has no Sapling spends")
    }

    fn create_proof<R: rand::RngCore>(&self, _circuit: sapling::circuit::Spend, _rng: &mut R) {}

    fn encode_proof(_proof: Self::Proof) -> sapling::bundle::GrothProofBytes {
        unreachable!("ceremony tx has no Sapling spends")
    }
}

impl OutputProver for NoSapling {
    type Proof = ();

    fn prepare_circuit(
        _esk: &sapling::keys::EphemeralSecretKey,
        _payment_address: sapling::PaymentAddress,
        _rcm: jubjub::Fr,
        _value: sapling::value::NoteValue,
        _rcv: sapling::value::ValueCommitTrapdoor,
    ) -> sapling::circuit::Output {
        unreachable!("ceremony tx has no Sapling outputs")
    }

    fn create_proof<R: rand::RngCore>(&self, _circuit: sapling::circuit::Output, _rng: &mut R) {}

    fn encode_proof(_proof: Self::Proof) -> sapling::bundle::GrothProofBytes {
        unreachable!("ceremony tx has no Sapling outputs")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn miner_address_is_regtest_p2pkh() {
        let addr = miner_address().expect("derive");
        assert!(
            addr.starts_with("tm") || addr.starts_with("t1") || addr.starts_with("t"),
            "unexpected miner address {addr}"
        );
        assert_eq!(addr, miner_address().unwrap());
    }

    #[test]
    fn fixture_height_covers_maturity_after_nu63() {
        assert_eq!(FIXTURE_HEIGHT, 104);
    }
}
