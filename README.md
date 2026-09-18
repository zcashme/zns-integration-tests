# zns-integration-tests

Local process harness for the Zcash Name Service: `zebrad` (regtest) and
[`zns-mint`](https://github.com/zcashme/zns-mint). Mint is built with
`--features regtest,fake-tee` from `../zns-mint` (override with
`$ZNS_MINT_DIR` / `$ZNS_MINT_BIN`). If no capsule is provided, the harness
runs mint's `write_fake_capsule` example (FakeTee, all-zero seed).

Before mint starts, the harness mines 104 blocks and publishes a **dev**
ceremony: 40 zero-value Registry Ironwood notes plus a Treasury Ironwood
note, funded from coinbase paid to the all-zero-seed Treasury t-addr.
That first Ironwood bundle is 42 actions (proving takes a couple of minutes).

```sh
export ZEBRAD_BIN=/path/to/zebrad
cargo test --test claim -- --nocapture
```

`claim` (`happy_path_claim_alice`) is a FakeTee **regtest** happy path, not a
TEE or mainnet audit. A second ZIP-32 user (seed `[1; 32]`, not mint's
all-zero seed) pays Treasury `ZNS:claim:forever:alice:<user UA>` (overpay:
coinbase minus ZIP-317). The test waits for mint to log the Name Note in
flight, mines it, and checks the on-chain action with
[`zns-verify`](https://github.com/zcashme/zns-verify) (not mint's decoder):
trial-decrypt under the Registry FVK, recipient is Registry j=0, memo
parses, and `verify_name_note` reproduces `cmx` from the memo fields,
`(g_d, pk_d)`, value, and `rho`. Asserted fields: name `alice`, action
`claim`, UA is this user, `expires_at=none`, value 0, `(g_d, pk_d)` are
the FakeTee Registry keys (LocalNetwork coin_type 1; not the mainnet
whitepaper vector). If mint logs a registration `txid=`, it must match
the on-chain note. The test fails if mint treats that payment as a
non-request, does not authorize alice, or rejects the Name Note.

It does **not** check the following.

- **TEE / network.** Mint runs FakeTee on regtest. There is no SEV-SNP
  attestation, no production seed capsule, and Registry `(g_d, pk_d)` are
  LocalNetwork (coin_type 1), not the mainnet whitepaper vector.
- **Vault sweep.** After boot, Treasury may try to move excess to the
  P2PKH vault. The test continues if that proposal fails. Claim fees are
  paid from the ceremony Treasury note, not from a successful sweep.
- **Claim price.** The user overpays (whole coinbase minus ZIP-317). The
  test does not check that the payment equals mint's oracle-priced
  forever-claim fee, or that an underpay is rejected.
- **Payment ↔ Name Note binding.** It records the Treasury *payment*
  txid only to catch `non-request payment` on that tx. The on-chain
  check is the *registration* tx (Name Note to Registry). It does not
  prove mint spent that specific payment note to fund the Name Note.
- **Name Note spend-authority.** `zns-verify` checks decrypt + memo parse
  + `cmx` reproduction. It does not check the nullifier, spend
  authorization, or that only Registry can spend the note.
- **Mint decoder / resolver.** Memo parse and `cmx` use `zns-verify`,
  not mint's inbound parser. Nothing queries a lightwalletd/resolver for
  `alice`.

Skips locally if `zebrad` is missing; CI requires `$ZEBRAD_BIN`.
CI downloads Sapling params into `$ZCASH_PARAMS_DIR`. Locally, put
`sapling-spend.params` and `sapling-output.params` in `~/.zcash-params`
(from https://download.z.cash/downloads/).

## License

MIT. See [LICENSE](LICENSE).
