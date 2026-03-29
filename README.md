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

Compare two directories and show the differences, or find duplicated files within a single directory.

* For two directories, it compares the file contents if the file sizes are the same. Useful to verify backup copies.
* For a single directory, it cryptographically hashes matching file sizes to discover exact duplicates.

## Installation

```bash
cargo install compare-dir
```

See [Releases] for the change history.

[Releases]: https://github.com/kojiishi/compare-dir/releases

If you want to install from the local source code:
```bash
cargo install --path .
```

## Usage

Compare two directories:
```bash
compare-dir <dir1> <dir2>
```

Find duplicated files in a single directory:
```bash
compare-dir <dir>
```

Please use the `-h` option to see all options.

# Hash Cache

The file hashes are cached in a file named `.hash_cache`.

If you think file contents may be changed
without their last modified time changed,
please remove the cache file.
The tool will then recompute the hashes.

If one of ancestor directories has the cache file,
the nearest one is used instead.
If you want the cache file to be in an ancestor directory,
you can create an empty file.
```bash
touch ~/data/.hash_cache
compare-dir ~/data/subdir
```
This will use `~/data/.hash_cache` as the cache file
instead of creating `~/data/subdir/.hash_cache`.
