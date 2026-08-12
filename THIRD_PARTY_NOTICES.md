# Third-party notices

## gpui 0.2.2 and gpui-component 0.5.1

- Sources: `https://crates.io/crates/gpui/0.2.2` and `https://crates.io/crates/gpui-component/0.5.1`
- Declared in `Cargo.toml` as `gpui = "0.2.2"` and `gpui-component = "=0.5.1"`; resolved checksums are in `Cargo.lock`
- These crates provide the native desktop UI shell packaged as `devmanager.exe`
- Detailed `gpui-component` provenance remains in the section below

## rusqlite 0.39.0 (bundled SQLite)

- Source: `https://crates.io/crates/rusqlite/0.39.0`
- Declared in `Cargo.toml` as `rusqlite = { version = "0.39.0", features = ["bundled"] }`
- The bundled SQLite amalgamation is compiled into the application binaries; no separate SQLite redistributable is packaged
- Used for durable host/task storage. Packages do not ship user prompt databases or organization content

## similar 3.1.2 (prompt version diff)

- Source: `https://crates.io/crates/similar/3.1.2`
- Planned pin: `similar = { version = "=3.1.2", default-features = false, features = ["text", "unicode", "inline"] }`
- License: Apache-2.0
- Required only if actually locked after dependency union. Current `Cargo.lock` status: NOT_LOCKED / not present in `Cargo.lock`.
- When locked, `packaging/Assert-ThirdPartyProvenance.ps1` requires this exact version in notices.

## Selected crypto (exact Cargo.lock versions)

Machine-checked against `Cargo.lock` by `packaging/Assert-ThirdPartyProvenance.ps1`:

| Crate | Locked version | Root-direct |
| --- | --- | --- |
| `rustls` | `0.23.37` | yes |
| `ring` | `0.17.14` | no (via rustls) |
| `rcgen` | `0.13.2` | yes |
| `sha2` | `0.10.9` | yes |
| `hmac` | `0.12.1` | yes |
| `snow` | `0.10.0` | yes |
| `tokio-rustls` | `0.26.4` | yes |
| `webpki-roots` | `1.0.6` | yes |
| `zeroize` | `1.8.2` | yes |
| `web-push-native` | `0.4.0` | yes |
| `getrandom` | `0.3.4` | yes |
| `base64` | `0.22.1` | yes |

Connect production Noise is the pinned `snow` 0.10.0 crate listed above. Connect relay TLS uses `tokio-rustls` 0.26.4 plus `webpki-roots` 1.0.6 for `wss://` only. Packaging must not embed private keys, pairing secrets, or OS-vault material.

## snow 0.10.0 (Connect production Noise)

- Source: `https://crates.io/crates/snow/0.10.0`
- Upstream: `https://github.com/mcginty/snow`
- Declared in `Cargo.toml` as `snow = { version = "=0.10.0", default-features = false, features = ["ring-accelerated", "use-aes-gcm", "use-blake2", "use-curve25519", "use-getrandom"] }`
- License: Apache-2.0 OR MIT (crate metadata; no formal third-party audit claim)
- Locked checksum: `599b506ccc4aff8cf7844bc42cf783009a434c1e26c964432560fb6d6ad02d82`
- RustSec advisory RUSTSEC-2024-0011 (unauthenticated nonce increment on stateful `TransportState`) is patched in `snow >= 0.9.5`. This pin is 0.10.0.
- This is a security-note/risk record, not an audit. Production Connect uses snow's Noise XX/IK implementation rather than a homemade HMAC substitute.

## caseless 0.2.2 and unicode-normalization 0.1.24


- Sources: `https://crates.io/crates/caseless/0.2.2` and `https://crates.io/crates/unicode-normalization/0.1.24`
- Upstream repositories: `https://github.com/unicode-rs/rust-caseless` and `https://github.com/unicode-rs/unicode-normalization`
- The exact versions are pinned in `Cargo.toml` and recorded in `Cargo.lock`.
- `caseless` is MIT licensed; `unicode-normalization` is dual MIT/Apache-2.0 licensed. The published license texts were reviewed from the resolved registry packages.
- The Inbox search path uses the crates only for bounded compatibility-caseless Unicode normalization and full default case folding; no unbounded input or output is admitted.

## gpui-component 0.5.1

- Source: `https://github.com/longbridge/gpui-component/releases/tag/v0.5.1`
- Reviewed tag commit: `0f0ab35` (the v0.5.1 release page identifies this commit).
- Workspace manifest reviewed: `https://raw.githubusercontent.com/longbridge/gpui-component/v0.5.1/Cargo.toml`
- Component manifest reviewed: `https://raw.githubusercontent.com/longbridge/gpui-component/v0.5.1/crates/ui/Cargo.toml`
- Initialization implementation reviewed: `https://raw.githubusercontent.com/longbridge/gpui-component/v0.5.1/crates/ui/src/lib.rs`
- Upstream usage and license statement reviewed: `https://raw.githubusercontent.com/longbridge/gpui-component/v0.5.1/README.md`
- Published Apache license text: `https://docs.rs/crate/gpui-component/0.5.1/source/LICENSE-APACHE`
- Version is pinned exactly in `Cargo.toml` as `gpui-component = "=0.5.1"`.
- The v0.5.1 manifest declares `Apache-2.0`, uses workspace edition 2024, and has no explicit `rust-version`. Rust 1.85 is the edition floor because Rust 2024 became stable there; this is not an upstream claim that every transitive dependency has the same MSRV.
- The v0.5.1 manifest has no default features. Optional features reviewed and intentionally disabled here: `decimal`, `inspector`, `tree-sitter-languages`, and `webview`.
- Compatibility result: the package manifest and README both target GPUI `0.2.2`, matching this repository's direct `gpui = "0.2.2"` dependency. Cargo resolution and focused compilation are the final compatibility check.
- Direct upstream lockfile evidence: `https://raw.githubusercontent.com/longbridge/gpui-component/v0.5.1/Cargo.lock`

