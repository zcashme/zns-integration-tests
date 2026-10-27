//! Live reorg of the mint queues. Needs `zebrad`, `zallet-zebra`, and a
//! sibling `zns-mint`. Skips when either binary is missing.
//!
//! ```sh
//! cargo test --test reorg -- --nocapture
//! ```

use zns_integration_tests::Stack;

#[tokio::test(flavor = "multi_thread")]
async fn reorg_queues() {
    // Spendable coinbases `User::fund` mines and shields. The run then
    // spends five 2 ZEC payments and three 0.1 ZEC probes (~10.3 ZEC).
    // `fund` only returns once the shield is at least 9 ZEC, so 16 is
    // headroom for that wait and the payments after it.
    let Some(mut stack) = Stack::start(16).await.expect("stack") else {
        return;
    };
    zns_integration_tests::reorg_queues(&mut stack)
        .await
        .expect("reorg queues");
}
