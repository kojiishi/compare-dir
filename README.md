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
It can also find changed, corrupted, and duplicated files.

## Installation

```shell-session
cargo install compare-dir
```

See [Releases] for the change history.

## Usages

`compare-dir` supports the following features:

* [Compare Directories]
* [Find Changed or Corrupted Files][find changed files]
* [Find Duplicates]

### Compare Directories
[compare directories]: #compare-directories

When you backup your important data,
it is important to keep in mind that
the backup may not be done correctly,
or the backup may somehow corrupt.
They can happen more often than you might imagine.

The following example compares two directories.
The comparison is done first by the modified time and sizes.
It also compares file contents if the file sizes are the same,
to verify backup copies are not corrupted.
Please see [compare files] for more details.

```shell-session
compare-dir <dir1> <dir2>
```

#### Compare Files
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

#### Symbols

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

The following bash example creates a list of paths of the same file sizes,
but different contents.
They often indicate possible copy failures or corruptions.
```bash
compare-dir -s <dir1> <dir2> | grep '^..!' | cut -c 5-
```
If you prefer `sed` over `cut`:
```bash
compare-dir -s <dir1> <dir2> | grep '^..!' | sed 's/^....//'
```
To do this in PowerShell:
```powershell
compare-dir -s <dir1> <dir2> | sls '^..!' | %{$_ -replace '^....',''}
```

### Find Changed or Corrupted Files
[find changed files]: #find-changed-or-corrupted-files

`compare-dir` can find changed files
by comparing hashes with the previously saved hashes in the [hash cache].
This is useful when there could be possible corruptions,
such as after unexpected power down or RAID rebuild.

First, the [hash cache] needs to be created.
[Comparing directories][compare directories] creates it.
Another way is to use the `-c update` option.

```shell-session
compare-dir -c update <dir>
```

Then the `-c check` option can find changed files.

```shell-session
compare-dir -c check <dir>
```

It prints a symbol, followed by the path.
| Symbol | Meaning |
| --- | --- |
| `+` | The file isn't in the [hash cache]. |
| `!` | The file is changed. |

The `-c check` option doesn't update the [hash cache],
so that you can run it multiple times.
If you want to update the [hash cache],
please use `-c update` option instead.
This option prints the same output as `-c check`,
but also updates the [hash cache].

### Find Duplicates
[find duplicates]: #find-duplicates

`compare-dir` discovers exact duplicated files
with the `-c dup` option.

```shell-session
compare-dir -c dup <dir>
```

[Releases]: https://github.com/kojiishi/compare-dir/releases

## Hash

`compare-dir` uses the [blake3] hash algorithm.

[blake3]: https://crates.io/crates/blake3

### Hash Cache
[hash cache]: #hash-cache

File hashes are saved to a file named `.hash_cache`.

This is used in many ways,
depending on the `--compare` option.

| `--compare` | Hash cache usage |
| --- | --- |
| full, size | Not used. |
| hash, dup | Used if modified time doesn't change. Updated otherwise. |
| rehash | Updated. |
| check | Used. |
| update | Used and updated. |

### Invalidation
[invalidation]: #invalidation
[invalidate]: #invalidation

When comparing files with the [`-c hash` option][compare files] (default),
hashes in the hash cache are used if the modified time doesn't change.

If file contents are changed without changing their modified time,
the cache needs to be invalidated.
You can invalidate the hash cache
by the [`-c rehash` option][compare files],
or by deleting the cache file.

The following example shows a scenario where
a different content is found,
make a new backup copy,
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

### Backup
[backup]: #backup

When backing up, there are two strategies you can take.

#### Exclude `.hash_cache`

1. Exclude `.hash_cache` when backing up.
2. Use `compare-dir <dir1> <dir2>` to verify.
3. Later, you can use `compare-dir -c check <backup-dir>`
   to verify the backup data isn't changed or corrupted.

#### Include `.hash_cache`

1. Update the cache in the source by `compare-dir -c update <source-dir>`.
2. Include `.hash_cache` when backing up.
3. Use `compare-dir -c check <backup-dir>` to verify.
