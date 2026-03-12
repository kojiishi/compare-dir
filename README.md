[![CI-badge]][CI]
[![crate-badge]][crate]
[![docs-badge]][docs]

[CI-badge]: https://github.com/kojiishi/compare-dir/actions/workflows/rust-ci.yml/badge.svg
[CI]: https://github.com/kojiishi/compare-dir/actions/workflows/rust-ci.yml
[crate-badge]: https://img.shields.io/crates/v/compare-dir.svg
[crate]: https://crates.io/crates/compare-dir
[docs-badge]: https://docs.rs/compare-dir/badge.svg
[docs]: https://docs.rs/compare-dir/

# compare-dir

Compare two directories and show the differences.

Useful to verify backup copies.

## Installation

```bash
cargo install compare-dir
```

If you want to install from the local source code:
```bash
cargo install --path .
```

## Usage

```bash
compare-dir <dir1> <dir2>
```

Please use the `-h` option to see all options.
