# Security

## Dependency security posture

The RustSec advisory gate is enforced by `cargo audit` (run as part of
`just check`). The locked dependency graph currently resolves all reported
RustSec advisories with no suppression:

- **TLS stack** (transitive via the HTTP client): the six
  `rustls`/`rustls-webpki`/`webpki-root-certs` advisories
  (RUSTSEC-2026-0346, 0347, 0348, 0349, 0382, 0383) are cleared by the locked
  `rustls 0.23.46`, `rustls-webpki 0.103.16` and `webpki-root-certs 1.0.11`,
  all at or above their patched versions.
- **Tokio unsoundness warning**: resolved by the locked `tokio 1.54.1`, which
  is within the patched range (`>= 1.53.3`) for the `unsound` advisories
  (RUSTSEC-2026-0386 / RUSTSEC-0000-0000). The supported fix is therefore
  applied; no explicit reachability exemption is required.

No blanket `ignore` entries were added for these advisories; `audit.toml`
carries only the pre-existing `RUSTSEC-2024-0384` exclusion. Reproduce with:

```
cargo audit
cargo deny check all
```
