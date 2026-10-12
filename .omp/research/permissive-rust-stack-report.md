# Permissively-licensed Rust crate inventory for a new multi-protocol download manager

**Target:** new, from-scratch, MIT-licensed multi-protocol download manager (HTTP/HTTPS, HTTP/3, BitTorrent, HLS, DASH, FTP/FTPS, SFTP/SCP, Metalink, eD2K, SOCKS/HTTP proxy).

**Evidence sources used, and how they are labelled below:**

| Label | Meaning |
|---|---|
| `[cache]` | Read directly out of `C:\Users\sn1p4\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\<crate>-<ver>\Cargo.toml` (429 crates present). The quoted `license = "..."` line is verbatim from that file. |
| `[git]` | Read directly out of `C:\Users\sn1p4\.cargo\git\checkouts\rqbit-d896eb2b048eb41a\d9e79c9\` (rqbit monorepo, workspace version 9.0.0, rev `d9e79c9`). |
| `[api]` | crates.io registry API (`https://crates.io/api/v1/crates/<name>`), fetched live during this session; the per-version `license` field and `created_at` timestamp are quoted. |
| `[web]` | Public web search result (URL given). |
| `UNVERIFIED` | Could not confirm; stated as unknown rather than guessed. |

**Clean-room note.** Nothing under `E:\dsh-workspace\tdmfast\chua` or `E:\dsh-workspace\tdmfast\tdmfast` was read, listed, grepped, or opened. Every fact below comes from the local cargo cache, the local cargo git checkouts, the crates.io API, or public web sources. The feature list was derived from public protocol standards (BEPs, RFC 8216/MLA, ISO/IEC 23009-1, RFC 959/4217, RFC 5854, the eD2K protocol wiki) plus generic download-manager requirements.

**Method limitation to be explicit about.** `web_fetch` in this environment could only reach a subset of hosts (the platform blocked outbound fetch to most IPs, including `docs.rs`, `raw.githubusercontent.com`, `github.com`, `crates.io`). crates.io data was therefore obtained through the registry JSON API instead, and prose/README/feature questions were answered by web search result snippets only. Where a claim rests on a README I could not open, it is marked as such. Licenses and versions are solid; some *feature-detail* claims are weaker and are flagged individually.

---

## 0. Global license audit of the local cache (context for trust)

All 429 cached crates were audited by reading their `license = "..."` fields `[cache]`:

| License expression seen in cache | Count |
|---|---|
| `MIT OR Apache-2.0` | 208 |
| `MIT` | 92 |
| `Unicode-3.0` | 36 |
| `Apache-2.0 OR MIT` | 23 |
| `MIT/Apache-2.0` | 21 |
| `Apache-2.0` | 5 |
| `Unlicense OR MIT` | 5 |
| `Zlib` | 4 |
| `ISC` | 3 |
| `BSD-3-Clause` | 3 |
| `Apache-2.0 OR ISC OR MIT` | 3 |
| other permissive combinations (Unlicense/MIT, MPL-2.0 ×2, Zlib/BSD/0BSD/CC0/BSL-1.0 variants, `ISC AND (Apache-2.0 OR ISC)`) | ~25 |

**No GPL / AGPL / LGPL / SSPL / BUSL / source-available crate exists in the cache.** The only entries outside the requested allowlist are:

- **36 × `Unicode-3.0`** (the `icu_*`, `zerovec`, `yoke`, `tinystr`, `litemap`, `writeable`, `potential_utf` families). Verbatim license header from `[cache]` `...\icu_provider-2.1.1\LICENSE`: `UNICODE LICENSE V3`. The Unicode License V3 is an OSI-approved permissive license, but it is **not on your stated allowlist** — flagging it rather than assuming. It arrives transitively via `url → idna → idna_adapter → icu_*`, so any dependency on `url` (which is nearly everything: `reqwest`, `librqbit`, `tokio-socks`) pulls it in. You can avoid it only by eschewing `url`, which is not realistic. Recommend an explicit allowlist entry.
- **1 × `webpki-roots` = `CDLA-Permissive-2.0`** — this is a *data* license (Mozilla root certificates), permissive, but again not on your allowlist. See §1.
- **1 × `CC0-1.0 OR MIT-0 OR Apache-2.0`** and **1 × `Apache-2.0 OR BSL-1.0`** — both permissive, fine.
- **2 × `MPL-2.0`** — see §4/§9 (`mp4parse`, `symphonia`); unmodified use is acceptable under your rule.

---

## 1. Async runtime, HTTP/1.1 + HTTP/2 client, connection pooling, TLS, HTTP/3

### 1a. Runtime and HTTP core

| Crate | Current version (date) | License (source) | Maintenance | Does / does not | Production-grade? |
|---|---|---|---|---|---|
| `tokio` | 1.52.3 `[cache]` / latest 1.52.x `[api]` | `license = "MIT"` `[cache]` `tokio-1.52.3\Cargo.toml` | Active; 1.x stable, huge ecosystem | Multi-threaded async runtime, timers, `fs`, `io-util`, signals. Does not include an HTTP stack | Yes — industry default |
| `hyper` | 1.12.0 `[api]` (cache has 1.8.1) | `license = "MIT"` `[cache]` `hyper-1.8.1\Cargo.toml` | Very active (1.12.0 published 2026-10-06 `[api]`) | HTTP/1.1 + HTTP/2 client and server at the protocol layer. No TLS, no pooling, no proxy, no redirect | Yes |
| `hyper-util` | 0.1.21 `[api]` (cache 0.1.20) | `license = "MIT"` `[cache]` `hyper-util-0.1.20\Cargo.toml` | Active (0.1.21, 2026-09-24 `[api]`) | Provides the connection-pooling legacy client. Features read from `[cache]`: `client`, `client-legacy`, **`client-pool`**, **`client-proxy`**, **`client-proxy-system`**, `http1`, `http2`, `tokio`. `client-proxy-system` is OS system-proxy discovery built in (see §8) | Yes — this is what `reqwest` sits on |
| `h2` | 0.4.20 `[api]` (cache 0.4.13) | `license = "MIT"` `[cache]` | Very active (2026-10-06 `[api]`) | HTTP/2 framing, HPACK, flow control | Yes |
| `tower` / `tower-http` | 0.5.3 / 0.6.8 `[cache]` | `MIT` `[cache]` | Active | Middleware layers (retry, timeout, rate-limit, decompression) | Yes |
| `http` / `http-body-util` / `httparse` | 1.4.0 / 0.1.5 / 1.10.1 `[cache]` | `MIT OR Apache-2.0` `[cache]` | Active | Types, body adapters, a fast header parser. `httparse` is useful if you hand-roll a minimal HTTP path | Yes |

### 1b. HTTP client

| Crate | Current version (date) | License (source) | Maintenance | Does / does not | Production-grade? |
|---|---|---|---|---|---|
| `reqwest` | **0.13.5** (2026-09-08) `[api]`; cache holds 0.12.28 and 0.13.4 | `license = "MIT OR Apache-2.0"` `[cache]` `reqwest-0.13.4\Cargo.toml` | Very active | HTTP/1.1 + HTTP/2 + HTTP/3, connection pooling, redirects, cookies, `Range` requests, gzip/brotli/deflate/zstd, SOCKS, streaming bodies, multipart, charset | Yes — the default choice |
| `reqwest` (blocking) | same | same | same | `blocking` feature exists but is a wrapper; do not use it in a tokio app | Yes |
| `ureq` | 3.4.2 (2026-09-13) `[api]` | `MIT OR Apache-2.0` `[api]` | Active | Small **blocking**, rustls-based HTTP/1.1 (+ h2?) client; no HTTP/2 story to rely on. Good for tiny CLI probes, not for a concurrent D/M | Yes, but wrong shape |
| `isahc` | 2.0.1 (2026-07-05) `[api]` | `MIT` `[api]` | Maintained | libcurl binding → brings the **curl license** (MIT-like but with its own notice) and a C build dependency; HTTP/2/3 depends on the system curl. Licensing is permissive but the C dependency is a build/compliance burden in a pure-Rust MIT product. `[web]` crates.io declares MIT; the transitive curl obligation is UNVERIFIED in detail | Usable, not recommended |
| `awc` | 3.8.2 (2026-02-18) `[api]` | `MIT OR Apache-2.0` `[api]` | Active (actix) | Client tied to the actix ecosystem; no advantage here | Yes, wrong ecosystem |

**`reqwest` capability evidence, read from the cached manifest** `[cache]` `reqwest-0.13.4\Cargo.toml`:
- `default = [... "default-tls" ...]`, `default-tls = ["rustls"]` → **rustls is the default TLS backend in 0.13**, not native-tls.
- `http3 = [ ... "dep:h3", "dep:h3-quinn", "dep:quinn", "dep:rustls-platform-verifier" ... ]` → HTTP/3 is a first-class feature, implemented over `quinn`.
- `socks = []` → SOCKS proxy support is a feature flag.
- `rustls`, `native-tls`, `native-tls-vendored`, `rustls-no-provider`, `hickory-dns`, `gzip`, `brotli`, `zstd`, `deflate`, `cookies`, `stream`, `json`, `charset`, `multipart`, `blocking` all exist as features.

### 1c. TLS

| Crate | Current version (date) | License (source) | Maintenance | Notes |
|---|---|---|---|---|
| `rustls` | 0.23.36 stable in cache `[cache]`; 0.24.0-dev.1 is the newest *published* version `[api]` (2026-09-14) | `license = "Apache-2.0 OR ISC OR MIT"` `[cache]` `rustls-0.23.36\Cargo.toml` | Very active | Pure Rust, no OpenSSL. **Pin to 0.23.x stable; 0.24 is a dev prerelease.** |
| `tokio-rustls` | 0.26.6 (2026-09-28) `[api]`; cache 0.26.4 | `MIT OR Apache-2.0` `[cache]` | Active | Async TLS stream over tokio |
| `hyper-rustls` | 0.27.10 (2026-09-20) `[api]`; cache 0.27.7 | `Apache-2.0 OR ISC OR MIT` `[cache]` | Active | hyper ↔ rustls connector |
| `rustls-platform-verifier` | 0.7.1 (2026-09-24) `[api]`; cache 0.7.0 | `MIT OR Apache-2.0` `[cache]` | Active, rustls org | On Windows delegates to the OS verifier (CryptoAPI/WinVerifyTrust path). **This is the crate that matters for "does it honour the Windows certificate store, including enterprise/Chinese roots".** `reqwest` 0.13 depends on it under `rustls` `[cache]` |
| `rustls-native-certs` | 0.8.4 (2026-06-01) `[api]`; cache 0.8.3 | `Apache-2.0 OR ISC OR MIT` `[cache]` | Active | Loads the OS trust store as `rustls` roots. On Windows reads the `ROOT` store — this is how user/enterprise-installed roots are picked up |
| `webpki-roots` | 1.0.9 (2026-07-18) `[api]`; cache 1.0.6 | **`license = "CDLA-Permissive-2.0"`** `[cache]` `webpki-roots-1.0.6\Cargo.toml` | Active | Mozilla's bundled root list. Permissive **data** license, **not on your allowlist** — flag. Also note: a bundled list is the *wrong* choice for a Chinese/proxy environment because it will not contain corporate or local CA roots |
| `native-tls` | 0.2.18 (2026-02-18) `[api]` and `[cache]` | `MIT OR Apache-2.0` `[cache]` | Active | Thin wrapper over SChannel (Windows) / Security.framework / OpenSSL |
| `schannel` | 0.1.29 (2026-03-10) `[api]`; cache 0.1.28 | `MIT` `[cache]` | Active | Windows SChannel binding — full Windows trust-store behaviour by construction |
| `openssl` / `openssl-sys` | 0.10.81 / 0.9.117 (2026-06-12) `[api]` | `Apache-2.0` / `MIT` `[api]` | Active | **Rust binding is Apache-2.0/MIT, but the linked OpenSSL library is under the Apache-2.0-style OpenSSL license** (plus SSLeay for 1.0.x). Fine for MIT, but adds a C build. `[api]` cache list also shows an `ISC AND (Apache-2.0 OR ISC) AND OpenSSL` expression on `aws-lc-sys`'s OpenSSL-compat shim |
| `boring` / `boring-sys` | 5.2.0 (2026-08-11) `[api]` | `Apache-2.0` / `MIT` `[api]` | Active (Cloudflare) | BoringSSL binding. BoringSSL's own license is a mix of OpenSSL + ISC + BSD; **not on your allowlist and it is a large C build.** UNVERIFIED in detail — do not adopt without a license review |
| `ring` | 0.17.14 — **published 2025-03-11** `[api]`; cache 0.17.14 | `license = "Apache-2.0 AND ISC"` `[cache]` `ring-0.17.14\Cargo.toml` | **Effectively dormant — no release in ~19 months** `[api]` | Crypto primitives. Still works, still widely depended on, but `aws-lc-rs` is the actively maintained successor in the rustls world |
| `aws-lc-rs` | 1.18.1 (2026-09-01) `[api]`; cache 1.16.0 | `ISC AND (Apache-2.0 OR ISC)` `[cache]` | Very active (AWS) | Preferred rustls crypto provider. **Build cost:** needs CMake, a C compiler, and NASM on Windows `[web]` (general knowledge, corroborated by the presence of `cmake`, `cc`, `find-msvc-tools`, `jobserver` in the cache) |
| `aws-lc-sys` | 0.45.0 (2026-09-01) `[api]`; cache 0.37.1 | `ISC AND (Apache-2.0 OR ISC) AND Apache-2.0 AND MIT AND BSD-3-Clause AND (Apache-2.0 OR ISC OR MIT) AND (Apache-2.0 OR ISC OR MIT-0)` `[api]` | Very active | Long but fully permissive expression. Note it does **not** list the bare `OpenSSL` term in the version I read; the cached 0.37.1 shows `ISC AND (Apache-2.0 OR ISC) AND OpenSSL` — verify the exact expression for whichever version you lock |

