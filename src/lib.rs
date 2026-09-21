//! Cross-stack process harness for ZNS integration tests.

mod bad_spend;
mod binaries;
mod ceremony;
mod child;
mod claim;
mod harness;
mod mint;
mod non_request;
mod register;
mod resolver;
mod rpc;
mod tx;
mod verify;
mod zebra;

pub use bad_spend::user_cannot_spend_alice;
pub use binaries::zebrad_bin;
pub use ceremony::{miner_address, publish, treasury_ua, FIXTURE_HEIGHT};
pub use claim::{fund_user, fund_user_coinbases, pay_claim, pay_treasury, User};
pub use harness::Stack;
pub use mint::Mint;
pub use non_request::pay_invalid_memos;
pub use register::claim_alice;
pub use resolver::Resolver;
pub use verify::{
    registration_txid, registry_commitment_keys, wait_for_verified_name_note, VerifiedNameNote,
    VECTOR_G_D, VECTOR_PK_D,
};
pub use zebra::Zebrad;
