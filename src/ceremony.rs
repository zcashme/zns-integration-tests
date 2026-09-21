//! Dev-only Registry ceremony + Treasury funding for FakeTee mint boot.
//!
//! Same all-zero ZIP-32 seed as mint's `write_fake_capsule`. Not keygen:
//! no SNP, no loader, no production seed. Keep `ANCHOR_POOL_SIZE` and
//! `MIN_TREASURY_ZATS` aligned with `zns-mint`.

use anyhow::{anyhow, bail, Context, Result};
use orchard::builder::{Builder as OrchardBuilder, BundleType};
use orchard::bundle::BundleVersion;
use rand::rngs::OsRng;
use transparent::address::TransparentAddress;
use transparent::builder::{
    SpendInfo, TransparentBuilder, TransparentInputInfo, TransparentSigningSet,
};
use transparent::bundle::OutPoint;
use transparent::keys::{IncomingViewingKey, NonHardenedChildIndex};
use zcash_keys::encoding::encode_transparent_address_p;
use zcash_keys::keys::UnifiedSpendingKey;
use zcash_primitives::transaction::fees::transparent::InputSize;
use zcash_primitives::transaction::fees::zip317::FeeRule as Zip317;
use zcash_primitives::transaction::fees::FeeRule as _;
use zcash_primitives::transaction::Transaction;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_protocol::value::{ZatBalance, Zatoshis};
use zip32::AccountId;

use crate::tx::assemble_v6_transparent_ironwood;
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
    Ok(())
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

/// One entry of zebra's `getaddressutxos` response.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddressUtxo {
    txid: String,
    output_index: u32,
    satoshis: u64,
    script: String,
    height: u32,
}

pub(crate) async fn collect_mature_coinbase(
    zebra: &Zebrad,
    network: &LocalNetwork,
    miner: &TransparentAddress,
    tip: u32,
) -> Result<Vec<Coin>> {
    let address = encode_transparent_address_p(network, miner);
    let utxos: Vec<AddressUtxo> = serde_json::from_value(
        zebra
            .rpc(
                "getaddressutxos",
                serde_json::json!([{ "addresses": [address] }]),
            )
            .await
            .context("getaddressutxos")?,
    )
    .context("getaddressutxos response")?;
    let mut coins = Vec::new();
    for utxo in utxos {
        // zebra does not maturity-filter; coinbase needs COINBASE_MATURITY
        // confirmations to be spendable.
        if utxo.height + COINBASE_MATURITY > tip.saturating_add(1) {
            continue;
        }
        let mut txid = hex::decode(&utxo.txid).context("txid hex")?;
        txid.reverse(); // display order -> internal byte order
        let script = hex::decode(&utxo.script).context("script hex")?;
        coins.push(Coin {
            outpoint: OutPoint::new(
                txid.try_into().map_err(|_| anyhow!("txid length"))?,
                utxo.output_index,
            ),
            coin: transparent::bundle::TxOut::new(
                Zatoshis::from_u64(utxo.satoshis).context("satoshis range")?,
                transparent::address::Script(zcash_script::script::Code(script)),
            ),
        });
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

    // Transparent side: the miner's P2PKH coinbase input.
    let mut signing = TransparentSigningSet::new();
    let pubkey = signing.add_key(miner_sk);
    let mut transparent_builder = TransparentBuilder::empty();
    transparent_builder.add_input(TransparentInputInfo::from_parts(
        coin.outpoint,
        coin.coin,
        SpendInfo::P2pkh { pubkey },
    )?);
    let transparent = transparent_builder.build();

    // Ironwood side: 40 zero Registry anchors + Treasury funding + one zero
    // pad so the action count is even (42). UNPADDED, empty-tree anchor.
    let mut ironwood_builder = OrchardBuilder::new(
        BundleType::UNPADDED,
        BundleVersion::ironwood_v3(),
        BundleVersion::ironwood_v3().default_flags(),
        orchard::Anchor::empty_tree(),
    )?;
    let registry_fvk = orchard::keys::FullViewingKey::from(registry_usk.orchard());
    let registry_addr = registry_fvk.address_at(0u32, orchard::keys::Scope::External);
    let registry_ovk = registry_fvk.to_ovk(orchard::keys::Scope::External);
    for _ in 0..ANCHOR_POOL_SIZE {
        ironwood_builder.add_output(
            Some(registry_ovk.clone()),
            registry_addr,
            orchard::value::NoteValue::from_raw(0),
            [0; 512],
        )?;
    }
    let treasury_fvk = orchard::keys::FullViewingKey::from(treasury_usk.orchard());
    let treasury_addr = treasury_fvk.address_at(0u32, orchard::keys::Scope::External);
    let treasury_ovk = treasury_fvk.to_ovk(orchard::keys::Scope::External);
    ironwood_builder.add_output(
        Some(treasury_ovk.clone()),
        treasury_addr,
        orchard::value::NoteValue::from_raw(treasury_value.into_u64()),
        [0; 512],
    )?;
    ironwood_builder.add_output(
        Some(treasury_ovk),
        treasury_addr,
        orchard::value::NoteValue::from_raw(0),
        [0; 512],
    )?;
    let (ironwood, _) = ironwood_builder
        .build::<ZatBalance>(&mut OsRng)?
        .expect("ironwood bundle exists");

    assemble_v6_transparent_ironwood(network, target, transparent, ironwood, &signing)
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
