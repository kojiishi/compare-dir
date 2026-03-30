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

## Symbols

When comparing two directories,
the output is human-readable by default.
The `--symbol` (or `-s`) option changes the output format to be symbolized,
which is easier for programs to read.

| Position | Character | Meaning |
| --- | :---: | --- |
| 1st | `=` | In both directories. |
|| `>` | Only in `dir1`. |
|| `<` | Only in `dir2`. |
| 2nd | `=` | Modified time are the same. |
|| `>` | `dir1` is newer. |
|| `<` | `dir2` is newer. |
| 3rd | `=` | Same file sizes and contents. |
|| `!` | Same file sizes but contents differ. |
|| `>` | `dir1` is larger. |
|| `<` | `dir2` is larger. |

For example:
```
=>= dir/path
```
means that `dir/path` in `dir1` is newer than the file in `dir2`,
but they have the same file sizes and contents.

The following PowerShell example creates a list of paths of the same contents.
```powershell
compare-dir -s <dir1> <dir2> | sls '^..=' | %{$_ -replace '^.*? ', ''}
```

## Hash Cache

The file hashes are cached in a file named `.hash_cache`.

If you think file contents may be changed
without their last modified time changed,
please remove the cache file.
The tool will then recompute the hashes.

If one of ancestor directories has the cache file,
the nearest one is used instead.
For example:
```bash
touch ~/data/.hash_cache
compare-dir ~/data/subdir
```
Then `~/data/.hash_cache` is used as the cache file,
instead of `~/data/subdir/.hash_cache`.
