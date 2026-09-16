//! Bring up zebra, mint, and resolver for a local e2e run.

use anyhow::Result;

use crate::mint::Mint;
use crate::resolver::Resolver;
use crate::zebra::{Zebrad, NU6_3_ACTIVATION_HEIGHT};

/// The three processes the Level 1 harness drives.
///
/// Dropping the stack kills all children.
pub struct Stack {
    pub zebra: Zebrad,
    pub mint: Mint,
    pub resolver: Resolver,
}

impl Stack {
    /// Start zebra (regtest, mint's NU schedule), mine through NU6.3, then
    /// mint (`--features regtest,fake-tee`) and resolver (testnet binary so
    /// RPC binds).
    pub async fn start() -> Result<Self> {
        let mint_build = tokio::task::spawn_blocking(Mint::build);
        let resolver_build = tokio::task::spawn_blocking(Resolver::build);

        let zebra = Zebrad::start().await?;
        zebra.generate_blocks(NU6_3_ACTIVATION_HEIGHT).await?;

        // TODO: publish the 40 zero-value Registry Ironwood notes (keygen
        // ceremony) and a funded Treasury Ironwood note before starting mint.
        // Boot requires `ANCHOR_POOL_SIZE == 40` and `MIN_TREASURY_BALANCE`.

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