**rustls vs native-tls for Chinese CA / proxy environments — assessment.** All of the following are `[web]`-sourced general findings, not local evidence; treat as engineering judgement, not verified fact:
- The Windows OS trust store is the only place a corporate MITM root, a bank root, or a locally-installed CA lives. `rustls-platform-verifier` exists precisely to delegate to that store, and `reqwest` 0.13 now wires it in by default under the `rustls` feature `[cache]`. That removes the single biggest historical reason to prefer `native-tls` (i.e. "rustls ignores my company's root").
- `rustls-native-certs` reads the Windows `ROOT` store, which also captures user- and machine-installed roots.
- `webpki-roots` (a frozen Mozilla bundle) is the configuration that *does* break in these environments; do not use it as the verifier in a Chinese-market build.
- Residual differences that still favour `native-tls`/`schannel` in edge cases: legacy CA constraints, SHA-1-signed roots, and unusual intermediate-chaining behaviour are handled by SChannel's own policy engine. `[web]` Search results also surfaced multiple real-world bug reports of self-update/HTTPS failures behind MITM firewalls and their fixes being "use the system CA certificates" — e.g. [jdx/mise PR #5459](https://github.com/jdx/mise/pull/5459), [jdx/hk PR #650](https://github.com/jdx/hk/pull/650), [pali PR #143](https://github.com/butterflyskies/pali/pull/143). These corroborate the direction but are third-party anecdotes.
- **Practical recommendation:** rustls 0.23 + `rustls-platform-verifier` (or `native-tls` behind a feature flag) and make the trust source user-selectable in settings ("system store / bundled roots / custom PEM"). Also ship explicit SNI + proxy-CONNECT handling, since Chinese proxy environments frequently do TLS interception that requires the proxy root to be installed system-wide.

### 1d. HTTP/3 / QUIC

| Crate | Current version (date) | License (source) | Maintenance | Notes |
|---|---|---|---|---|
| `quinn` | 0.11.12 (2026-09-14) `[api]`; cache 0.11.9 | `MIT OR Apache-2.0` `[cache]` | Active | QUIC transport. **Permissive — usable.** |
| `quinn-proto` | 0.11.19 (2026-09-30) `[api]`; cache 0.11.14 | `MIT OR Apache-2.0` `[cache]` | Active | Sans-IO QUIC state machine |
| `quinn-udp` | in cache | `MIT OR Apache-2.0` `[cache]` | Active | UDP socket layer |
| `h3` | 0.0.8 — **2025-05-06** `[api]` | `MIT` `[api]` | **Slow — one release in ~17 months, still 0.0.x** | HTTP/3 framing. The API is not stable |
| `h3-quinn` | 0.0.10 — 2025-05-06 `[api]` | `MIT` `[api]` | Same | QUIC transport adapter for `h3` |
| `s2n-quic` | 1.90.0 (2026-10-02) `[api]` | `Apache-2.0` `[api]` | Very active (AWS) | Permissive. Rust implementation with a C crypto dependency (`s2n-tls`/AWS-LC) |
| `neqo-transport` | **not resolvable via the registry path used** | — | — | Mozilla's QUIC stack is not published under that crates.io name; **UNVERIFIED** whether any `neqo-*` crate is publishable/usable. Do not plan on it |

**HTTP/3 verdict:** available and permissively licensed. Reach it through `reqwest` with the `http3` feature (which pulls `h3`/`h3-quinn`/`quinn` `[cache]`) rather than hand-wiring `h3`. Caveat: the `h3` crate is still `0.0.x` with sparse releases, so HTTP/3 should be an **opt-in, per-download fallback**, with HTTP/2 as the default path. Happy-eyeballs-style race between h2 and h3 is application work.

---

## 2. BitTorrent

### 2a. `librqbit` — the main candidate

**Exact license confirmation (this was an explicit ask):** `license = "Apache-2.0"`, verbatim from `[git]` `...\rqbit-d896eb2b048eb41a\d9e79c9\crates\librqbit\Cargo.toml`. Corroborated three ways:
1. Every sibling crate in the same workspace declares `license = "Apache-2.0"` `[git]`.
2. The repo root `LICENSE` is the Apache License 2.0 text, `Copyright 2021 Igor Katson` `[git]`.
3. crates.io per-version license for `librqbit` 9.0.1 is `Apache-2.0` `[api]`.

`librqbit` is **Apache-2.0, not MIT** — Apache-2.0 is on your allowlist and is MIT-compatible for your purposes (you may license your own code MIT while depending on it). Note the one Apache-2.0 obligation that applies: preserve `NOTICE`/attribution if one ships.

| Crate | Version / date | License (source) | Published on crates.io? | Role |
|---|---|---|---|---|
| `librqbit` | 9.0.1 (2026-08-20) `[api]`; workspace 9.0.0 at git rev `d9e79c9` `[git]` | `Apache-2.0` `[git]`+`[api]` | Yes | Session, torrent lifecycle, peer connections, magnet resolution, storage, streaming, HTTP API |
| `librqbit-core` | 9.0.1 (2026-08-20) `[api]` | `Apache-2.0` `[git]`+`[api]` | Yes | `Magnet` parser, `TorrentMetaV1*` metainfo, `Id20`, peer-id generation |
| `librqbit-dht` | 9.0.1 (2026-08-20) `[api]` | `Apache-2.0` `[git]`+`[api]` | Yes | Mainline DHT (BEP 5/32) |
| `librqbit-bencode` | 9.0.1 (2026-08-20) `[api]` | `Apache-2.0` `[git]`+`[api]` | Yes | Serde-based bencode |
| `librqbit-peer-protocol` | 9.0.1 (2026-08-20) `[api]` | `Apache-2.0` `[git]`+`[api]` | Yes | Peer wire message framing |
| `librqbit-tracker-comms` | 9.0.1 (2026-08-20) `[api]` | `Apache-2.0` `[git]`+`[api]` | Yes | HTTP(S) + UDP tracker clients — `SupportedTracker::{Udp,Http}` and `UdpTrackerClient` read from `[git]` `tracker_comms\src\tracker_comms.rs` |
| `librqbit-lsd` | 9.0.1 (2026-08-20) `[api]` | `Apache-2.0` `[git]`+`[api]` | Yes | Local Service Discovery (BEP 14) |
| `librqbit-upnp` | 9.0.1 (2026-08-20) `[api]` | `Apache-2.0` `[git]`+`[api]` | Yes | UPnP port forwarding |
| `librqbit-upnp-serve` | 9.0.1 (2026-08-20) `[api]` | `Apache-2.0` `[git]`+`[api]` | Yes | Serve a torrent over UPnP to LAN devices |
| `librqbit-utp` | 0.7.0 (2026-02-14) `[api]`; cache 0.7.0 `[cache]` | `Apache-2.0` `[cache]`+`[api]` | Yes — **separate repo**, `github.com/ikatson/librqbit-utp` | uTP (BEP 29). Versioned independently (0.7.x, not 9.x) |
| `librqbit-sha1-wrapper` | 9.0.1 (2026-08-20) `[api]` | `Apache-2.0` `[api]` | Yes | SHA-1 abstraction switching between `ring` and `sha1` crates |
| `librqbit-buffers` | 9.0.1 (2026-08-20) `[api]` | `Apache-2.0` `[api]` | Yes | Zero-copy byte buffers |
| `librqbit-clone-to-owned` | 9.0.1 (2026-08-20) `[api]` | `Apache-2.0` `[api]` | Yes | `Cow` helper derive |
| `librqbit-dualstack-sockets` | 0.7.0 (2026-02-10) `[api]` | `Apache-2.0` `[api]` | Yes — separate repo | Dual-stack v4/v6 listener |

**Maintenance:** alive and recently released. 9.0.1 on 2026-08-20 `[api]`, 9.0.0 on 2026-08-15, following a quiet gap after 8.1.1 (2025-06-12) `[api]`. Edition 2024 throughout `[git]`. Downloads: `librqbit` 136,036; the DHT/tracker/peer crates each ~110k–125k `[api]` — real production usage, not a toy.

**Capability matrix — verified against the local source, not the README:**

