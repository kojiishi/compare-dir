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

A command line tool to compare two directories and show the differences.
It can also find duplicated files within a single directory.

### Compare Directories

In this mode,
the tool compares two directories
by comparing the modified time and sizes.
It also compares file contents if the file sizes are the same.
This mode is useful to verify backup copies.

### Find Duplicates

In this mode,
the tool discovers exact duplicated files.

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
|| ` ` | Modified time are not comparable. |
| 3rd | `=` | Same file sizes and contents. |
|| `!` | Same file sizes but contents differ. |
|| `>` | `dir1` is larger. |
|| `<` | `dir2` is larger. |
|| ` ` | Sizes are not comparable. |

For example:
```
=>= dir/path
```
means that `dir/path` in `dir1` is newer than the file in `dir2`,
but they have the same file sizes and contents.

The following bash example creates a list of paths of the same contents.
```bash
compare-dir -s <dir1> <dir2> | grep '^..=' | cut -c 5-
```
If you prefer `sed` over `cut`:
```bash
compare-dir -s <dir1> <dir2> | grep '^..=' | sed 's/^....//'
```
To do this in PowerShell:
```powershell
compare-dir -s <dir1> <dir2> | sls '^..=' | %{$_ -replace '^....',''}
```

## Compare Files
[compare files]: #compare-files

When comparing files,
comparing byte-to-byte is faster
if you compare them only once,
but comparing [hashes](#hash) is faster
if you compare them multiple times
because hashes are saved in the [hash cache].

The `--compare` (or `-c`) option can change
how files are compared.

| `--compare` | Meaning |
| --- | --- |
| size | Compare only by file sizes. |
| hash | Compare file contents by their hashes. |
| rehash | Same as `hash`, but recompute hashes without using the data in the [hash cache]. |
| full | Compare file contents byte-by-byte. |

## Hash

File hashes are computed
when comparing files
(with the [`-c hash` option](#compare-files)),
and when finding duplicates.

### Hash Cache
[hash cache]: #hash-cache

File hashes are saved to a file named `.hash_cache`
to make subsequent runs faster.

> [!NOTE]
> When backing up,
> if you intend to use this tool
> to verify backup copies,
> do not copy `.hash_cache`.
> You can also create the `.hash_cache`
> in the parent directory of the target directory.
> See [hash cache directory].

If file contents are changed without changing their modified time,
the cache needs to be invalidated.
You can invalidate the hash cache
by the [`-c rehash` option](#compare-files),
or by deleting the cache file.

The following example shows a scenario where
a different content is found,
make a backup copy,
and rehash the cache.
```shell_session
% compare-dir /master /backup
dir1/dir2/file: Contents differ
% cp /master/dir1/dir2/file /backup/dir1/dir2
% compare-dir -c rehash /master/dir1/dir2/file /backup/dir1/dir2
```

> [!NOTE]
> When the first argument is a file, not a directory,
> only the specified file is compared.
> The `-c rehash` option in this mode
> invalidates the hash cache only for the file,
> retaining hash caches for other files in the directory
> and its sub directories.

### Hash Cache Directory
[Hash Cache Directory]: #hash-cache-directory

If a `.hash_cache` cannot be found in the specified directory,
`compare-dir` will walk up the file tree until it finds one.
If none is found, a new `.hash_cache` is created in the specified directory.

For example:
```bash
touch /data/.hash_cache
compare-dir /data/dir
compare-dir /data/dir2
compare-dir /data
```
All three runs of `compare-dir` use
the same hash cache file at `~/data/.hash_cache`.
