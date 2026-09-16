//! Bring up zebra, mint, and resolver for a local e2e run.

use anyhow::Result;

use crate::ceremony::{self, FIXTURE_HEIGHT};
use crate::mint::Mint;
use crate::resolver::Resolver;
use crate::zebra::Zebrad;

/// The three processes the Level 1 harness drives.
///
/// Dropping the stack kills all children.
pub struct Stack {
    pub zebra: Zebrad,
    pub mint: Mint,
    pub resolver: Resolver,
}

impl Stack {
    /// Start zebra (regtest, mint's NU schedule), mine through coinbase
    /// maturity, publish the FakeTee ceremony + Treasury, then mint and
    /// resolver.
    pub async fn start() -> Result<Self> {
        let mint_build = tokio::task::spawn_blocking(Mint::build);
        let resolver_build = tokio::task::spawn_blocking(Resolver::build);

        let miner = ceremony::miner_address()?;
        let mut zebra = Zebrad::start_with_miner(&miner).await?;
        zebra.generate_blocks(FIXTURE_HEIGHT).await?;
        ceremony::publish(&mut zebra).await?;

        let mint_bin = mint_build.await.expect("mint build task")?;
        let resolver_bin = resolver_build.await.expect("resolver build task")?;

        let (mint, resolver) =
            tokio::try_join!(Mint::start(mint_bin), Resolver::start(resolver_bin))?;
        Ok(Self {
            zebra,
            mint,
            resolver,
        })
    }
}
