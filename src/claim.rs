//! Happy-path user claim: pay the Treasury an Ironwood note with a ZNS memo.
//!
//! Distinct from the FakeTee mint seed (`[0; 32]`). The user is a second
//! ZIP-32 wallet: coinbase to their t-addr, then one outputs-only Ironwood
//! bundle to the Treasury UA.

use anyhow::{anyhow, bail, Context, Result};
use orchard::builder::{Builder as OrchardBuilder, BundleType};
use orchard::bundle::BundleVersion;
use rand::rngs::OsRng;
use transparent::builder::{
    SpendInfo, TransparentBuilder, TransparentInputInfo, TransparentSigningSet,
};
use zcash_keys::encoding::encode_transparent_address_p;
use zcash_keys::keys::UnifiedSpendingKey;
use zcash_primitives::transaction::fees::transparent::InputSize;
use zcash_primitives::transaction::fees::zip317::FeeRule as Zip317;
use zcash_primitives::transaction::fees::FeeRule as _;
use zcash_primitives::transaction::Transaction;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::memo::{Memo, MemoBytes};
use zcash_protocol::value::{ZatBalance, Zatoshis};

use crate::ceremony::{
    self, account_usk, collect_mature_coinbase, orchard_ua, taddr_for_seed, COINBASE_MATURITY,
    DEV_SEED,
};
use crate::tx::assemble_v6_transparent_ironwood;
use crate::zebra::Zebrad;

/// Not the mint's all-zero seed.
pub(crate) const USER_SEED: [u8; 32] = [1u8; 32];

/// A user identity for claim (and later update) tests.
pub struct User {
    pub miner_address: String,
    pub ua: String,
}

impl User {
    pub fn new() -> Result<Self> {
        let network = ceremony::regtest_network();
        let (taddr, _) = taddr_for_seed(&network, &USER_SEED)?;
        Ok(Self {
            miner_address: encode_transparent_address_p(&network, &taddr),
            ua: orchard_ua(&network, &USER_SEED)?,
        })
    }
}

/// Restart zebra paying this user, mine until `mature` coinbases are spendable.
pub async fn fund_user_coinbases(zebra: &mut Zebrad, user: &User, mature: u32) -> Result<()> {
    if mature == 0 {
        bail!("fund_user_coinbases requires at least one mature coinbase");
    }
    zebra.restart_with_miner(&user.miner_address).await?;
    zebra.generate_blocks(COINBASE_MATURITY + mature).await?;
    Ok(())
}

/// Restart zebra paying this user, mine through coinbase maturity.
pub async fn fund_user(zebra: &mut Zebrad, user: &User) -> Result<()> {
    fund_user_coinbases(zebra, user, 1).await
}

/// ZIP-32 account 0 Ironwood FVK of the claim user (not Registry).
pub fn user_orchard_fvk() -> Result<orchard::keys::FullViewingKey> {
    let usk = account_usk(&ceremony::regtest_network(), &USER_SEED, 0)?;
    Ok(orchard::keys::FullViewingKey::from(usk.orchard()))
}

/// Spend a mature user coinbase to the Treasury with
/// `ZNS:claim:forever:<name>:<ua>`.
///
/// Overpays: the whole coinbase minus ZIP-317. Call after mint is live so
/// the note is an instruction, not pre-birth balance.
pub async fn pay_claim(zebra: &mut Zebrad, user: &User, name: &str) -> Result<String> {
    pay_treasury(
        zebra,
        user,
        &format!("ZNS:claim:forever:{name}:{}", user.ua),
    )
    .await
}

