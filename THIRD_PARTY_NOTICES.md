# Third-party notices

## similar 3.1.2

- Source: crates.io registry source
  registry+https://github.com/rust-lang/crates.io-index; locked checksum
  85ee016af5d736b69fc89e19254540fa4b5f5492853fb5503920f084011c78b6.
  The package manifest records repository
  https://github.com/mitsuhiko/similar.
- License: Apache-2.0, verified from the registry manifest license field and
  the packaged LICENSE file.
- MSRV: Rust 1.85, from the registry manifest rust-version = "1.85".
- Project feature selection: default-features = false,
  features = ["text", "unicode", "inline"]. The registry manifest defines
  text, unicode = ["text", "unicode-segmentation", "bstr?/unicode"], and
  inline = ["text"]; its default additionally includes std, which this
  project does not enable.
