//! Happy-path user claim: a real Zallet wallet pays the Treasury an
//! Ironwood note with a ZNS memo.
//!
//! The user is a genuine wallet — its own generated mnemonic, its own scan —
//! not in-process signing: zebrad mines coinbase to the wallet's transparent
//! address, `z_shieldcoinbase` moves it to Orchard, and claims pay through
//! `z_sendfromaccount`. Distinct from the all-zero mint seed (`[0; 32]`) by
//! construction; nothing in the harness holds the user's seed.

use anyhow::{bail, Result};
use serde_json::json;

use crate::ceremony::{treasury_ua, COINBASE_MATURITY};
use crate::zallet::Zallet;
use crate::zebra::Zebrad;

/// ZEC sent per claim payment. Deliberately overpays mint's oracle-priced
/// fee — claims are fail-closed and an underpay is a dead attempt — and
/// matches the amount the first-generation harness proved against mint.
const CLAIM_PAYMENT_ZEC: f64 = 2.0;

const SYNC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// The run's spending plan — four 2.0 ZEC payments plus fees, with
/// margin — as the spendable shielded balance the wallet must hold
/// before the run starts. Funded by waiting for the wallet's scan to
/// reach the node's mature-coinbase truth, then one shield, so the
/// budget is real, not assumed.
const USER_BUDGET_ZATS: u64 = 900_000_000;

/// A funded Zallet wallet playing the user.
pub struct User {
    pub zallet: Zallet,
    /// Transparent address zebrad mines to so the wallet holds coinbase.
    pub miner_address: String,
    /// The wallet-derived Orchard UA that goes into claim memos. The wallet
    /// can detect payments made back to it, which later update/release and
    /// refund scenarios rely on.
    pub ua: String,
}

impl User {
    /// Bring up a funded user: fresh Zallet wallet, mature coinbase, and a
    /// shielded balance, with the wallet synced to the chain.
    ///
    /// Restarts zebrad — call before the ceremony and before mint runs:
    /// restarts drop non-finalized blocks, so this leaves the ceremony on
    /// finalized, restart-safe history.
    pub async fn fund(zebra: &mut Zebrad, mature_coinbases: u32) -> Result<Self> {
        if mature_coinbases == 0 {
            bail!("fund requires at least one mature coinbase");
        }

        let mut zallet = Zallet::init(zebra)?;
        zebra.restart_with_miner(&zallet.miner_address).await?;
        zebra
            .generate_blocks(COINBASE_MATURITY + mature_coinbases)
            .await?;
        let target = zebra.tip_height().await?;

        zallet.start_daemon().await?;
        zallet.wait_until_synced(target, SYNC_TIMEOUT).await?;
        // The wallet's scan must actually reach the chain before shielding:
        // the balance scan trails the sync engine (status-synced is not
        // scan-complete), and the shield snapshots whatever the wallet has
        // scanned so far — seen on CI as a shield capturing 1 of 5 mature
        // coinbases. Wait until the wallet's coinbase-spendable view equals
        // the node's mature-coinbase truth, then shield once.
        let truth = crate::ceremony::mature_coinbase_zats(zebra, &zallet.miner_address).await?;
        zallet.wait_until_sees_coinbase(truth, SYNC_TIMEOUT).await?;
        zallet.shield_coinbase().await?;
        zebra.generate_blocks(1).await?; // confirm the shield tx
        let target = zebra.tip_height().await?;
        zallet.wait_until_synced(target, SYNC_TIMEOUT).await?;
        // The fresh shielded note lands in the scan after the confirming
        // block — wait for the budget instead of asserting it on arrival.
        zallet
            .wait_until_shielded(USER_BUDGET_ZATS, SYNC_TIMEOUT)
            .await?;

        let ua = zallet.orchard_ua().await?;
        eprintln!("user wallet miner: {}", zallet.miner_address);
        eprintln!("user wallet orchard UA: {ua}");
        Ok(Self {
            miner_address: zallet.miner_address.clone(),
            ua,
            zallet,
        })
    }

    /// Spend shielded funds to the Treasury with `memo_text`; returns the txid.
    pub async fn pay_treasury(&self, memo_text: &str) -> Result<String> {
        // Belt-and-braces: refuse to spend while the wallet reports itself
        // locked (only ever true around daemon start; a mid-run block does
        // not flip it, but the wait is free).
        self.zallet.wait_until_synced(0, SYNC_TIMEOUT).await?;
        let memo_hex: String = memo_text.bytes().map(|b| format!("{b:02x}")).collect();
        let recipients = json!([
            {
                "address": treasury_ua()?,
                "amount": CLAIM_PAYMENT_ZEC,
                "memo": memo_hex,
            }
        ]);
        self.zallet
            .send_from_account("orchard", recipients, "FullPrivacy")
            .await
    }

    /// Pay the Treasury `ZNS:claim:forever:<name>:<ua>`.
    pub async fn pay_claim(&self, name: &str) -> Result<String> {
        self.pay_treasury(&format!("ZNS:claim:forever:{name}:{}", self.ua))
            .await
    }
}
