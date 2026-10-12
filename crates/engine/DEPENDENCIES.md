# chua-engine dependency manifest

Every dependency of `chua-engine`, with its licence expression as published on
crates.io. Policy: `CLEANROOM.md` R5 (permissive only) and `.omp/DECISIONS.md`
ADR-0005, as extended by ADR-0008 (`Unicode-3.0`).

Verified with `cargo info <crate>`, 2026-10-12.

| Crate | Version | Licence expression | Notes |
|---|---|---|---|
| `tokio` | 1.53 | MIT | Runtime: `rt-multi-thread`, `macros`, `net`, `time`, `sync`, `io-util`, `fs` |
| `reqwest` | 0.13 | MIT OR Apache-2.0 | HTTP client. Default features off; `rustls`, `http2`, `system-proxy`, `charset` on |
| `rusqlite` | 0.40 | MIT | SQLite binding. Default features off; `bundled` on |
| `thiserror` | 2.0 | MIT OR Apache-2.0 | Error derive |
| `serde` | 1.0 | MIT OR Apache-2.0 | Derive only; no runtime codec of its own |
| `serde_json` | 1.0 | MIT OR Apache-2.0 | Persistence of structured failures |
| `url` | 2.5 | MIT OR Apache-2.0 | URL parsing, redirect targets, path segments |
| `bytes` | 1.12 | MIT | Body chunks |
| `futures` | 0.3 | MIT OR Apache-2.0 | `StreamExt` for the body iterator |
| `tracing` | 0.1 | MIT | Structured diagnostics |
| `tempfile` (dev) | 3.27 | MIT OR Apache-2.0 | Temporary directories in tests |

## Transitive licences that need naming

`ADR-0008` extends the whitelist with **`Unicode-3.0`**, which arrives through
`url → idna → icu_*`. That whole subtree is permissive and OSI-approved; it is
listed here so the expansion is explicit rather than silent.

The TLS stack is `rustls` (Apache-2.0 OR ISC OR MIT) plus
`rustls-platform-verifier` (MIT OR Apache-2.0), which delegates to the Windows
system verifier. Per ADR-0008 there is **no `webpki-roots`** and no `native-tls`,
so no frozen certificate bundle and no OpenSSL.

`libsqlite3-sys` carries SQLite itself, which is public domain.

## Deliberately absent

* `native-tls` / OpenSSL — ADR-0008 excludes them.
* `webpki-roots` — excluded by ADR-0008; the platform verifier is used instead.
* `slint` — **GPL-3.0-only or commercial**; forbidden outright.
* `tokio-util` — a `CancellationToken` would be convenient, but it is not on the
  approved list, so cancellation uses `tokio::sync::watch` instead.
* `percent-encoding` — decoding is required, but the policy for malformed
  sequences is security relevant, so `util::percent` implements it in-tree with
  its own tests.
* `getrandom` / `rand` — backoff jitter needs only to decorrelate concurrent
  tasks, which a hash of the source URL and attempt number provides.

## Adding a dependency

Requires an ADR entry with licence evidence before it appears here
(`AGENTS.md` §3.6, `.omp/DECISIONS.md` ADR-0005).
