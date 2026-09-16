# zns-integration-tests

Local process harness for the Zcash Name Service: `zebrad` (regtest),
[`zns-mint`](https://github.com/zcashme/zns-mint), and
[`zns-resolver`](https://github.com/zcashme/zns-resolver).

`Stack::start()` launches all three. Mint is built with
`--features regtest,fake-tee` from `../zns-mint` (override with
`$ZNS_MINT_DIR` / `$ZNS_MINT_BIN`). If no capsule is provided, the harness
runs mint's `write_fake_capsule` example (FakeTee, all-zero seed).

```sh
export ZEBRAD_BIN=/path/to/zebrad
cargo test --test spin_up -- --nocapture
```

Skips locally if `zebrad` is missing; CI requires `$ZEBRAD_BIN`.

TODO: create the 40-note Registry ceremony (and fund Treasury) on the
regtest chain before mint starts, so mint can finish boot.

## License

MIT. See [LICENSE](LICENSE).
