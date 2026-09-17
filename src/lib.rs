//! Cross-stack process harness for ZNS integration tests.

mod binaries;
mod ceremony;
mod child;
mod claim;
mod mint;
mod resolver;
mod rpc;
mod verify;
mod zebra;

pub use binaries::zebrad_bin;
pub use ceremony::{miner_address, publish, treasury_ua, FIXTURE_HEIGHT};
pub use claim::{fund_user, pay_claim, User};
pub use mint::Mint;
pub use resolver::Resolver;
pub use verify::{
    registration_txid, registry_commitment_keys, wait_for_verified_name_note, VerifiedNameNote,
    VECTOR_G_D, VECTOR_PK_D,
};
pub use zebra::Zebrad;