/// Spend a mature user coinbase to the Treasury with `memo_text`.
pub async fn pay_treasury(zebra: &mut Zebrad, user: &User, memo_text: &str) -> Result<String> {
    let network = ceremony::regtest_network();
    let (taddr, child) = taddr_for_seed(&network, &USER_SEED)?;
    let user_usk = account_usk(&network, &USER_SEED, 0)?;
    let treasury_usk = account_usk(&network, &DEV_SEED, 0)?;

    let tip = zebra.tip_height().await?;
    let coins = collect_mature_coinbase(zebra, &network, &taddr, tip).await?;
    if coins.is_empty() {
        bail!(
            "no mature user coinbase to {}; fund_user first",
            user.miner_address
        );
    }

    let sk = user_usk
        .transparent()
        .derive_external_secret_key(child)
        .map_err(|e| anyhow!("derive user miner secret key: {e}"))?;

    let anchor = zebra.ironwood_anchor().await?;
    let memo: MemoBytes = memo_text
        .parse::<Memo>()
        .map_err(|e| anyhow!("treasury memo: {e}"))?
        .into();

    let target = BlockHeight::from_u32(tip + 1);
    eprintln!("claim: proving Ironwood payment at target {target}");
    let tx = tokio::task::spawn_blocking(move || {
        build_claim_tx(network, sk, coins, anchor, treasury_usk, memo, target)
    })
    .await
    .context("claim proving task")??;
    let txid = tx.txid().to_string();
    eprintln!("claim: broadcast {txid}");

    zebra
        .ensure_rpc()
        .await
        .context("zebrad after claim prove")?;
    let mut raw = Vec::new();
    tx.write(&mut raw).context("serialize claim tx")?;
    zebra
        .rpc("sendrawtransaction", serde_json::json!([hex::encode(&raw)]))
        .await
        .context("sendrawtransaction claim")?;
    Ok(txid)
}

fn build_claim_tx(
    network: zcash_protocol::local_consensus::LocalNetwork,
    miner_sk: secp256k1::SecretKey,
    coins: Vec<ceremony::Coin>,
    anchor: orchard::Anchor,
    treasury_usk: UnifiedSpendingKey,
    memo: MemoBytes,
    target: BlockHeight,
) -> Result<Transaction> {
    let coin = coins
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no coinbase inputs"))?;
    let input_value = coin.coin.value();

    // Payment + even pad. Outputs-only, so the current tree root is the anchor.
    let ironwood_actions = 2;
    let fee = Zip317::standard()
        .fee_required(
            &network,
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
            "user coinbase {} zats cannot cover fee {}",
            input_value.into_u64(),
            fee.into_u64()
        );
    }
    let payment = Zatoshis::from_u64(input_value.into_u64() - fee.into_u64())
        .expect("input minus fee is in range");
    eprintln!(
        "claim: coinbase {} zats, fee {}, payment {}",
        input_value.into_u64(),
        fee.into_u64(),
        payment.into_u64()
    );

    // Transparent side: the user's P2PKH coinbase input.
    let mut signing = TransparentSigningSet::new();
    let pubkey = signing.add_key(miner_sk);
    let mut transparent_builder = TransparentBuilder::empty();
    transparent_builder.add_input(TransparentInputInfo::from_parts(
        coin.outpoint,
        coin.coin,
        SpendInfo::P2pkh { pubkey },
    )?);
    let transparent = transparent_builder.build();

    // Ironwood side: the payment carrying the claim memo, plus one zero
    // pad so the action count is even. UNPADDED at the current anchor.
    let mut ironwood_builder = OrchardBuilder::new(
        BundleType::UNPADDED,
        BundleVersion::ironwood_v3(),
        BundleVersion::ironwood_v3().default_flags(),
        anchor,
    )?;
    let treasury_fvk = orchard::keys::FullViewingKey::from(treasury_usk.orchard());
    let treasury_addr = treasury_fvk.address_at(0u32, orchard::keys::Scope::External);
    ironwood_builder.add_output(
        Some(treasury_fvk.to_ovk(orchard::keys::Scope::External)),
        treasury_addr,
        orchard::value::NoteValue::from_raw(payment.into_u64()),
        *memo.as_array(),
    )?;
    let user_fvk = orchard::keys::FullViewingKey::from(
        account_usk(&network, &USER_SEED, 0)
            .expect("user USK")
            .orchard(),
    );
    ironwood_builder.add_output(
        Some(user_fvk.to_ovk(orchard::keys::Scope::External)),
        user_fvk.address_at(0u32, orchard::keys::Scope::External),
        orchard::value::NoteValue::from_raw(0),
        [0; 512],
    )?;
    let (ironwood, _) = ironwood_builder
        .build::<ZatBalance>(&mut OsRng)?
        .expect("ironwood bundle exists");

    assemble_v6_transparent_ironwood(&network, target, transparent, ironwood, &signing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_is_not_the_mint_seed() {
        let user = User::new().expect("derive");
        let mint_miner = ceremony::miner_address().unwrap();
        assert_ne!(user.miner_address, mint_miner);
        assert!(user.ua.starts_with("uregtest1"), "got {}", user.ua);
        assert_ne!(user.ua, ceremony::treasury_ua().unwrap());
    }
}
