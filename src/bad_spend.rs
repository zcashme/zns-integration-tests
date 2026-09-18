//! Bad-actor spend of a Registry Name Note using the *user* FVK.
//!
//! The user paid for `alice` and that UA is in the memo, but the note's
//! recipient is Registry. `add_zns_spend` with the user FVK must fail
//! `FvkMismatch` even when `zns-verify` supplies the Name Note `(ψ, rcm)`.
//! After the failed attempt, `zns-verify` still finds the same claim.

use anyhow::{anyhow, bail, Result};
use zcash_primitives::transaction::builder::{BuildConfig, Builder, BundlePadding};
use zcash_primitives::transaction::fees::zip317;
use zcash_protocol::consensus::BlockHeight;

use crate::ceremony;
use crate::claim::user_orchard_fvk;
use crate::harness::Stack;
use crate::verify::{find_verified_name_note, load_registry_name_note, VerifiedNameNote};

/// Try to spend `alice` as the claim user; then re-verify the original note.
///
/// Checks: orchard `FvkMismatch` (user does not own the output). Dummy
/// Merkle path is only an argument; FVK is rejected first.
/// Does not check: a Registry-key spend (update/release), nullifier
/// publication, or that zebrad would reject a forged bundle (we never
/// produce one).
pub async fn user_cannot_spend_alice(stack: &mut Stack, claimed: &VerifiedNameNote) -> Result<()> {
    let loaded = load_registry_name_note(&stack.zebra, claimed.height, &claimed.txid, "alice")
        .await?
        .ok_or_else(|| anyhow!("alice Name Note gone from height {}", claimed.height))?;

    let orchard_note = loaded.orchard_note;
    let g_d = loaded.g_d;
    let pk_d = loaded.pk_d;
    let rho = loaded.rho;
    let cmx = loaded.cmx;
    let value = orchard_note.value().inner();
    let payload = loaded.payload()?;
    let (psi, rcm) =
        zns_verify::verify_name_note_with_witness(&payload, g_d, pk_d, value, rho, cmx)
            .ok_or_else(|| anyhow!("zns-verify opening failed for on-chain alice"))?;

    let user_fvk = user_orchard_fvk()?;
    let network = ceremony::regtest_network();
    let tip = stack.zebra.tip_height().await?;
    let target = BlockHeight::from_u32(tip + 1);
    let anchor = stack.zebra.ironwood_anchor().await?;
    let path = dummy_ironwood_path();
    let trapdoor = orchard::note::NoteCommitTrapdoor::from_inner(rcm);

    let err = {
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
        builder.add_zns_spend::<zip317::FeeError>(user_fvk, orchard_note, path, trapdoor, psi)
    };

    match err {
        Err(e) if e.to_string().contains("FullViewingKey does not correspond") => {
            eprintln!("user FVK cannot spend alice Name Note: {e}");
        }
        other => {
            bail!("expected FvkMismatch spending alice as the user, got {other:?}");
        }
    }

    // Name Note still decrypts and `cmx` still matches (unspent, unchanged).
    let still = find_verified_name_note(&stack.zebra, claimed.height, "alice")
        .await?
        .ok_or_else(|| anyhow!("alice Name Note missing after spend attempt"))?;
    assert_eq!(still.txid, claimed.txid);
    assert_eq!(still.action, "claim");
    assert_eq!(still.ua, claimed.ua);
    assert_eq!(still.g_d, claimed.g_d);
    assert_eq!(still.pk_d, claimed.pk_d);
    eprintln!(
        "alice still verified after user spend attempt txid={}",
        still.txid
    );
    Ok(())
}

fn dummy_ironwood_path() -> orchard::tree::MerklePath {
    // Orchard uncommitted leaf is pallas::Base(2). Never checked: FVK fails first.
    let mut bytes = [0u8; 32];
    bytes[0] = 2;
    let leaf = Option::from(orchard::tree::MerkleHashOrchard::from_bytes(&bytes))
        .expect("2 is a canonical Pallas base");
    orchard::tree::MerklePath::from_parts(0, [leaf; orchard::NOTE_COMMITMENT_TREE_DEPTH])
}