The lockfile's resolved package metadata was reviewed with `cargo metadata --format-version 1 --locked` after resolution. Starting at the activated `gpui-component 0.5.1` node, the normal feature graph contains 769 reachable packages; all 769 have a non-empty `license` field. The exact license-expression snapshot (counted packages) is:

| Metadata license expression | Packages |
| --- | ---: |
| `(Apache-2.0 OR MIT) AND BSD-3-Clause` | 1 |
| `(MIT OR Apache-2.0) AND NCSA` | 1 |
| `(MIT OR Apache-2.0) AND Unicode-3.0` | 1 |
| `0BSD` | 2 |
| `0BSD OR MIT OR Apache-2.0` | 1 |
| `Apache-2.0` | 24 |
| `Apache-2.0 / MIT` | 1 |
| `Apache-2.0 AND ISC` | 1 |
| `Apache-2.0 OR BSL-1.0` | 1 |
| `Apache-2.0 OR GPL-2.0-only` | 1 |
| `Apache-2.0 OR ISC OR MIT` | 4 |
| `Apache-2.0 OR MIT` | 69 |
| `Apache-2.0 WITH LLVM-exception` | 1 |
| `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | 16 |
| `Apache-2.0/MIT` | 6 |
| `BSD-2-Clause` | 4 |
| `BSD-2-Clause OR Apache-2.0 OR MIT` | 2 |
| `BSD-3-Clause` | 10 |
| `BSD-3-Clause OR Apache-2.0` | 2 |
| `CC0-1.0` | 4 |
| `CC0-1.0 OR Apache-2.0` | 1 |
| `CC0-1.0 OR MIT-0 OR Apache-2.0` | 1 |
| `ISC` | 5 |
| `ISC AND (Apache-2.0 OR ISC)` | 1 |
| `ISC AND (Apache-2.0 OR ISC) AND Apache-2.0 AND MIT AND BSD-3-Clause AND (Apache-2.0 OR ISC OR MIT) AND (Apache-2.0 OR ISC OR MIT-0)` | 1 |
| `MIT` | 175 |
| `MIT / Apache-2.0` | 4 |
| `MIT OR Apache-2.0` | 332 |
| `MIT OR Apache-2.0 OR LGPL-2.1-or-later` | 2 |
| `MIT OR Apache-2.0 OR Zlib` | 8 |
| `MIT OR Zlib OR Apache-2.0` | 1 |
| `MIT/Apache-2.0` | 39 |
| `MPL-2.0` | 3 |
| `Unicode-3.0` | 18 |
| `Unlicense OR MIT` | 8 |
| `Unlicense/MIT` | 2 |
| `Zlib` | 4 |
| `Zlib OR Apache-2.0 OR MIT` | 12 |

The graph does not activate the optional `decimal`, `inspector`, `tree-sitter-languages`, or `webview` feature dependencies. License expressions are the package metadata audit input; each resolved package remains subject to its own license text and notices.

## unicode-segmentation 1.12.0

- Source: `https://github.com/unicode-rs/unicode-segmentation/tree/v1.12.0`
- Crates.io package: `https://crates.io/crates/unicode-segmentation/1.12.0`
- Manifest reviewed: `https://raw.githubusercontent.com/unicode-rs/unicode-segmentation/v1.12.0/Cargo.toml`
- License text reviewed: `https://raw.githubusercontent.com/unicode-rs/unicode-segmentation/v1.12.0/LICENSE-MIT` and `https://raw.githubusercontent.com/unicode-rs/unicode-segmentation/v1.12.0/LICENSE-APACHE`
- The package declares `MIT OR Apache-2.0` and is pinned as `unicode-segmentation = "1.12.0"` in `Cargo.toml`; the exact resolved checksum is recorded in `Cargo.lock`.
- This dependency is used only for bounded extended-grapheme segmentation in task-header presentation and layout, including ZWJ emoji sequences.

## windows-capture 1.5.0

- Source: `https://github.com/NiiightmareXD/windows-capture/tree/1.5.0`
- Crates.io package: `https://crates.io/crates/windows-capture/1.5.0`
- Manifest reviewed: `https://raw.githubusercontent.com/NiiightmareXD/windows-capture/1.5.0/Cargo.toml`
- License text reviewed: `https://raw.githubusercontent.com/NiiightmareXD/windows-capture/1.5.0/LICENCE`
- The package declares `MIT`, targets Windows Graphics Capture, and uses the same `windows = 0.61.3` API family already pinned by this workspace. Its 1.5.0 manifest uses Rust edition 2024 and declares no runtime feature flags; the local toolchain and focused Windows build are the compatibility checks.
- The reviewed API provides direct `Window::from_raw_hwnd`, `ColorFormat::Bgra8`, `CursorCaptureSettings::WithoutCursor`, `DrawBorderSettings::WithoutBorder`, and `CaptureControl` stop/join support. The preview uses the direct HWND path and does not enumerate or match window titles.
