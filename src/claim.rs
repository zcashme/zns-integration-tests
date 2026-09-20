//! Happy-path user claim: pay the Treasury an Ironwood note with a ZNS memo.
//!
//! Distinct from the FakeTee mint seed (`[0; 32]`). The user is a second
//! ZIP-32 wallet: coinbase to their t-addr, then one outputs-only Ironwood
//! bundle to the Treasury UA.

use anyhow::{anyhow, bail, Context, Result};
use rand::rngs::OsRng;
use transparent::builder::TransparentSigningSet;
use zcash_keys::encoding::encode_transparent_address_p;
use zcash_keys::keys::UnifiedSpendingKey;
use zcash_primitives::transaction::builder::{BuildConfig, Builder, BundlePadding};
use zcash_primitives::transaction::fees::transparent::InputSize;
use zcash_primitives::transaction::fees::zip317::{self, FeeRule as Zip317};
use zcash_primitives::transaction::fees::FeeRule as _;
use zcash_primitives::transaction::Transaction;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::memo::{Memo, MemoBytes};
use zcash_protocol::value::Zatoshis;

use crate::ceremony::{
    self, account_usk, collect_mature_coinbase, orchard_ua, sapling_provers, taddr_for_seed,
    COINBASE_MATURITY, DEV_SEED,
};
use crate::zebra::Zebrad;

/// Not the mint's all-zero seed.
const USER_SEED: [u8; 32] = [1u8; 32];

/// Blocks to mine after switching the miner so one user coinbase is mature.
pub const USER_FUND_BLOCKS: u32 = COINBASE_MATURITY + 1;

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

/// Restart zebra paying this user, mine through coinbase maturity.
pub async fn fund_user(zebra: &mut Zebrad, user: &User) -> Result<()> {
    zebra.restart_with_miner(&user.miner_address).await?;
    zebra.generate_blocks(USER_FUND_BLOCKS).await?;
    Ok(())
}

/// Spend a mature user coinbase to the Treasury with
/// `ZNS:claim:forever:<name>:<ua>`.
///
/// Overpays: the whole coinbase minus ZIP-317. Call after mint is live so
/// the note is an instruction, not pre-birth balance.
pub async fn pay_claim(zebra: &mut Zebrad, user: &User, name: &str) -> Result<String> {
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
    let memo_text = format!("ZNS:claim:forever:{name}:{}", user.ua);
    let memo: MemoBytes = memo_text
        .parse::<Memo>()
        .map_err(|e| anyhow!("claim memo: {e}"))?
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
    log_treasury_decrypts(&tx, &account_usk(&network, &DEV_SEED, 0)?);

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

    let mut signing = TransparentSigningSet::new();
    let pubkey = signing.add_key(miner_sk);
    let treasury_fvk = orchard::keys::FullViewingKey::from(treasury_usk.orchard());
    let treasury_addr = treasury_fvk.address_at(0u32, orchard::keys::Scope::External);
    let treasury_ovk = Some(treasury_fvk.to_ovk(orchard::keys::Scope::External));
    let user_fvk = orchard::keys::FullViewingKey::from(
        account_usk(&network, &USER_SEED, 0)
            .expect("user USK")
            .orchard(),
    );
    let pad_addr = user_fvk.address_at(0u32, orchard::keys::Scope::External);
    let pad_ovk = Some(user_fvk.to_ovk(orchard::keys::Scope::External));

    let mut builder = Builder::new(
        network,
        target,
        BuildConfig::Standard {
            sapling_anchor: None,
            orchard_anchor: None,
            ironwood_anchor: Some(anchor),
            orchard_padding: BundlePadding::UNPADDED,
            ironwood_padding: BundlePadding::UNPADDED,
        },
    );
    builder
        .add_transparent_p2pkh_input(pubkey, coin.outpoint, coin.coin)
        .map_err(|e| anyhow!("transparent input: {e}"))?;
    builder
        .add_ironwood_output::<zip317::FeeError>(treasury_ovk, treasury_addr, payment, memo)
        .map_err(|e| anyhow!("claim payment output: {e}"))?;
    builder
        .add_ironwood_output::<zip317::FeeError>(
            pad_ovk,
            pad_addr,
            Zatoshis::ZERO,
            MemoBytes::empty(),
        )
        .map_err(|e| anyhow!("ironwood padding output: {e}"))?;

    let provers = sapling_provers()?;
    let built = builder
        .build(
            &signing,
            &[],
            &[],
            OsRng,
            &provers.spend,
            &provers.output,
            &Zip317::standard(),
        )
        .map_err(|e| anyhow!("prove/sign claim tx: {e}"))?;
    Ok(built.transaction().clone())
}

fn log_treasury_decrypts(tx: &Transaction, treasury_usk: &UnifiedSpendingKey) {
    let fvk = orchard::keys::FullViewingKey::from(treasury_usk.orchard());
    let ivk = fvk.to_ivk(orchard::keys::Scope::External).prepare();
    let Some(bundle) = tx.ironwood_bundle() else {
        eprintln!("claim: no ironwood bundle");
        return;
    };
    eprintln!(
        "claim: ironwood v {:?} actions={}",
        bundle.bundle_version(),
        bundle.actions().len()
    );
    for (i, action) in bundle.actions().iter().enumerate() {
        let domain = orchard::note_encryption::IronwoodDomain::for_action(action);
        match zcash_note_encryption::try_note_decryption(&domain, &ivk, action) {
            Some((note, _, memo)) => {
                let end = memo.iter().position(|b| *b == 0).unwrap_or(memo.len());
                eprintln!(
                    "claim: decrypted Treasury payment (action {i}, {} zats): {:?}",
                    note.value().inner(),
                    String::from_utf8_lossy(&memo[..end.min(160)])
                );
            }
            None => eprintln!(
                "claim: action {i} is the even-count pad to the user; Treasury keys cannot open it (expected)"
            ),
        }
    }
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