| Capability | Supported? | Evidence |
|---|---|---|
| Magnet links (`magnet:?xt=urn:btih:…`) | **Yes** | `SUPPORTED_SCHEMES = ["http:", "https:", "magnet:"]`, `Magnet::parse`, BEP 9 metadata fetch, `resolve_magnet` — `[git]` `session.rs` lines 79, 1222–1245, 1767+ |
| Bare 40-hex infohash accepted as a magnet | **Yes** | `[git]` `session.rs` line 355 |
| DHT bootstrap with **no tracker** | **Yes** | `DhtSessionConfig { bootstrap_addrs }`; default routers `DHT_BOOTSTRAP = ["dht.transmissionbt.com:6881", "dht.libtorrent.org:25401"]` — `[git]` `crates\dht\src\lib.rs` line 41, `dht.rs` lines 1279–1321. `disable_trackers: true` is a first-class option `[git]` `session.rs` line 281. Explicit error path text if no peer source exists: *"no known way to resolve peers (no DHT, no trackers, no initial_peers)"* `[git]` `session.rs` line 1383 |
| Manual peer bootstrap (`initial_peers`) | **Yes** | `[git]` `session.rs` line 287 |
| Seeding / uploading | **Yes** | `overwrite` doc: *"needs to be enabled in order to resume/seed the torrent"* `[git]` `session.rs` lines 262–263; `create_and_serve_torrent()` `[git]` `session.rs` line 1815; `POST /torrents/create` "Create a torrent and start seeding" `[git]` `README.md`. A `disable-upload` feature exists for leech-only mode `[git]` `crates\librqbit\Cargo.toml` |
| Selective file download | **Yes** | `only_files: Option<Vec<usize>>` and `only_files_regex: Option<String>` in `AddTorrentOptions`; `update_only_files()` at runtime — `[git]` `session.rs` lines 255–258, 1749. BEP 53 select-only from magnet is parsed too (`magnet.get_select_only()`, line 1228) |
| Torrent creation | **Yes** | `pub use create_torrent_file::{CreateTorrentOptions, CreateTorrentResult, create_torrent}` `[git]` `lib.rs`; `CreateTorrentOptions { name, trackers, piece_length }`, `CreateTorrentResult::as_magnet()` / `.as_info()` / `.info_hash()` / `.as_bytes()` `[git]` `create_torrent_file.rs`. **Caveat:** piece length selection is a hardcoded 2 MiB with a `TODO: make this smarter or smth` `[git]` line 50–53 |
| `list_only` (show files, don't download) | **Yes** | `list_only: bool`, `ListOnlyResponse` `[git]` `session.rs` lines 267–268, 302 |
| Streaming with seek | **Yes** | README "Streaming support … smartly block the stream until the pieces are available … seeking"; `GET /torrents/{id}/stream/{idx}` accepts `Range` `[git]` `README.md`; `src/streaming.rs`, `src/torrent_state/streaming.rs` `[git]` |
| Resume / session persistence / fastresume | **Yes** | `SessionOptions { fastresume, persistence: Option<SessionPersistenceConfig> }`; JSON store by default, Postgres behind a feature — `[git]` `session.rs` lines 435–441; `session_persistence\{json,postgres,inmemory}.rs` `[git]` |
| Private torrents (BEP 27) | **Partial** | README lists BEP-27. But note the code path: *"private trackers are not fully implemented, so using only the first tracker"*, truncating to 1 tracker `[git]` `session.rs` lines 1689–1694 |
| Rate limits | **Yes** | `LimitsConfig` / `ratelimits` `[git]` `limits.rs`; `governor` + `leaky-bucket` in the workspace deps `[git]` |
| MSE / peer encryption | **Yes** | `MseMode` exported `[git]` `lib.rs`; `src/mse\` module with `dh768`, `rc4`, `stream` `[git]` |
| Storage backends | **Yes** | `storage\` — in-memory, mmap, fs, sparse, write-through cache; pluggable `BoxStorageFactory` `[git]` |
| Windows-specific I/O | **Yes** | `[target.'cfg(windows)'.dependencies] windows = { features = ["Win32","Win32_System","Win32_System_IO","Win32_System_Ioctl"] }` — i.e. sparse-file/`FSCTL_SET_SPARSE` support `[git]` `crates\librqbit\Cargo.toml` |
| BitTorrent v2 / BEP 52 | **Partial** | SHA-256 trait and BEP 52 error variants and `hash_id.rs` truncation comments exist `[git]` `error.rs`, `lib.rs`, `hash_id.rs`. Treat v2 as **incomplete** — UNVERIFIED how far it goes |
| **Web seeds (BEP 19 `url-list` / BEP 17)** | **NO** | `grep` for `web_seed`, `http_seed`, `url_list`, `BEP 19` across `crates\librqbit\src` returned **no matches** `[git]`. This is a real gap you must write yourself |
| UDP tracker (BEP 15) | **Yes** | `tracker_comms_udp.rs`, `UdpTrackerClient` `[git]` |
| IPv6 (BEP 7 / BEP 32) | **Yes** | README BEP list; `librqbit-dualstack-sockets` `[git]` |
| PEX (BEP 11), LSD (BEP 14), uTP (BEP 29), padding (BEP 47) | **Yes** | README BEP list `[git]`; `librqbit_lsd`, `librqbit-utp` crates `[git]` |
| Proxy support inside BT | **Indirect** | `reqwest` is used with the `socks` feature `[git]` `crates\librqbit\Cargo.toml` → tracker HTTP and `.torrent` fetches can go through SOCKS. **Peer connections over SOCKS is UNVERIFIED** and is likely *not* supported; assume you must add it |

**What `librqbit` does not do:** no web seeds, no HTTP download manager (no ranged multi-connection HTTP), no HLS/DASH, no proxy for the peer wire, private-tracker multi-tracker behaviour is stubbed, piece-length heuristic is a TODO, BT v2 is partial. It also ships an opinionated HTTP API + optional web UI feature set you would ignore.

**Toy or production?** Production. It is the engine behind the released `rqbit` desktop/CLI client, with 136k downloads, an edition-2024 codebase, tests including e2e peer tests, and the `desktop/src-tauri` app in the same repo `[git]`.

### 2b. Alternatives and adjacent crates

| Crate | Version (date) | License (source) | Status | Assessment |
|---|---|---|---|---|
| `mainline` | 8.0.1 (2026-09-30) `[api]` | `MIT` `[api]` | **Actively maintained**, 528,568 downloads `[api]` | Standalone Mainline DHT. MIT (more permissive than `librqbit-dht`'s Apache-2.0). Useful if you want DHT decoupled from the BT engine, or as a fallback/validation implementation |
| `dht` | 7.0.0 (2026-08-05) `[api]` | `MIT` `[api]` | Published from `github.com/nuhvi/mainline` — a fork of the same project | Same codebase as `mainline`; prefer `mainline` (canonical repo, 528k vs 2.7k downloads) |
| `torrent` | 0.1.10 (2026-07-20) `[api]` | `MIT` `[api]` | **Very new and tiny — 278 downloads, 11 releases since 2026-06-13** `[api]` | "High-level async BitTorrent library — session management, HTTP/UDP tracker, DHT, peer connections, file storage" from `github.com/mogeko/torrent-rs`. MIT is nicer than Apache-2.0, but the download count and age make this a **toy/beta** for planning purposes. Not a candidate for a shipping product |
| `torrent-core` | 0.1.10 (2026-07-20) `[api]` | `MIT` `[api]` | 377 downloads `[api]` | Low-level half of the same project (bencode, metainfo, peer wire, DHT, piece mgmt). Same toy verdict |
| `cratetorrent` | 0.1.0 — **2020-12-22** `[api]` | `MIT OR Apache-2.0` `[api]` | **Abandoned** (2,436 downloads, single release ~6 years ago) | Historically interesting, unmaintained. Exclude |
| `bittorrent` | 0.2.0 — 2017-06-25 `[api]` | `MIT/Apache-2.0` `[api]` | **Abandoned** | Do not use |
| `bip_metainfo` | 0.12.0 — **2017-11-15** `[api]` | `MIT/Apache-2.0` `[api]` | **Abandoned** | See §3 |
| `bip_bencode` | 0.4.4 — 2017-11-17 `[api]` | `MIT/Apache-2.0` `[api]` | **Abandoned** | See §3 |
| `bip_dht` | 0.6.0 — 2017-07-15 `[api]` | `MIT/Apache-2.0` `[api]` | **Abandoned** | Do not use |
| `bip_handshake` / `bip_peer` | 0.7.1 / 0.5.0 — 2017 `[api]` | `MIT/Apache-2.0` `[api]` | **Abandoned** | Do not use |
| `kad` | 0.6.1 — **2021-08-06** `[api]` | `MIT / Apache-2.0` `[api]` | **Effectively abandoned** (15,272 downloads) | Generic Kademlia DHT, not BitTorrent-Mainline-specific. Could inform an eD2K Kad design but is not a drop-in — and it is stale |
| `igd-next` | 0.18.0 (2026-10-04) `[api]`; cache 0.17.1 | `MIT` `[cache]` | Active, 16.4M downloads `[api]` | UPnP IGD client. Alternative/complement to `librqbit-upnp` |
| `portmapper` | 0.19.3 (2026-08-25) `[api]` | License from `[api]`: UNVERIFIED in this session (published from `github.com/n0-computer/net-tools`) | Active, 2.86M downloads `[api]` | UPnP + NAT-PMP + PCP abstraction. **Verify the license before adopting** — do not assume |
| `natpmp` | 0.5.0 — **2024-11-06** `[api]` | `[api]` not queried; UNVERIFIED | Low activity | NAT-PMP only |
| `librqbit-utp` | 0.7.0 (2026-02-14) `[api]`, cached `[cache]` | `Apache-2.0` `[cache]`+`[api]` | Active | Standalone uTP; usable even if you do not take `librqbit` |

**Copyleft check for BitTorrent:** none found. Every BT crate I could resolve is MIT, Apache-2.0, or MIT/Apache-2.0. No GPL/LGPL/source-available BT crate appeared in any search `[web]`.

---

## 3. Magnet / BT metadata and `.torrent` parsing (bencode)

| Crate | Version (date) | License (source) | Maintenance | Does / does not | Verdict |
|---|---|---|---|---|---|
| `bendy` | 0.6.1 (2025-11-10) `[api]` | **`BSD-3-Clause`** `[api]` | **Active** — a burst of releases through 2025 (0.4.0-beta.3 → 0.4.0 → 0.5.0 → 0.6.0 → 0.6.1) after a long dormancy since 2018, 908,581 downloads `[api]` | Encode/decode bencode **with enforced canonicalization rules** — important for a *correct* infohash, because the infohash is the SHA-1 of the canonical re-encoding of the `info` dictionary. Serde-compatible. Does not do the BT layer itself | **Recommended** for generic bencode work; BSD-3-Clause is cleaner than Apache-2.0 for you |
| `librqbit-bencode` | 9.0.1 (2026-08-20) `[api]`; source `[git]` | `Apache-2.0` `[git]`+`[api]` | Active with the rqbit release train | "Bencode serialization and deserialization using Serde". Carries `WithRawBytes` so the exact original `info` bytes survive — that is what `create_torrent_file.rs` hashes `[git]`. Does not do canonicalization enforcement the way `bendy` does | Best choice **if you take `librqbit`**, since the metainfo types are shared and you avoid a second bencode implementation |
| `serde_bencode` | 0.2.4 — **2023-10-09** `[api]` | `MIT` `[api]` | Low activity (1.08M downloads) | Serde bencode. Works, but stale and no canonicalization focus | Acceptable fallback |
| `bencode` | 0.1.16 — **2017-04-01** `[api]` | `MIT/Apache-2.0` `[api]` | **Abandoned ~9 years** | Original crate | Do not use |
| `bip_bencode` | 0.4.4 — 2017-11-17 `[api]` | `MIT/Apache-2.0` `[api]` | **Abandoned** | — | Do not use |
| `bip_metainfo` | 0.12.0 — 2017-11-15 `[api]` | `MIT/Apache-2.0` `[api]` | **Abandoned** | `.torrent` metainfo parsing, once the canonical choice | Do not use — 9 years stale, no v2 support |
| `librqbit-core` (`Magnet` / `TorrentMetaV1*`) | 9.0.1 `[api]` | `Apache-2.0` `[git]` | Active | The magnet URI parser and the v1 metainfo types. `Magnet::parse`, `get_select_only()` (BEP 53), `Magnet { info_hash, trackers, name }` `[git]` `session.rs` 1222–1245; `torrent_metainfo::TorrentMetaV1Info/File/Owned` `[git]`. `hash_id.rs` has BEP 52 truncation logic `[git]` | Take this if you take `librqbit` |

**What you must write regardless:** `.torrent` **v2-only** and **hybrid** torrent handling beyond what `librqbit` exposes is partial `[git]`; canonicalization edge cases (unsorted dict keys, duplicate keys, non-UTF8 byte strings, `path` traversal sanitisation) are your responsibility — `bendy`'s canonicalization helps but path-safety is yours.

---

## 4. HLS and DASH parsing, AES-128 decryption, remuxing

### 4a. HLS

| Crate | Version (date) | License (source) | Maintenance | Does / does not |
|---|---|---|---|---|
| **`m3u8-rs`** | 6.0.1 (2026-07-30) `[api]`; cache 6.0.0 `[cache]` | `license = "MIT"` `[cache]` `m3u8-rs-6.0.0\Cargo.toml` | **Active** — 1.65M downloads `[api]`; was 6.0.0 on 2026-07-30's predecessor, and the repo (`github.com/rutgersc/m3u8-rs`) `[api]` has been going since 2016 | **Recommended.** Parse *and serialize* both playlist kinds. Verified from the cached source `[cache]`: `MediaPlaylist` (`playlist.rs` line 718), `MasterPlaylist` (line 174) with `alternatives: Vec<AlternativeMedia>` for `EXT-X-MEDIA` (line 181), `MediaSegment { uri, …, key: Option<Key>, map: Option<Map>, … }` (lines 830–843), `Key { method: KeyMethod, uri: Option<String>, iv: Option<String>, keyformat: Option<String>, keyformatversions: Option<String> }` (`EXT-X-KEY`, lines 953–967), `Map { uri }` (`EXT-X-MAP`, lines 1003–1013), `SessionKey` (`EXT-X-SESSION-KEY`, line 626), and a `ByteRange { length, .. }` writer for `#EXT-X-BYTERANGE` (line 1036). Dependencies are only `chrono` + optional `nom` `[cache]` — **no crypto, no HTTP** |
| `hls_m3u8` | 0.7.0 (2026-05-06) `[api]` | `MIT OR Apache-2.0` `[api]` | Moderate (325k downloads) | Alternative parser from `github.com/sile/hls_m3u8`. Reportedly very complete on tags but a heavier, more opinionated API; last release 2026-05 `[api]` |
| `quick-m3u8` | 0.8.0 (2026-02-17) `[api]` | `Unlicense` `[api]` | Low (10k downloads), repo `github.com/theRealRobG/m3u8` | Permissive (Unlicense is on your list) but low adoption |
| `m3u8` | 0.1.0 — **2015-06-22** `[api]` | **blank license field** `[api]` | **Abandoned, license unclear** | **Do not use.** A missing license is worse than a bad one |

**What no HLS crate does:** none of them fetch segments, none decrypt AES-128, none handle live-playlist reload, none do byte-range arithmetic into a shared file, none remux. `m3u8-rs` stops at "here is the parsed structure".

### 4b. DASH

| Crate | Version (date) | License (source) | Maintenance | Does / does not |
|---|---|---|---|---|
| **`dash-mpd`** | 0.21.0 (2026-10-04) `[api]`; cache 0.19.3 `[cache]` | `license = "MIT"` `[cache]` `dash-mpd-0.19.3\Cargo.toml` | **Very active** — 431,980 downloads `[api]`, releases through 2026-10 `[api]`, repo `github.com/emarsden/dash-mpd-rs` `[api]` | **Recommended, and better than the HLS story.** The cached manifest's own description reads: *"Parse, serialize, download an MPD manifest for MPEG-DASH or WebM-DASH media streaming"* `[cache]` — so it goes **beyond parsing**: it can fetch segments too. Depends on `quick-xml`, `chrono`, `regex`, `serde`, `reqwest` (`[cache]` manifest shows `quick-xml` 0.39/0.41, `reqwest`, `isocountry`-style deps). Supports MPD, SegmentTemplate/SegmentTimeline/SegmentList, multiple periods, BaseURL, `ContentProtection` DRM detection (UNVERIFIED at field level) |
| `dash-mpd-cli` | companion binary in the same repo `[api]` | `MIT` | Active | Useful as a **behavioural reference** for what the library can do — not as a dependency |

**Note on naming:** `dash-mpd` and `dash-mpd-rs` are the same project — `dash-mpd` is the crates.io package name, `dash-mpd-rs` is the repository name `[api]`. There is no separate competing crate of note.

### 4c. AES-128 decryption (HLS `EXT-X-KEY:METHOD=AES-128`)

| Crate | Version (date) | License (source) | Verdict |
|---|---|---|---|
| `aes` | 0.9.3 (2026-08-28) `[api]`; cache 0.8.4 `[cache]` | `MIT OR Apache-2.0` `[cache]` | RustCrypto AES block cipher. Permissive, audited family, production-grade. **Recommended** |
| `cbc` | 0.2.1 (2026-05-20) `[api]`; cache 0.1.2 `[cache]` | `MIT OR Apache-2.0` `[cache]` | CBC mode — this is what HLS AES-128 uses. Combine with a PKCS#7 pad from `block-padding` (cached) |
| `ctr` | 0.10.1 (2026-05-20) `[api]` | `MIT OR Apache-2.0` `[api]` | For SAMPLE-AES/CTR variants if ever needed |
| `cipher` / `digest` / `block-padding` | cached `[cache]` | `MIT OR Apache-2.0` `[cache]` | Supporting RustCrypto traits |

**What you write yourself:** downloading the key URI, the `IV` defaulting rule (IV = segment media sequence number when `EXT-X-KEY` omits `IV`), PKCS#7 unpadding, and per-segment sequencing. This is small — a few hundred lines — and all the primitives are permissive.

### 4d. MPEG-TS / fMP4 / remuxing

| Crate | Version (date) | License (source) | Maintenance | Does / does not |
|---|---|---|---|---|
| **`ts2mp4`** | 0.4.0 (2026-09-11) `[api]`; cache 0.3.1 `[cache]` | `license = "MIT"` `[cache]` `ts2mp4-0.3.1\Cargo.toml` | **New and active** — first release 2026-01-14, 5 releases to 0.4.0 `[api]`; repo `github.com/aciddust/ts2mp4` `[api]` | *"A library and CLI tool for converting MPEG-TS files to MP4 format"* `[api]`. **This is the single most relevant crate for "join split HLS segments into one MP4".** Caveat: 2,703 total downloads `[api]` — **young, unproven, treat as "promising but not battle-tested"** |
| `mpeg2ts-reader` | 0.18.2 — **2025-01-29** `[api]` | `MIT/Apache-2.0` `[api]` | Slowing (111,980 downloads) | Full ISO/IEC 13818-1 (MPEG-2 TS) demux: PAT/PMT/PES, descriptors. Repo `github.com/dholroyd/mpeg2ts-reader`. Good for *reading*; does not mux to MP4 |
| `mpeg2ts` | 0.6.1 (2026-08-11) `[api]` | `MIT` `[api]` | Active (114,846 downloads), repo `github.com/sile/mpeg2ts` | TS packet/PES handling, same author as `hls_m3u8` |
| `mp4` | 0.14.0 — **2023-08-01** `[api]` | `MIT` `[api]` | **Stale — ~3 years** (14.2M downloads) | MP4 box read/write. Widely used but no recent releases `[api]` |
| `mp4parse` | 0.17.0 — 2023-05-29 `[api]` | **`MPL-2.0`** `[api]` | Stale (2.3M downloads) | Firefox's parser. **MPL-2.0 — acceptable only if unmodified**; it is a read-only parser anyway |
| `mp4san` | 0.5.4 (2026-09-17) `[api]` | `MIT` `[api]` | Active (209,600 downloads) | MP4 *sanitizer* (strip/harden). Useful for validating untrusted MP4 output, not for muxing |
| `webm-iterable` / `ebml-iterable` | cached `[cache]` | permissive `[cache]` | — | EBML/WebM iteration, if you need WebM-DASH output |
| `isobmff` | **not found on crates.io** `[api]` | — | — | Does not exist under that name |
| `symphonia` | 0.6.1 (2026-08-13) `[api]` | **`MPL-2.0`** `[api]` | Active (16.8M downloads) | **Decode only** — audio (and demux for many containers). It will not mux TS→MP4. MPL-2.0, acceptable only unmodified |

### 4e. ffmpeg — the licensing crux

Three distinct options, with materially different obligations:

**(a) Link `libav*` as LGPL.** The Rust crates that do this, and their own licenses `[api]`:

| Crate | Version (date) | Crate license `[api]` | Maintenance | Note |
|---|---|---|---|---|
| `ffmpeg-next` | 9.0.0 (2026-08-05) `[api]`; 9.0.0 on 2026-08 | **`WTFPL`** `[api]` | Active (7.8M downloads) | The classic binding, repo `github.com/zmwangx/rust-ffmpeg` |
| `ffmpeg-sys-next` | 9.0.1 (2026-10-09) `[api]` | **`WTFPL`** `[api]` | Active | The `-sys` half; links `libavcodec`/`libavformat`/`libavutil`/`libswscale`/`libswresample` |
| `ffmpeg-the-third` | 6.0.0+ffmpeg-9.0 (2026-08-09) `[api]` | `WTFPL` `[api]` | Maintained | Fork of the above |
| `rsmpeg` | 0.18.0+ffmpeg.8.0 — 2025-08-24 `[api]` | **`MIT`** `[api]` | Slowing (297k downloads), by Lark | Requires Rust nightly |
| `ac-ffmpeg` | 0.19.0 — 2025-05-16 `[api]` | `MIT` `[api]` | Low activity (55k downloads) | Narrower, well-tested for decode/encode |
| `ez-ffmpeg` | 0.19.3 (2026-10-06) `[api]` | `MIT OR Apache-2.0 OR MPL-2.0` `[api]` | Newer | Repo `github.com/YeautyYE/ez-ffmpeg` |

The **crate** license is not the question. `WTFPL` is permissive (a permissive-license reviewer may still dislike it for its wording — flag it), and `MIT` on `rsmpeg`/`ac-ffmpeg` is clean. The question is the **linked library**:
- FFmpeg's own default license is **LGPL-2.1-or-later**. LGPL is **on your disqualified list**. It is usable by a proprietary/MIT application *only* under the LGPL's dynamic-linking exception, which carries real obligations: dynamic linking to a shared `libav*.dll` the user can replace, and providing the means to relink (typically satisfied by shipping the unmodified FFmpeg DLLs plus build info). `[web]` See [FFmpeg's own libav-user thread on static linking and the relink requirement](https://ffmpeg.org/pipermail/libav-user/2018-February/010960.html) and a general [LGPL linking-rules explainer](https://safeguard.sh/resources/blog/what-is-the-lgpl-license).
- If the build is configured `--enable-gpl` (which is what you need for `libx264`/`libx265`), the result is **GPL** and **linking it makes your MIT application GPL**. **Not acceptable.**
- Non-free/`--enable-nonfree` components are worse still.

**(b) Invoke an external `ffmpeg.exe` as a subprocess.** This is a *fundamentally different* licensing question: you are not creating a derivative work of FFmpeg, you are distributing (or merely requiring) a separate program the user runs. Under the standard reading, an MIT application that shells out to a user-supplied or separately-downloaded FFmpeg binary does **not** become GPL/LGPL. Obligations attach to how you distribute that binary, not to your code. This is the option that keeps your MIT codebase clean.
- Practical costs: process spawn latency, no in-process error types, progress parsing from stderr, argument-injection care, and the need to locate/validate/version-check the binary.

**(c) Pure-Rust remux with no FFmpeg at all.** `ts2mp4` + `mpeg2ts-reader` + `mp4` can cover TS→MP4 for the common H.264/AAC case, at the cost of codec edge-case coverage (H.265, AC-3/E-AC-3, odd PIDs, PCR discontinuities, encrypted SAMPLE-AES). `[web]` Search corroborated that FFmpeg compliance problems in commercial builds revolve precisely around the GPL/LGPL configuration choice ([hoop.dev writeup](https://hoop.dev/blog/ffmpeg-licensing-compliance-avoiding-legal-pitfalls-in-your-build-process)).

**Recommendation:** ship an MIT binary with **(c) as the built-in path** and **(b) as the optional "use my ffmpeg" enhancement**, user-configured. Do **not** link `libav*`, even the LGPL build, unless you deliberately accept LGPL dynamic-linking obligations — and if you do, it must be the *unmodified LGPL shared build* with relinkability preserved, never a GPL build.

---

## 5. FTP / FTPS and SFTP / SCP

### 5a. FTP / FTPS

| Crate | Version (date) | License (source) | Maintenance | Does / does not |
|---|---|---|---|---|
| **`suppaftp`** | **12.2.0 (2026-10-11)** `[api]`; cache 6.3.0 `[cache]` | `license = "MIT OR Apache-2.0"` `[cache]` `suppaftp-6.3.0\Cargo.toml` | **Very active** — 4.86M downloads, released 2026-10-11 `[api]`, repo `github.com/veeso/suppaftp` `[cache]` | **Recommended.** Sync + async, FTPS via **either** `native-tls` **or** `rustls` (features `native-tls`, `native-tls-vendored`, `rustls`, `async-native-tls`, `async-rustls`, `async-secure`, `secure` — read from `[cache]`). Verified API from cached source `[cache]`: `retr_as_stream` (mod.rs 463), `finalize_retr_stream` (471), **`resume_transfer(offset)`** (569) → resumable downloads, `list` (594), **`mlsd`** (623), `size` (688), `cwd` (311), `into_secure` (155), `put_with_stream` (512). Repo has grown from 6.3.0 in cache to 12.2.0 — fast-moving but a long-lived project |
| `ftp` | 3.0.1 — **2018-04-15** `[api]` | `Apache-2.0/MIT` `[api]` | **Abandoned ~8 years** | Do not use |
| `async_ftp` (note: `async_ftp`, not `async-ftp`) | 6.0.0 — **2021-11-04** `[api]` | `Apache-2.0/MIT` `[api]` | **Abandoned ~5 years** | Do not use |
| `libunftp` | server side | — | — | Irrelevant — it is an FTP **server** |

**FTPS has a caveat worth stating:** implicit FTPS (port 990) and explicit FTPS (AUTH TLS) differ, and `suppaftp`'s async + TLS combinations are the part most likely to have bugs in the wild. Test against real servers. **Choose the `rustls` feature** for consistency with §1c.

### 5b. SFTP / SCP

| Crate | Version (date) | License (source) | Maintenance | Assessment |
|---|---|---|---|---|
| **`russh`** | 0.64.1 (2026-10-05) `[api]` | **`Apache-2.0`** `[api]` | **Very active** — 8.2M downloads, releases through 2026-10 `[api]` | **Recommended.** Pure-Rust SSH client *and* server; no C, no OpenSSL, no libssh2. License Apache-2.0 (on your list) |
| **`russh-sftp`** | 3.0.1 (2026-09-28) `[api]` | `Apache-2.0` `[api]` | Active — 3.67M downloads, repo `github.com/AspectUnk/russh-sftp` `[api]` | **Recommended.** Async SFTP client and server on top of `russh`. This is your SFTP implementation |
| `openssh-sftp-client` | 0.15.10 (2026-10-01) `[api]` | `MIT` `[api]` | Active — 2.58M downloads, repo `github.com/openssh-rust/openssh-sftp-client` `[api]` | Alternative/complement. Reportedly **drives the `ssh`/`sftp` binary as a subprocess** rather than implementing the protocol (UNVERIFIED but strongly implied by the name and project framing). If so: permissive license, no linkage issue, but requires an OpenSSH client on the user's machine. Useful as a fallback, not as the primary |
| `ssh2` | 0.9.6 (2026-06-30) `[api]` | `MIT OR Apache-2.0` `[api]` | Maintained — 8.19M downloads | **libssh2 binding.** Crate license is permissive. The vendored C library **libssh2 is BSD-3-Clause** `[web]` (this is well-established: libssh2's own `COPYING` is a 3-clause BSD). So **yes, the libssh2 binding is acceptable in an MIT codebase** — BSD-3-Clause is on your allowlist and imposes only notice obligations. **Caveats:** (i) `libssh2-sys` 0.3.3 (2026-09-05) `[api]`, license `MIT OR Apache-2.0` `[api]`, may link OpenSSL or mbedTLS depending on features — check which, and prefer the mbedTLS/WinCNG path to avoid an OpenSSL dependency; (ii) it is a C build; (iii) it is a blocking, non-tokio-friendly API requiring `spawn_blocking` |
| `libssh2-sys` | 0.3.3 (2026-09-05) `[api]` | `MIT OR Apache-2.0` `[api]` | Active | The `-sys` crate. **The BSD-3-Clause term comes from the vendored libssh2, not from this manifest** — the crate license field does not disclose it. Flagging because it is exactly the kind of hidden obligation worth documenting |
| `thrussh` | 0.49.0 (2026-08-28) `[api]` | `Apache-2.0` `[api]` | 333k downloads | The **ancestor** of `russh`. Still releasing, but `russh` is the maintained lineage. Prefer `russh` |
| `libssh-rs` | 0.3.8 (2026-07-05) `[api]` | `MIT` `[api]` | 315k downloads | Binding to **libssh** (a *different* C library from libssh2). libssh's license is LGPL-2.1 **or** BSD-2-Clause depending on build — UNVERIFIED which, and the LGPL branch would be disqualified. **Avoid unless you verify a BSD-2-Clause build** |

**SFTP/SCP verdict:** take `russh` + `russh-sftp` (both Apache-2.0, pure Rust). `ssh2`/libssh2 is **acceptable** (BSD-3-Clause) if you need its maturity or SCP semantics, but it adds a C dependency for no licensing benefit over `russh`. **SCP specifically:** `russh` implements SSH but SCP is a separate legacy protocol on top; assuming `russh-sftp` gives you SCP would be wrong — **SCP support is UNVERIFIED in `russh` and likely needs implementing or shelling out**.

---

## 6. Metalink and multi-source / mirror aggregation

| Crate | Version (date) | License (source) | Maintenance | Assessment |
|---|---|---|---|---|
| `metalink` | 0.1.0 — **2021-02-01** `[api]` | `MIT` `[api]` | **Abandoned — one release ever, ~5 years ago**, 13,422 downloads `[api]`; repo `github.com/OpenByteDev/metalink` `[api]` | The **only** Rust Metalink crate that resolves. Description: *"A Metalink 4.0 parser"* `[api]`. Parser-only: no fetching, no mirror selection, no hash verification, no Metalink/HTTP (`Link` header) support. Single-version 0.1.0 says this was never finished. **Treat as a reference, not a dependency** |
| `metalink-rs` | **not found** `[api]` | — | — | Does not exist |
| `libmetalink` bindings | **not found on crates.io** `[api]` | — | — | No Rust binding exists. (The C library `libmetalink` itself is permissively licensed — Debian's RFP notes it as a small C parser; its own license is UNVERIFIED in this session `[web]`) |

**Verdict: Metalink is application code.** Even the one crate that exists is an unfinished parser, and a Metalink implementation that matters is 90% *policy* (mirror ranking, parallel multi-source chunking, hash verification, `metaurl`/Metalink/HTTP discovery). Write it yourself.

**Authoritative specs to implement from:**
- **RFC 5854** — *The Metalink Download Description Format*: <https://datatracker.ietf.org/doc/html/rfc5854>
- **Metalink/HTTP (`Link` header with `rel=describedby`, `duplicate`, `mirror`, `metaurl`)**: <https://datatracker.ietf.org/doc/html/draft-bryan-metalinkhttp>
- **IANA media type** `application/metalink4+xml` (`application/metalink+xml` for v3): <https://www.iana.org/assignments/media-types/application/metalink4+xml>
- **Reference implementation behaviour to match:** aria2's Metalink handling — <https://aria2.github.io/manual/en/html/README.html> (aria2 is GPL-2.0+; **read the spec, not the code**)

**Multi-source/mirror aggregation:** there is **no crate for this at all**. Segment scheduling, per-mirror speed scoring, source failover, and range-coalescing into one file are pure application logic. The building blocks (see §12) are `Range` header support, `bytes`/`memmap2` for the output file, and an interval structure to track which byte ranges are owned/in-flight/done.

---

## 7. eD2K / eMule

**Search result: there is no permissively-licensed Rust eD2K client library. This is a self-implement-from-spec item.**

| Candidate | Version (date) | License (source) | What it actually is |
|---|---|---|---|
| `ed2k` | 1.0.1 — **2023-05-19** `[api]` | **`MIT OR Apache-2.0`** `[api]` (not present in the local cache — `[api]` only) | **Only the hash function** — crates.io description: *"An implementation of the ED2K hash function"* `[api]`, repo `github.com/Kimundi/ed2k` `[api]`. It is **not** a client, not a network stack. Two releases, both 2023-05-19, 4,727 downloads `[api]` → **low-adoption, effectively unmaintained, but the license is clean and the algorithm is small enough to verify yourself** |
| `kad` | 0.6.1 — 2021-08-06 `[api]` | `MIT / Apache-2.0` `[api]` | A **generic** Kademlia DHT, not eD2K's Kad. Possibly a design reference for the Kad network. Abandoned |
| `emule`, `edonkey` | **not found on crates.io** `[api]` | — | Do not exist |
| **`emulebb-rust`** | 0.1.0-beta.1 `[web]` | **Almost certainly GPL-derived — DO NOT USE** | Found via web search: a Rust eD2K client with an MFC-eMule parity track — [eMuleBB Rust release notes](https://emulebb.github.io/emulebb-tooling/products/emulebb-rust/RELEASE-0.1.0-beta.1-NOTES/), [architecture doc](https://emulebb.github.io/emulebb-tooling/products/emulebb-rust/design/architecture/), [parity review](https://emulebb.github.io/emulebb-tooling/products/emulebb-rust/active/PARITY-REVIEW-2026-07-05/). It is an **application, not a crates.io library**, and its explicit goal is parity with eMule (which is **GPL-2.0**). **I could not verify its LICENSE file** (github.com was unreachable from this environment), so I am not asserting the license — but on the evidence available it must be treated as **copyleft-contaminated and out of bounds**, including as a code reference. Do not read its source |

**Other implementations to be aware of as licensing hazards, not resources:** eMule (GPL-2.0), aMule (GPL-2.0), eDonkey2000 (proprietary, defunct), MLDonkey (GPL-2.0). **Their source is off-limits for this clean-room build**; only their *protocol documentation* is usable, and even then you should implement from the specification documents rather than from any implementation.

### Authoritative public protocol documentation to implement from

| Topic | Source |
|---|---|
| eD2K protocol overview (server↔client, client↔client, messages) | amule wiki, *Ed2k protocol*: <https://wiki.amule.org/wiki/Ed2k_protocol> |
| eD2K link format (`ed2k://\|file\|name\|size\|hash\|/`, `\|sources\|`, `\|p\|`, `\|h\|` for AICH) | amule wiki, *Ed2k link*: <https://wiki.amule.org/wiki/Ed2k_link> |
| IANA URI scheme registration for `ed2k` | <https://www.iana.org/assignments/uri-schemes/prov/ed2k> |
| Formal protocol specification | *The eMule Protocol Specification*, Yoram Kulbak & Danny Bickson, Hebrew University (2005) — the canonical academic description of the eMule/eD2K protocol; copies circulate as `protocol_guide.pdf` (e.g. via the codeplex archive mirror: <http://codeplexarchive.org/browse/ZeraldotNet/releases/3/protocol_guide.pdf>) |
| AICH (Advanced Intelligent Corruption Handling) | amule wiki, *AICH*: <https://wiki.amule.org/wiki/AICH> |
| Historical/architectural background | *eDonkey* peer-to-peer overview, HU Berlin: <https://waste.informatik.hu-berlin.de/Lehre/vorherige/ws05-06/SE_p2p/eDonkey.pdf> |

**Algorithm you must implement (the parts that are easy to get wrong):**
- **eD2K hash (the ed2khash / "root hash"):** the file is split into blocks of **9,728,000 bytes** (9500 KiB). Each block is MD4-hashed. If the file is **≤ 9,728,000 bytes**, the eD2K hash **is** the MD4 of the whole file (no tree). Otherwise, the MD4 digests of the blocks are concatenated and MD4-hashed again; **if that concatenation is itself larger than 9,728,000 bytes, the process recurses** (a Merkle tree over MD4 digests with the same block size at every level). The result is the 16-byte ed2k hash shown in `ed2k://` links. Note the well-known subtlety that a file of *exactly* one block does not get a second MD4 pass.
- **AICH hash:** a **SHA-1**-based tree over smaller parts (the part size is derived from the file size, ~180 KiB–9,728,000 B), yielding a 20-byte root used in the `h=` parameter of ed2k links and in the AICH recovery protocol.
- **Partfile format:** the on-disk `.part` layout plus the `.part.met` metadata file (a versioned binary record containing the hash, block bitmap, sources, and AICH hashset). This is what makes resumable eD2K downloads work.
- **Kad:** eD2K's Kademlia variant (128-bit node IDs, 4-bit buckets / 32 buckets per routing zone, `K=10`, `α=3`) — related to, but **not wire-compatible with**, Mainline DHT. Do not assume `mainline` or `librqbit-dht` can be reused; `kad` 0.6.1 is generic and abandoned.

**Rust crates you *can* use for the crypto:** `md4` 0.11.0 (2026-03-27) `[api]` / cached 0.10.2 `[cache]`, `MIT OR Apache-2.0` `[cache]`; `sha1` 0.11.0 (2026-07-10) `[api]` / cached `[cache]`, `MIT OR Apache-2.0` `[cache]`; `digest` trait `[cache]`, `MIT OR Apache-2.0`. `crypto-hash` 0.3.4 (2019-08-18) `[api]`, `MIT` `[cache]` — convenience wrapper, stale but fine. **Do not take a dependency on `ed2k` 1.0.1 blindly** — the algorithm is ~100 lines on top of `md4`, and writing it yourself (verified against published test vectors) removes both the staleness risk and any residual licensing doubt.

**Effort estimate:** eD2K is by far the largest self-build item in this inventory — server protocol, source exchange, queue/credit system (the "rating"/credit formula), Kad, AICH recovery, partfile persistence, and the compressed/encrypted datagram transport (the eMule obfuscation protocol, which is *not* the same as BitTorrent MSE) all have to be written.

---

## 8. HTTP proxy / SOCKS5, PAC files, Windows system proxy discovery

### 8a. SOCKS and HTTP CONNECT

| Crate | Version (date) | License (source) | Maintenance | Notes |
|---|---|---|---|---|
| `tokio-socks` | 0.5.3 (2026-05-29) `[api]`; cache 0.5.2 `[cache]` | `license = "MIT"` `[cache]` `tokio-socks-0.5.2\Cargo.toml` | Active — 23.5M downloads `[api]` | **Recommended.** Async SOCKS4/4a/5 client, `SOCKS5` + `SOCKS4` + auth. Does the TCP connect through the proxy; you wire it into your connector |
| `hyper-socks2` | 0.9.1 — **2024-03-08** `[api]` | `Apache-2.0 OR MIT` `[api]` | **Slowing — no release in ~2.5 years** (1.7M downloads) | SOCKS5 layer for `hyper` connectors. Still the most direct hyper integration, but stale |
| `async-socks5` | 0.6.0 — 2024-02-25 `[api]` | `Apache-2.0 OR MIT` `[api]` | Slowing (3.4M downloads) | SOCKS5 only |
| `socks` | 0.3.4 — 2022-02-19 `[api]` | `MIT/Apache-2.0` `[api]` | **Stale ~4 years** (42M downloads) | Synchronous SOCKS5. Fine, but blocking |
| `reqwest` `socks` feature | 0.13.5 `[api]` | `MIT OR Apache-2.0` `[cache]` | Active | Simplest path: `reqwest` with `socks` gives you SOCKS5 for all HTTP traffic `[cache]` — verified `socks = []` feature exists in the cached 0.13.4 manifest |
| `hyper-proxy` | 0.9.1 — **2021-03-27** `[api]` | `MIT` `[api]` | **Abandoned ~5 years**, 8.7M downloads | Historically the HTTP-CONNECT-for-hyper crate. **Do not adopt.** `hyper-util`'s own `client-proxy` feature supersedes it `[cache]` |
| HTTP CONNECT handling | — | — | — | Use `hyper-util` with features `client`, `client-legacy`, `client-pool`, **`client-proxy`**, **`client-proxy-system`** `[cache]` `hyper-util-0.1.20\Cargo.toml`. `client-proxy-system` is *built-in OS system-proxy discovery* |
| `hyper-util` `client-proxy-system` | 0.1.21 `[api]` | `MIT` `[cache]` | Active | **This is the lowest-effort route to "respect the Windows system proxy".** Read the feature list from the cached manifest `[cache]` |

### 8b. PAC files

| Crate | Version (date) | License (source) | Maintenance | Notes |
|---|---|---|---|---|
| `proxyparser` | 0.2.0 (2026-08-12) `[api]` | `MIT` `[api]` | **Very new** — repo created 2025-12-23 `[api]`, **452 downloads** `[api]`; repo `github.com/zongyangbigpolo/rustproxyparser` `[api]`. Self-described: *"A cross-platform system proxy detector with strong support for complex enterprise PAC scripts"* `[api]`, keywords `http-proxy, network, pac, proxy, system-proxy` `[api]` | **The only crate that directly targets "enterprise PAC scripts."** License is the best possible (MIT). Adoption is negligible → **toy-adjacent; usable but you must own the risk** |
| `proxy-watch` | 0.3.0 (2026-10-09) `[api]` | `MIT OR Apache-2.0` `[api]` | **Very new** — repo created 2026-09-07 `[api]`, **1,044 downloads** `[api]`; repo `github.com/gnoays/proxy-watch` `[api]`. Self-described: *"Read and watch the operating system's proxy settings, layered with `*_proxy`, and the route each URL takes under them"* `[api]`, keywords `http, pac, proxy, system-proxy, watcher` `[api]` | **Watching** proxy changes is a genuinely useful feature (proxy flips mid-download in corporate/VPN environments). New; small |
| `libproxy` | 0.1.1 — **2019-11-21** `[api]` | **`MIT`** `[api]` (the Rust *binding* crate) | **Abandoned ~7 years**, 57k downloads | **Critical distinction:** the crate is MIT, but **libproxy the C library is LGPL-2.1-or-later**. `[web]` Confirmed by the docs for the binding and its factory (`docs.rs/libproxy`, `libproxy/factory.rs`) and general knowledge of the project. **Depending on the binding pulls in an LGPL C library → disqualified under your rules**, unless you take the LGPL dynamic-linking route with relink obligations, which is not worth it here |
| `pacparser` | **not found on crates.io** `[api]` | — | — | No Rust binding. (The upstream C `pacparser` is LGPL — another LGPL trap to note) |
| `system_proxy` | **0.0.0** (2023-03-12) `[api]` | **`MPL-2.0`** `[api]` | **Version 0.0.0 = never actually released** `[api]`, 26,993 downloads | Not usable. MPL-2.0 would be acceptable unmodified, but a 0.0.0 placeholder is not a dependency |
| `proxy_config` | 0.0.2 — **2017-08-16** `[api]` | `MIT/Apache-2.0` `[api]` | **Abandoned ~9 years** | Do not use |
| **PAC evaluation via a JS engine** | see below | — | — | If `proxyparser`/`proxy-watch` do not cover a customer's PAC script, you need to evaluate JavaScript |

**JS engine options for PAC (`FindProxyForURL(url, host)`) — licensing is the whole story here:**

| Option | Version (date) | License (source) | Verdict |
|---|---|---|---|
| `boa_engine` | 0.22.0 (2026-08-28) `[api]` | **`Unlicense OR MIT`** `[api]` | **Best licensing.** Pure Rust, no C toolchain. Caveat: Boa's own docs say it does not yet implement *all* of JavaScript `[api]` description — for PAC that is usually fine (`FindProxyForURL` uses a small ES subset plus the PAC-specific helper functions `isPlainHostName`, `dnsDomainIs`, `shExpMatch`, `myIpAddress`, `dnsResolve`, `isInNet`, etc., which you must inject yourself) |
| `rquickjs` | 0.14.0 (2026-09-18) `[api]` | `MIT` `[api]` (bindings) — QuickJS itself is MIT `[web]` | **Strong alternative.** Mature, widely used (5.2M downloads `[api]`), embeddable engine with full ES2020. Adds a C build. **Verify the QuickJS version bundled and that it is the MIT branch** (QuickJS is MIT; the `quickjs-ng` fork is also MIT — both fine) |
| `quickjs` | 0.1.0 — **2019-07-14** `[api]` | `MIT` `[api]` | **Abandoned ~7 years**, 10k downloads. Do not use |
| `duktape` | 0.0.2 — **2015-01-28** `[api]` | `Unlicense` `[api]` | Abandoned 11 years |
| `js-sys` | 0.3.106 `[api]` | `MIT OR Apache-2.0` `[api]` | **WASM-only** — irrelevant for a Windows desktop app |

**Recommendation for PAC:** prefer `hyper-util`'s `client-proxy-system` + a first-party PAC evaluator over `boa_engine`, and treat `proxyparser`/`proxy-watch` as *optional accelerators* you can drop if they disappoint, because they are too new to depend on structurally. **Never touch `libproxy`/`pacparser` — LGPL.**

### 8c. Windows system proxy discovery — first-party paths

Two mechanisms, both reachable from `windows`/`windows-sys` (permissive, §11):

1. **WinHTTP:** `WinHttpGetIEProxyConfigForURL` / `WinHttpGetProxyForUrl` with `WINHTTP_AUTOPROXY_OPTIONS` (`WINHTTP_AUTOPROXY_AUTO_DETECT` + `WINHTTP_AUTOPROXY_CONFIG_URL`) — this is the API that actually executes PAC/WPAD. Returns a proxy string plus a bypass list, and can hand you the PAC URL.
2. **Registry (WinINET):** `HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings` — `ProxyEnable` (DWORD), `ProxyServer` (string, may be `host:port` or a per-scheme `http=…;https=…` list), `ProxyOverride` (bypass list, supports `<local>`), and `AutoConfigURL` (the PAC URL). Read via `winreg` or `windows-registry`.

`[web]` Corroborating evidence that this is a real pattern: the `proxy-watch` crate documents reading "the plain HKCU registry values" in its Windows backend (`proxy_watch/sys/win/mod.rs`), and `windows-erg` 0.2.0 (2026-06-04) `[api]`, `MIT` `[api]`, wraps Windows APIs ergonomically.

| Crate | Version (date) | License (source) | Notes |
|---|---|---|---|
| `winreg` | 0.56.0 (2026-03-14) `[api]`; cache 0.55.0 `[cache]` | `license = "MIT"` `[cache]` `winreg-0.55.0\Cargo.toml` | **Recommended** for the `Internet Settings` keys and for autostart/file-association registry writes |
| `windows-registry` | **0.100.0** (2026-09-03) `[api]`; cache 0.6.1 `[cache]` | `MIT OR Apache-2.0` `[cache]` | Microsoft's own registry crate. Note the version jumped 0.6.1 → **0.100.0** — a major API change is in flight; **pin carefully** |
| `windows-sys` | 0.61.2 (2025-10-06) `[api]`; cache 0.52.0/0.59.0/0.60.2/0.61.2 `[cache]`, all `MIT OR Apache-2.0` `[cache]` | Permissive | Raw FFI for `WinHTTP`, `WinINet`, `Shell`, `Win32_UI` |
| `windows` | 0.62.2 (2025-10-06) `[api]`; cache 0.62.2 `[cache]` | `license = "MIT OR Apache-2.0"` `[cache]` | The ergonomic, typed projection. **Use this**, not `winapi` |
| `winapi` | 0.3.9 — **2020-06-26** `[api]` | `MIT/Apache-2.0` `[cache]` | **Legacy/superseded.** Do not start new code on it |
| `windows-erg` | 0.2.0 (2026-06-04) `[api]` | `MIT` `[api]` | 192 downloads — toy-adjacent. Skip |

---

## 9. Persistence

| Crate | Version (date) | License (source) | Maintenance | Notes |
|---|---|---|---|---|
| **`rusqlite`** | 0.40.2 (2026-08-08) `[api]` | **`MIT`** `[api]` | **Very active** — 123M downloads `[api]` | **Recommended.** Ergonomic synchronous SQLite wrapper. Use with `tokio::task::spawn_blocking` or a dedicated DB thread |
| `libsqlite3-sys` | 0.38.2 (2026-08-08) `[api]`; cache 0.32.0 `[cache]` | **`license = "MIT"`** `[cache]` `libsqlite3-sys-0.32.0\Cargo.toml` | Active | The `-sys` crate. **SQLite itself is public domain** — the safest possible license. `bundled` feature compiles the amalgamation from source, removing any system-SQLite dependency. Note the crate declares `MIT` for its *own* code; SQLite's public-domain status is separate and even more permissive |
| `sqlx` | 0.9.0 (2026-05-21) `[api]`; cache 0.9.0 with `sqlx-sqlite-0.9.0` `[cache]` | `license = "MIT OR Apache-2.0"` `[cache]` `sqlx-0.9.0\Cargo.toml` | Active (162M downloads) | Async-first, compile-time-checked queries. Heavier than `rusqlite` and drags in a lot (the cache already has `sqlx-mysql`, `sqlx-postgres` alongside `sqlx-sqlite`). **`librqbit` uses `sqlx` only for its optional Postgres persistence backend** `[git]`, so taking `librqbit` does not force `sqlx` on you unless you enable that feature |
| `refinery` | 0.10.0 (2026-10-02) `[api]` | `MIT` `[api]` | Active (11M downloads) | **Recommended** for schema migrations on top of `rusqlite`. Download-manager schemas change often; do not hand-roll migrations |
| `sea-orm` / `diesel` | 2.0.4 (2026-09-27) / 2.3.14 (2026-10-07) `[api]` | `MIT OR Apache-2.0` `[api]` | Active | Full ORMs. Overkill for a download manager's metadata store |
| **`redb`** | 4.3.0 (2026-10-02) `[api]` | `MIT OR Apache-2.0` `[api]` | **Active** — 13.1M downloads `[api]` | **Recommended embedded alternative.** Pure Rust, no C, ACID, single-file, MVCC. Excellent for a piece/segment bitmap and KV state where SQL is noise |
| `fjall` | 3.1.12 (2026-10-03) `[api]` | `MIT OR Apache-2.0` `[api]` | Active (1.8M downloads) | LSM-tree KV, pure Rust, LSM + value-log. Also permissive |
| `sled` | **1.0.0-alpha.124** — **2024-10-11** `[api]` | `MIT OR Apache-2.0` `[api]` | **Stalled in alpha for ~2 years** `[api]` | The famous beta-forever database. **Do not choose for a new product** |
| `heed` | 0.22.1 (2026-04-07) `[api]` | `MIT` `[api]` | Active (6.6M downloads) | LMDB binding. LMDB is the OpenLDAP Public License (BSD-like, permissive) — UNVERIFIED in this session. Adds a C dependency |
| `lmdb-rkv` | 0.14.0 — **2020-03-10** `[api]` | `Apache-2.0` `[api]` | **Abandoned ~6 years** | Superseded by `heed` |
| `rocksdb` | 0.25.0 (2026-08-16) `[api]` | `Apache-2.0` `[api]` | Active (57M downloads) | C++ build, large binary. Wrong tool for a desktop app |
| `duckdb` | 1.10506.0 (2026-09-30) `[api]` | `MIT` `[api]` | Active | Analytical DB; irrelevant here |

**Recommendation:** `rusqlite` (bundled SQLite, MIT + public domain) with `refinery` for migrations, plus `redb` if you want a zero-C store for hot segment state. Both fully permissive. `serde_json` 1.0.154 `[api]` (MIT OR Apache-2.0 `[cache]`) for settings/config, `toml` 1.1.8 `[api]` (`MIT OR Apache-2.0` `[api]`) if you prefer user-editable config.

---

## 10. Windows integration

| Need | Crate | Version (date) | License (source) | Maintenance | Notes |
|---|---|---|---|---|---|
| **Win32 API surface** | `windows` | 0.62.2 (2025-10-06) `[api]`; cached `[cache]` | `MIT OR Apache-2.0` `[cache]` | Active, Microsoft | **Use this.** Typed projection over `windows-sys`. Feature-gate narrowly; it is a huge crate otherwise |
| Raw FFI escape hatch | `windows-sys` | 0.61.2 `[api]` | `MIT OR Apache-2.0` `[cache]` | Active | For items not yet projected |
| **Tray icon** | `tray-icon` | 0.26.1 (2026-10-07) `[api]` | `MIT OR Apache-2.0` `[api]` | **Active** — 34.3M downloads `[api]` | **Recommended.** Cross-platform tray/menu-bar, from the Tauri org. Also gives you the context menu. Requires a window/event-loop integration on Windows |
| **Notifications** | `notify-rust` | 4.18.2 (2026-10-07) `[api]` | `MIT OR Apache-2.0` `[api]` | **Active** — 16.2M downloads `[api]` | **Recommended.** On Windows uses the WinRT toast API. Gives you "download complete" toasts properly |
| **Autostart** | `auto-launch` | 0.6.0 (2026-01-10) `[api]` | **`MIT`** `[api]` | Maintained — 6.65M downloads `[api]` | **Recommended.** Writes `HKCU\...\Run` on Windows / XDG autostart on Linux. Small, single-purpose |
| Autostart (manual) | `winreg` | 0.56.0 `[api]` | `MIT` `[cache]` | Active | Trivial to do yourself — `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. Which makes `auto-launch` optional |
| **File association** | *none needed* | — | — | — | **No crate.** Use `windows` for `IApplicationAssociationRegistration`/`ShellExecute` or (recommended) write the `HKCU\Software\Classes\<ext>` + `RegisteredApplications` + `Capabilities` keys yourself. Windows 8+ **requires** the `Capabilities`/`RegisteredApplications` approach to appear in "Default apps" — a naive `HKCU\Software\Classes\.torrent` write will register but will not be user-selectable. **Plan this as first-party code** |
| **Single instance** | `interprocess` | 2.4.5 (2026-10-10) `[api]` | **`0BSD OR Apache-2.0`** `[api]` | **Very active** — 15.8M downloads, released 2026-10-10 `[api]` | **Recommended.** `0BSD` is maximally permissive. Cross-platform local sockets (Windows named pipes) — ideal for "second launch hands its arguments to the running instance", which is exactly the correct single-instance UX |
| Single instance (alternative) | `single-instance` | 0.3.3 — **2021-12-16** `[api]` | `MIT` `[api]` | **Stale ~5 years** (946k downloads) | Works, simple mutex approach, but unmaintained and gives you no IPC for argument forwarding |
| Single instance (alternative) | `tauri-plugin-single-instance` | 3.0.0-alpha.2 (2026-10-10) `[api]` | `Apache-2.0 OR MIT` `[api]` | Active but Tauri-coupled | Only if you adopt Tauri |
| Custom protocol / browser handoff | `windows` (`Win32_UI_Shell`) | — | permissive | — | Register `HKCU\Software\Classes\<scheme>` yourself so `magnet:`/`ed2k:`/`metalink:` links launch your app |
| Resource embedding (icon, manifest, version) | `embed-resource` | 3.0.12 (2026-10-06) `[api]` | `MIT` `[api]` | Active — 44.6M downloads `[api]` | **Recommended** over the stale `winres` 0.1.12 (2021-09-28) `[api]` |
| Elevation / service mode | `windows-service` | 0.8.1 (2026-05-08) `[api]` | `MIT OR Apache-2.0` `[api]` | Active | Only if you need a background service |
| Open file / folder in Explorer | `open` (5.4.4, 2026-09-10) `[api]`, `opener` (0.9.0, 2026-09-29) `[api]`, `webbrowser` (1.2.4, 2026-08-05) `[api]` | `MIT` / `MIT OR Apache-2.0` `[api]` | All active | All permissive. `open` is the simplest |
| File dialogs | `rfd` | 0.17.2 (2026-01-12) `[api]` | `MIT` `[api]` | Active — 34.7M downloads `[api]` | Native file pickers if you build a GUI |
| GUI (if you need one) | `eframe` 0.36.2 (2026-09-08) `[api]`, `iced` 0.14.0 (2025-12-07) `[api]`, `tauri` **3.0.0-alpha.4** `[api]` | `MIT OR Apache-2.0` (eframe/iced), `Apache-2.0 OR MIT` (tauri) `[api]` | Active | All permissive. Note `tauri` latest published is an **alpha** `[api]` |
| GUI — **DISQUALIFIED** | `slint` | 1.18.1 (2026-09-21) `[api]` | **`GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0`** `[api]` | Active | **Explicitly flagged: not usable.** The GPL-3.0 option is disqualified and the alternatives are proprietary "source available"-style licenses, not OSI-permissive. **You must not use Slint** in an MIT-licensed product unless you buy the royalty-free/software license and accept its terms — which is a commercial decision, not a licensing one |
| Windows version detection / device info | `sysinfo` | 0.39.6 (2026-07-09) `[api]` | `MIT` `[api]` | Active | Fine |
| Crash/dump, ETW, WinRT async | `windows` + `windows-future` (cached) `[cache]` | `MIT OR Apache-2.0` `[cache]` | Active | `windows-future` is in the cache and is needed for WinRT toast/async paths |

**Recommended GUI note:** if the product is a desktop GUI, `eframe` (egui) or `iced` are both fully MIT/Apache-2.0 and avoid the Slint licensing problem entirely.

---

## 11. Utility: segmented/range downloads, retry/backoff, rate, hashing

### 11a. Range / segment download helpers

| Crate | Version (date) | License (source) | Notes |
|---|---|---|---|
| `http-range-header` | 0.4.2 — **2024-11-28** `[api]` | `MIT` `[api]` | Parses `Range`/`Content-Range` headers (126M downloads). Server-oriented but the parsing is reusable |
| — | — | — | **There is no crate that does multi-connection segmented downloading for you.** No "download accelerator" building block exists permissively. This is first-party: probe with a `HEAD`/`GET Range: bytes=0-0`, read `Accept-Ranges` + `Content-Length` + `ETag`/`Last-Modified`, split into N segments, schedule them across a pool, coalesce, verify, and persist a segment bitmap. The pieces: `reqwest` with header overrides, `tokio-util` `CancellationToken`/`SyncIoBridge` (cached, `MIT` `[cache]`), `futures` `FuturesUnordered` (`MIT OR Apache-2.0` `[cache]`), `memmap2` 0.9.11 `[api]` (`MIT OR Apache-2.0` `[cache]`) or `tokio::fs::File::write_at` for the output |
| `intervaltree` | 0.2.7 (cached) `[cache]` | `MIT` `[cache]` | Useful for tracking which byte ranges are done/in-flight across mirrors. Small and stable |
| `async-compression` | 0.4.50 (2026-09-30) `[api]`; cache 0.4.39 `[cache]` | `MIT OR Apache-2.0` `[cache]` | Streaming gzip/brotli/zstd/deflate **decompression**. Note: when you issue `Range` requests you generally want `Accept-Encoding: identity`, so this is mainly for non-ranged paths. `librqbit` itself uses it for tracker/`.torrent` decompression `[git]` |
| `fs2` | 0.4.3 — **2018-01-06** `[api]` | `MIT/Apache-2.0` `[cache]` | **Stale ~8 years.** File locking (advisory). Prefer `fd-lock` 4.0.4 (2025-03-10) `[api]`, `MIT OR Apache-2.0` `[api]`, or `interprocess` which also does locks |

### 11b. Retry / backoff

| Crate | Version (date) | License (source) | Maintenance | Notes |
|---|---|---|---|---|
| **`backon`** | 1.6.0 (2025-10-18) `[api]`; cache 1.6.0 `[cache]` | `license = "Apache-2.0"` `[cache]` `backon-1.6.0\Cargo.toml` | Active — 94.4M downloads `[api]` | **Recommended.** Async-first, `tokio`-native, jittered exponential backoff, retry *and* rate-limit policies, tower-compatible. Apache-2.0. `librqbit` itself depends on it `[git]` |
| `tokio-retry` | 0.3.2 (2026-06-09) `[api]` | `MIT` `[api]` | Active — 102M downloads | Simple, well-known, MIT. Slightly less featureful than `backon` |
| `backoff` | 0.4.0 — **2021-12-14** `[api]` | `MIT/Apache-2.0` `[api]` | **Stale ~5 years** (108M downloads) | Historically dominant but unmaintained. Prefer `backon`/`tokio-retry` |
| `retry` | 2.2.0 (2026-01-31) `[api]` | `MIT` `[api]` | Active (20.9M downloads) | Blocking/`Iterator`-style retry |
| `again` | 0.1.2 — **2020-05-31** `[api]` | `MIT` `[api]` | Stale | Do not use |
| `governor` | 0.10.4 (2025-12-16) `[api]`; cache 0.10.4 `[cache]` | `license = "MIT"` `[cache]` | Active — 83.5M downloads | **Recommended** for rate limiting (GCRA): global and per-host request throttling |
| `leaky-bucket` | 1.1.2 (2024-05-22) `[api]`; cache 1.1.2 `[cache]` | `MIT OR Apache-2.0` `[cache]` | Slowing — 2.8M downloads | Alternative rate limiter; used by `librqbit` for BT rate limits `[git]` |
| `tower` (`limit`, `retry`, `timeout`) | 0.5.3 `[cache]` | `MIT` `[cache]` | Active | If your HTTP path is tower-based, these layers cover it without extra crates |

### 11c. Progress and rate calculation

| Crate | Version (date) | License (source) | Notes |
|---|---|---|---|
| `indicatif` | 0.18.6 (2026-07-01) `[api]` | `MIT` `[api]` | **Recommended** for CLI progress: multi-bar, per-download bars, ETA/rate built in. 233M downloads |
| `size_format` | 1.0.2 (cached) `[cache]` | `MIT OR Apache-2.0` `[cache]` | Byte-size formatting |
| `humansize` | 2.1.3 — 2022-12-31 `[api]` | `MIT/Apache-2.0` `[api]` | Stale but stable; more format options |
| — | — | — | **Smoothed rate (EWMA) and per-torrent/per-file rate display are application code.** There is no crate for "bytes/sec over a sliding window with good UX". `quanta` (cached, `[cache]`, permissive) gives cheap monotonic timestamps if `Instant` is not precise enough |

### 11d. Hashing (BitTorrent SHA-1, BT v2 SHA-256, eD2K MD4 + ed2khash + AICH SHA-1)

| Crate | Version (date) | License (source) | Notes |
|---|---|---|---|
| `sha1` | 0.11.0 (2026-07-10) `[api]`; cache 0.10.6 and 0.11.0 `[cache]` | `license = "MIT OR Apache-2.0"` `[cache]` | **Required for BitTorrent piece verification.** RustCrypto, hardware-accelerated via `cpufeatures`/`sha1-asm` |
| `sha2` | 0.11.0 (2026-03-25) `[api]`; cache 0.10.9/0.11.0 `[cache]` | `MIT OR Apache-2.0` `[cache]` | For BT v2 (BEP 52) infohashes and any SHA-256 Metalink verification |
| `md-5` | 0.11.0 (2026-03-27) `[api]`; cache 0.10.6/0.11.0 `[cache]` | `MIT OR Apache-2.0` `[cache]` | MD5 — for Metalink `hash type="md5"` and legacy HTTP `Content-MD5`. **Note the crate name is `md-5`, not `md5`** |
| `md4` | 0.11.0 (2026-03-27) `[api]`; cache 0.10.2 `[cache]` | `MIT OR Apache-2.0` `[cache]` | **Required for eD2K.** The ed2khash is MD4-based |
| `digest` | 0.11.3 `[cache]` | `MIT OR Apache-2.0` `[cache]` | The unifying trait; also gives you `Digest` generics so you can write the eD2K tree hash generically |
| `crypto-hash` | 0.3.4 — **2019-08-18** `[api]` | `license = "MIT"` `[cache]` | Convenience wrapper (5.7M downloads). Stale; use RustCrypto directly in new code |
| `librqbit-sha1-wrapper` | 9.0.1 `[api]` | `Apache-2.0` `[api]` | Abstracts `ring` vs `sha1`. Only take it if you take `librqbit` |
| `ring` | 0.17.14 (published 2025-03-11) `[api]` | `Apache-2.0 AND ISC` `[cache]` | **Dormant.** Do not choose as a new dependency in 2026 — prefer the pure-Rust `sha1`/`sha2` crates |
| `hex` / `base64` / `data-encoding` | cached `[cache]` | `MIT OR Apache-2.0` `[cache]` | Encoding for hashes, magnet `xt`, ed2k link params |
| `bitvec` / `bitv.rs` patterns | `bitvec` 1.0.1 `[cache]`; `librqbit\src\bitv.rs` `[git]` | `MIT` `[cache]` | Piece/segment bitmaps. `librqbit` has its own `bitv.rs` and a pluggable `BitVFactory` `[git]` |

**Note on `ring` and crypto-provider churn across the stack:** the cache shows both `ring` 0.17.14 and `aws-lc-rs` 1.16.0, and `rustls` 0.23 can use either. Pick **one** provider deliberately; mixing produces build bloat and, in `librqbit`'s case, the `sha1-ring` vs default feature choice `[git]` `crates\librqbit\Cargo.toml`.

**Other useful, verified-permissive utilities from the cache** `[cache]`: `anyhow` 1.0.104 (`MIT OR Apache-2.0`), `thiserror` 2.0.18 (`MIT OR Apache-2.0`), `tracing` 0.1.44 + `tracing-subscriber` 0.3.23 (`MIT`), `clap` 4.6.1 (`MIT OR Apache-2.0`), `serde` 1.0.228 / `serde_json` 1.0.150 (`MIT OR Apache-2.0`), `bytes` 1.11.1 (`MIT`), `byteorder` 1.5.0 (`Unlicense OR MIT`), `chrono` 0.4.45 (`MIT OR Apache-2.0`), `uuid` 1.23.4 (`Apache-2.0 OR MIT`), `dashmap` 6.1.0 (`MIT`), `directories` 6.0.0 (`MIT OR Apache-2.0`), `etcetera` 0.11.0 (`MIT OR Apache-2.0`), `dunce` 1.0.5, `fs_extra` 1.3.0, `walkdir` 2.5.0 (`Unlicense/MIT`), `memmap2` 0.9.9, `metrics` 0.24.6 (`MIT`), `socket2` 0.6.4, `network-interface` 2.0.5, `if-addrs` 0.15.0 (`MIT OR BSD-3-Clause`), `encoding_rs`/`chardetng` (charset detection for filenames), `normpath`-style helpers.

---

## 12. Recommended stack

**Runtime and networking**
- `tokio` 1.5x (`MIT`) — runtime
- `reqwest` 0.13 (`MIT OR Apache-2.0`) with features `http2`, `http3`, `rustls`, `socks`, `stream`, `json`, `gzip`/`brotli`/`zstd`, `cookies` — HTTP/1.1 + HTTP/2 + HTTP/3, pooling, ranges, redirects
- `hyper` 1.x + `hyper-util` 0.1 (`MIT`) with `client`, `client-legacy`, `client-pool`, `client-proxy`, **`client-proxy-system`** — when you need the connector yourself (proxy CONNECT, custom DNS, happy-eyeballs)
- `hyper-rustls` 0.27 + `tokio-rustls` 0.26 + `rustls` 0.23 (stable, **not** 0.24-dev) — TLS
- `rustls-platform-verifier` 0.7 (+ `rustls-native-certs` 0.8 as the fallback/user-selectable path) — **this is the Chinese-CA / enterprise-MITM answer**
- `aws-lc-rs` **or** `ring` — pick one crypto provider deliberately (aws-lc-rs if you accept the CMake/NASM build cost)
- `quinn` 0.11 / `h3` 0.0.x — only via `reqwest`'s `http3` feature; keep HTTP/3 opt-in per download because `h3` is pre-1.0

**BitTorrent**
- `librqbit` 9.0.x (`Apache-2.0`) — engine: magnet, DHT-only bootstrap, seeding, selective download, torrent creation, streaming with seek, MSE, rate limits, resume/persistence, private torrents (partial), BEP 3/5/7/9/10/11/12/14/15/20/23/27/29/32/47/53
- Sibling crates only if you need them standalone: `librqbit-core`, `librqbit-dht`, `librqbit-bencode`, `librqbit-tracker-comms`, `librqbit-peer-protocol`, `librqbit-lsd`, `librqbit-upnp`, `librqbit-utp` (0.7.x) — all `Apache-2.0`
- `mainline` 8.0.1 (`MIT`) — DHT if you want it decoupled from `librqbit`
- `bendy` 0.6.1 (`BSD-3-Clause`) — canonicalizing bencode when you touch `.torrent`/infohash code outside `librqbit`
- `portmapper` 0.19.x **only after verifying its license** — otherwise `igd-next` 0.18 (`MIT`) + `librqbit-upnp`

**Streaming (HLS/DASH)**
- `m3u8-rs` 6.0.x (`MIT`) — HLS playlists
- `dash-mpd` 0.21.x (`MIT`) — MPD parse **and** segment download
- `aes` 0.9 + `cbc` 0.2 + `block-padding` (`MIT OR Apache-2.0`) — HLS AES-128
- `ts2mp4` 0.4 (`MIT`) + `mpeg2ts-reader` 0.18 (`MIT/Apache-2.0`) + `mp4` 0.14 (`MIT`) — TS→MP4 remux path
- **Optional, user-supplied:** external `ffmpeg.exe` invoked as a subprocess. **Never link `libav*`.**

**File transfer protocols**
- `suppaftp` 12.x (`MIT OR Apache-2.0`) with the **`rustls`** feature — FTP/FTPS, resumable (`resume_transfer`), `mlsd`
- `russh` 0.64 + `russh-sftp` 3.0 (`Apache-2.0`) — SSH/SFTP
- `ssh2` 0.9.6 (`MIT OR Apache-2.0`, vendored **libssh2 = BSD-3-Clause**) — only if you need libssh2 maturity; it is *licensing-acceptable* but adds C

**Proxy**
- `tokio-socks` 0.5.3 (`MIT`) and/or `reqwest`'s `socks` feature
- `hyper-util` `client-proxy-system` for OS proxy discovery
- Windows: `windows` / `windows-sys` (`WinHttpGetIEProxyConfigForURL`, `WinHttpGetProxyForUrl`) + `winreg` (`Internet Settings` keys)
- PAC: first-party evaluator on `boa_engine` 0.22 (`Unlicense OR MIT`) or `rquickjs` 0.14 (`MIT`); optionally trial `proxyparser` 0.2 (`MIT`) / `proxy-watch` 0.3 (`MIT OR Apache-2.0`), both very new
- **Never** `libproxy` (LGPL C library), **never** `pacparser` (LGPL C library)

**Persistence**
- `rusqlite` 0.40 (`MIT`, `bundled`) + `libsqlite3-sys` 0.38 (`MIT`; **SQLite itself is public domain**) + `refinery` 0.10 (`MIT`) for migrations
- `redb` 4.3 (`MIT OR Apache-2.0`) for pure-Rust segment/state storage
- `serde`/`serde_json`/`toml` for config

**Windows shell integration**
- `windows` 0.62 + `windows-sys` 0.61 (`MIT OR Apache-2.0`)
- `tray-icon` 0.26 (`MIT OR Apache-2.0`) — tray + menu
- `notify-rust` 4.18 (`MIT OR Apache-2.0`) — toasts
- `auto-launch` 0.6 (`MIT`) or direct `HKCU\...\Run` via `winreg`
- `interprocess` 2.4.5 (`0BSD OR Apache-2.0`) — single instance **with argument forwarding**
- `embed-resource` 3.0 (`MIT`) — icon/manifest/version resources
- `rfd` 0.17 (`MIT`) if GUI; `eframe` 0.36 or `iced` 0.14 (`MIT OR Apache-2.0`) for the GUI itself
- File association and custom URI schemes (`magnet:`, `ed2k:`, `metalink:`): **first-party** `HKCU\Software\Classes` + `Capabilities`/`RegisteredApplications`

**Utilities**
- `backon` 1.6 (`Apache-2.0`) and/or `tokio-retry` 0.3.2 (`MIT`) — retry/backoff
- `governor` 0.10 (`MIT`) — rate limiting (`leaky-bucket` 1.1 as the `librqbit`-compatible alternative)
- `indicatif` 0.18 (`MIT`) — CLI progress
- `bytes`, `tokio-util` (`CancellationToken`), `futures` (`FuturesUnordered`), `memmap2`, `intervaltree`, `http-range-header` — segment engine building blocks
- `sha1` 0.11, `sha2` 0.11, `md-5` 0.11, `md4` 0.11, `digest` 0.11 (`MIT OR Apache-2.0`) — all hashing, including eD2K's MD4 tree
- `anyhow`, `thiserror`, `tracing`, `tracing-subscriber`, `clap`, `directories`/`etcetera`

**Explicitly excluded (licensing)**
- `slint` — `GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0`
- `libproxy` crate → LGPL-2.1 C library; `pacparser` → LGPL C library
- `libssh-rs` → libssh, LGPL-2.1 **or** BSD-2-Clause depending on build (unverified)
- Any `libav*` linking (LGPL, and GPL with `--enable-gpl`); any GPL FFmpeg feature set
- `boring`/`boring-sys` — BoringSSL license mix, not on your allowlist, large C build
- `emulebb-rust` and all eMule/aMule/MLDonkey-derived code — copyleft; do not read, do not reference
- `webpki-roots` — `CDLA-Permissive-2.0`, permissive but off-list, and the wrong choice for these network environments anyway
- **36 `Unicode-3.0` (ICU) transitive crates** — permissive but off-list; add explicitly to the allowlist or make a conscious decision to accept them

---

## 13. Gaps: what we must write ourselves

| # | Gap | Why it is ours | Size |
|---|---|---|---|
| 1 | **Multi-connection segmented HTTP(S) downloader** | No crate does it. Range probing, N-way split, segment scheduler, per-segment persistence, coalescing, verification, resume across restart, adaptive connection count | **Large** — the core of the product |
| 2 | **Multi-source / mirror aggregation** | No crate at all. Mirror ranking, speed scoring, source failover, cross-mirror range coalescing, hash-based source rejection | **Large** |
| 3 | **Metalink (RFC 5854 + Metalink/HTTP)** | The only crate (`metalink` 0.1.0, MIT) is a 5-year-old single-release unfinished parser. Write your own parser + the whole mirror policy layer. Specs: [RFC 5854](https://datatracker.ietf.org/doc/html/rfc5854), [draft-bryan-metalinkhttp](https://datatracker.ietf.org/doc/html/draft-bryan-metalinkhttp), [IANA `application/metalink4+xml`](https://www.iana.org/assignments/media-types/application/metalink4+xml) | **Medium** |
| 4 | **eD2K / eMule client** | No permissive library exists. Server protocol, source exchange, queue/credit, Kad, AICH, partfile `.part`/`.part.met`, obfuscated datagram transport. Plus the **ed2khash MD4 tree (9,728,000-byte blocks with recursion)**, AICH SHA-1 tree, and `ed2k://` link parsing. Specs: [amule: Ed2k protocol](https://wiki.amule.org/wiki/Ed2k_protocol), [amule: Ed2k link](https://wiki.amule.org/wiki/Ed2k_link), [amule: AICH](https://wiki.amule.org/wiki/AICH), [IANA `ed2k` scheme](https://www.iana.org/assignments/uri-schemes/prov/ed2k), Kulbak & Bickson *eMule Protocol Specification* | **Very large** — the single biggest item |
| 5 | **BitTorrent web seeds (BEP 19 / BEP 17)** | Confirmed **absent** from `librqbit` by source grep `[git]`. Needs `url-list`/`httpseeds` parsing, HTTP range fetching mapped onto pieces, and integration into the piece picker | **Medium** |
| 6 | **HLS/DASH download engine** | Both parsers exist; neither fetches-and-assembles correctly. Live-playlist reload, `EXT-X-BYTERANGE` math, `EXT-X-MAP` init-segment handling, AES-128 key fetch + `IV` defaulting + PKCS#7 unpadding, discontinuity/`EXT-X-DISCONTINUITY-SEQUENCE` handling, segment ordering, fMP4 vs TS track selection, variant selection from the master playlist | **Large** |
| 7 | **Remux / join policy** | `ts2mp4` is young (2.7k downloads). Decide per-case: raw concatenation, TS→MP4 remux, or external ffmpeg. Handle codec edge cases (H.265, AC-3, PID changes, PCR discontinuities) | **Medium–large** |
| 8 | **PAC file evaluator** | If `proxyparser`/`proxy-watch` (both <1.1k downloads, both created within the last year) do not cover a customer's PAC script, you need your own `FindProxyForURL` sandbox with the PAC helper functions injected | **Medium** |
| 9 | **Windows file association + custom URI schemes** | No crate. Windows 8+ requires the `Capabilities`/`RegisteredApplications` structure, not just `HKCU\Software\Classes`. Needed for `magnet:`, `ed2k:`, `metalink:`, `.torrent`, `.metalink` | **Small–medium** |
| 10 | **BitTorrent peer connections through a proxy** | `librqbit` uses `reqwest` with SOCKS for HTTP paths, but peer-wire-over-SOCKS is not evident `[git]`. If "all traffic through the proxy" is a requirement (it usually is in the target market), this is a patch to or wrapper around the peer connector | **Medium** |
| 11 | **Rate smoothing / ETA / progress model** | No crate does EWMA rate, remaining-time, or a unified progress model across HTTP + BT + HLS + eD2K. Share model, per-file and per-torrent aggregation, and persistence so progress survives restart | **Small–medium** |
| 12 | **`.torrent` v2 / hybrid handling** | `librqbit`'s BEP 52 support is partial `[git]`. v2-only and hybrid torrents need real work (SHA-256 tree, file-tree `path`, cross-version infohash) | **Medium** |
| 13 | **Scheduler and global resource governor** | Max concurrent downloads, per-host connection caps, global bandwidth cap split fairly across protocols, prioritisation, pause/resume semantics. No crate | **Medium** |
| 14 | **Path/filename safety and sanitisation** | Untrusted names from `.torrent`, HLS URIs, Metalink, and eD2K links. Path traversal, Windows reserved names (`CON`, `NUL`, `AUX`, `COM1`…), long paths, invalid characters, duplicate-name collision | **Small but security-critical** |
| 15 | **SCP support** | `russh` gives SSH; SCP as a distinct legacy protocol is UNVERIFIED and probably needs implementing or shelling out | **Small–medium** |

---

## 14. Things I could not verify (stated plainly, not guessed)

1. **`portmapper` 0.19.3 license** — the crates.io API metadata for it was not retrieved in this session. Do not adopt before checking.
2. **`natpmp` 0.5.0 license** — not queried.
3. **`neqo-*` (Mozilla QUIC)** — could not resolve a crates.io package; status unknown.
4. **BoringSSL's exact license composition** for `boring-sys` 5.2.0 — the crate is `MIT`; the vendored BoringSSL is a mix and is off your allowlist. Unverified in detail.
5. **`libssh` (for `libssh-rs`) license branch** — LGPL-2.1 vs BSD-2-Clause depends on build. Unverified.
6. **`emulebb-rust` LICENSE file** — `github.com` was unreachable from this environment. On the strength of it being an eMule-parity project, treat as copyleft-contaminated and out of bounds regardless.
7. **`libmetalink`'s own license** — the C library is not on crates.io and its license was not confirmed.
8. **Field-level DASH feature coverage in `dash-mpd`** (SegmentTimeline, multi-period, `ContentProtection` DRM detection) — inferred from the crate description and dependency list, not read from source.
9. **HLS `SAMPLE-AES` / `EXT-X-KEY` `KEYFORMAT=com.apple.streamingkeydelivery`** — FairPlay DRM. `m3u8-rs` *parses* `keyformat`/`keyformatversions` `[cache]` but nothing in the permissive stack can decrypt it; DRM content is out of scope unless you integrate a DRM module.
10. **`russh` SCP support** — unverified; likely absent.
11. **`openssh-sftp-client` subprocess-vs-protocol implementation** — strongly implied but not confirmed.
12. **README-level feature claims** that I corroborated only through the crates.io description field rather than the repository README, because `raw.githubusercontent.com` and `github.com` were unreachable: `dash-mpd`, `ts2mp4`, `proxyparser`, `proxy-watch`, `metalink`. The *licenses and versions* for all of these came from the registry API and are solid; the *capability* claims are weaker.

---

*Prepared from: local cargo registry cache (`C:\Users\sn1p4\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f`, 429 crates, all `Cargo.toml` license fields read), local cargo git checkouts (`rqbit-d896eb2b048eb41a\d9e79c9`, 14 workspace crates, source inspected for capability claims), the crates.io registry JSON API (live, this session), and public web search. Nothing under `E:\dsh-workspace\tdmfast\chua` or `E:\dsh-workspace\tdmfast\tdmfast` was accessed.*
