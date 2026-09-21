//! Piecewise v6 tx assembly: transparent + Ironwood, Sapling slot empty.
//!
//! Ceremony and claim both build that shape; one place owns the sign/prove/freeze
//! sequence so branch, expiry, and proving-key choice stay aligned.

use anyhow::Result;
use rand::rngs::OsRng;
use transparent::builder::{TransparentSigningSet, Unauthorized};
use transparent::bundle::Bundle as TransparentBundle;
use zcash_primitives::transaction::builder::{cached_orchard_proving_key, DEFAULT_TX_EXPIRY_DELTA};
use zcash_primitives::transaction::components::orchard::bundle_version_for_branch;
use zcash_primitives::transaction::sighash::{signature_hash, SignableInput};
use zcash_primitives::transaction::txid::TxIdDigester;
use zcash_primitives::transaction::{self, Transaction, TransactionData};
use zcash_protocol::consensus::{BlockHeight, BranchId, Parameters};
use zcash_protocol::value::ZatBalance;

type UnprovenIronwood = orchard::Bundle<
    orchard::builder::InProgress<orchard::builder::Unproven, orchard::builder::Unauthorized>,
    ZatBalance,
>;

/// Sign the transparent inputs, prove+sign the Ironwood bundle, freeze a v6 tx.
///
/// `transparent` and `ironwood` must already be built (unsigned). Sapling is
/// always `None`. Expiry is `target + DEFAULT_TX_EXPIRY_DELTA`.
pub fn assemble_v6_transparent_ironwood<P: Parameters>(
    network: &P,
    target: BlockHeight,
    transparent: Option<TransparentBundle<Unauthorized>>,
    ironwood: UnprovenIronwood,
    signing: &TransparentSigningSet,
) -> Result<Transaction> {
    let branch_id = BranchId::for_height(network, target);
    let expiry = target + DEFAULT_TX_EXPIRY_DELTA;

    let unauthed: TransactionData<transaction::Unauthorized> = TransactionData::from_parts_v6(
        branch_id,
        0,
        expiry,
        transparent.clone(),
        None,
        None,
        Some(ironwood.clone()),
    );
    let txid_parts = unauthed.digest(TxIdDigester);

    let transparent = transparent
        .map(|b| {
            b.apply_signatures(
                |index| {
                    *signature_hash(&unauthed, &SignableInput::Transparent(index), &txid_parts)
                        .as_ref()
                },
                signing,
            )
        })
        .transpose()?;

    let bundle_v = bundle_version_for_branch(branch_id, orchard::ValuePool::Ironwood)
        .expect("ironwood bundle implies NU6.3");
    let ironwood = ironwood
        .create_proof(
            cached_orchard_proving_key(bundle_v.circuit_version()),
            &mut OsRng,
        )?
        .prepare(
            OsRng,
            *signature_hash(&unauthed, &SignableInput::Shielded, &txid_parts).as_ref(),
        )
        .finalize()?;

    Ok(TransactionData::from_parts_v6(
        branch_id,
        0,
        expiry,
        transparent,
        None,
        None,
        Some(ironwood),
    )
    .freeze()?)
}
