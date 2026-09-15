# zns-integration-tests

End-to-end tests for the Zcash Name Service (ZNS) stack: mint, verify, and
resolver, checked against the whitepaper.

This harness exercises protocol behavior that the component repos cannot cover
alone — claim / update / release flows, Name Note verification, and name → UA
resolution — without running a TEE.

## In scope

| Repo | Role |
|---|---|
| [zns-mint](https://github.com/zcashme/zns-mint) | Issues Name Notes (claim, update, release) |
| [zns-verify](https://github.com/zcashme/zns-verify) | Recomputes Name Note commitments; shared verification kernel |
| [zns-resolver](https://github.com/zcashme/zns-resolver) | Indexes verified bindings and serves name → UA lookup |
| [zns-whitepaper](https://github.com/zcashme/zns-whitepaper) | Normative protocol; tests assert documented rules |

## Out of scope

TEE / SEV-SNP attestation, capsule decrypt, and mint boot-in-enclave. Those
belong in `zns-mint`. This repo assumes a local, non-attested mint.

## Status

Scaffold only. The test harness is not in this commit.

## License

MIT. See [LICENSE](LICENSE).
