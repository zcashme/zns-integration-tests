//! Happy-path claim, then unhappy memos, then a user spend attempt of alice.
//!
//! `src/register.rs` — mint settlement + `zns-verify` field checks.
//! `src/non_request.rs` — garbage / missing-term / invalid-name intake.
//! `src/bad_spend.rs` — user FVK cannot spend the Registry Name Note.

use anyhow::Result;
use zns_integration_tests::{claim_alice, pay_invalid_memos, user_cannot_spend_alice, Stack};

#[tokio::test(flavor = "multi_thread")]
async fn happy_path_claim_alice() -> Result<()> {
    // 4 mature coinbases: 1 claim + 3 unhappy memos. Spend attempt needs none.
    let Some(mut stack) = Stack::start(4).await? else {
        return Ok(());
    };

    let note = claim_alice(&mut stack).await?;
    pay_invalid_memos(&mut stack).await?;
    user_cannot_spend_alice(&mut stack, &note).await?;
    Ok(())
}
