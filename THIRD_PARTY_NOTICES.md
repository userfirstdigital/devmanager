# Third-party notices

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
