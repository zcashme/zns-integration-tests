# zns-integration-tests

Local process harness for the Zcash Name Service: `zebrad` (regtest),
[`zns-mint`](https://github.com/zcashme/zns-mint), and
[`zns-resolver`](https://github.com/zcashme/zns-resolver).

`Stack::start()` launches all three. Mint is built with
`--features regtest,fake-tee` from `../zns-mint` (override with
`$ZNS_MINT_DIR` / `$ZNS_MINT_BIN`). If no capsule is provided, the harness
runs mint's `write_fake_capsule` example (FakeTee, all-zero seed).

Before mint starts, the harness mines 104 blocks and publishes a **dev**
ceremony: 40 zero-value Registry Ironwood notes plus a Treasury Ironwood
note, funded from coinbase paid to the all-zero-seed Treasury t-addr.
That first Ironwood bundle is 42 actions (proving takes a couple of minutes).

```sh
export ZEBRAD_BIN=/path/to/zebrad
cargo test --test spin_up -- --nocapture
cargo test --test claim -- --nocapture
```

`claim` funds a second ZIP-32 user (not the mint seed), pays the Treasury
`ZNS:claim:alice:<user UA>`, and waits for mint to submit the Name Note.
Skips locally if `zebrad` is missing; CI requires `$ZEBRAD_BIN`.
CI downloads Sapling params into `$ZCASH_PARAMS_DIR`. Locally, put
`sapling-spend.params` and `sapling-output.params` in `~/.zcash-params`
(from https://download.z.cash/downloads/).

## License

MIT. See [LICENSE](LICENSE).
