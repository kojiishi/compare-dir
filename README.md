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

A command line tool to
[compare two directories and show the differences][compare directories].
It can also [find changed, corrupted][find changed files],
and [duplicated files][find duplicated files].

## Installation

**Prerequisites:** [Install Rust] if it's not installed yet.

[install Rust]: https://rustup.rs/

Then the following command installs `compare-dir`
from [crates.io][crate]:

```shell-session
cargo install compare-dir
```

See [Releases] for the change history.

[Releases]: https://github.com/kojiishi/compare-dir/releases

## Usages

`compare-dir` supports following features:

* [Compare Directories]
* [Find Changed or Corrupted Files][find changed files]
* [Find Duplicated Files]

[`--compare` option]: #compare
Please use the <span id="compare">`--compare` (`-c` for short) option</span>
to specify the feature.

| `--compare` | Meaning |
| --- | --- |
| auto (default) | Same as `check` if single argument, `hash` if two arguments. |
| size | [Compare directories]. Files are compared only by file sizes. |
| hash | [Compare directories]. Files contents are compared by their [hashes][hash]. |
| rehash | Same as `hash`, but recompute hashes without using the data in the [hash cache]. |
| full | [Compare directories]. Files contents are compared byte-by-byte. |
| check | [Find changed or corrupted Files][find changed files]. |
| update | Same as `check`, but also [update the hash cache][update]. |
| dup | [Find duplicated files]. |

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

Please see the [`--compare` option] to change how files are compared.

#### Output Formats
[output format]: #output-formats

When comparing two directories,
the output is human-readable by default.
The `--out` (or `-o`) option can change the output format.

| `--out` | Alias | Meaning |
| --- | --- | --- |
| default | d | Human-readable output. |
| symbol | s | Symbolized output, easier for programs to read. See [symbols]. |

For backward compatibility, `--symbol` (or `-s`) is also supported and is equivalent to `--out symbol`.

##### Symbols
[symbols]: #symbols

The symbolized format (`--out symbol`) output is as follows:

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
compare-dir -o symbol <dir1> <dir2> | grep '^..!' | cut -c 5-
```
If you prefer `sed` over `cut`:
```bash
compare-dir -o symbol <dir1> <dir2> | grep '^..!' | sed 's/^....//'
```
To do this in PowerShell:
```powershell
(compare-dir -o symbol <dir1> <dir2>) -match '^..!' -replace '^....'
```

### Find Changed or Corrupted Files
[find changed files]: #find-changed-or-corrupted-files

`compare-dir` can find changed files
by comparing hashes with the previously saved hashes in the [hash cache].
This is useful when there could be possible corruptions,
such as after unexpected power down or RAID rebuild.

First, the [hash cache] needs to be created.
There are two ways to do this.
* [Comparing directories][compare directories] creates it automatically,
  when it runs with the `-c hash` option (default).
* Use the `-c update` option.
  ```shell-session
  compare-dir -c update <dir>
  ```

Then the `-c check` option can find changed files.

```shell-session
compare-dir -c check <dir>
```
This prints the same [output format] as [compare directories],
as if it compares the current files
against the files when the [hash cache] was created.
For example, with the `-s` option (the [output format] is [symbols]):
```
<   file1
=<< file2
=<! file3
```
This means that:
* `file1` was added.
* `file2` became newer and larger.
* `file3` became newer and different content, but the size didn't change.

<a id="update"></a>
The `-c check` option doesn't update the [hash cache],
so that you can run it multiple times.
If you want to update the [hash cache],
please use `-c update` option instead.
This option prints the same output as `-c check`,
but also updates the [hash cache].

[update]: #update

### Find Duplicated Files
[find duplicated files]: #find-duplicated-files
[find duplicates]: #find-duplicated-files

`compare-dir` discovers exact duplicated files
with the `-c dup` option.

```shell-session
compare-dir -c dup <dir>
```

Finding duplicated files from multiple directories is also supported.
```shell-session
compare-dir -c dup <dir1> <dir2> <dir3>
```

#### Output Formats

| `--out` | Alias | Meaning |
| --- | --- | --- |
| default | d | Human-readable output. |
| yaml | y, yml | YAML format. |

The `--out yaml` (or `-o yaml` / `-o y` / `-o yml`) outputs the results in the YAML format.
You can use other tools such as [yq] to
convert the YAML results to JSON or other formats.
```shell-session
compare-dir -o yaml -c dup <dir> | yq -o json
```

[yq]: https://github.com/mikefarah/yq

## Backup Strategies
[backup]: #backup-strategies

When backing up, there are two strategies you can take.

### Strategy 1: Exclude `.hash_cache`

1. Backup by excluding the [hash cache] file.
   ```shell-session
   rsync -av --delete --exclude .hash_cache /path/to/source /path/to/backup
   ```
   On Windows, using `robocopy`:
   ```shell-session
   robocopy \path\to\source \path\to\backup /MIR /XF .hash_cache
   ```
2. Use `compare-dir <dir1> <dir2>` to verify.
   ```shell-session
   compare-dir /path/to/source/ /path/to/backup/
   ```
3. Check backup files are not changed or corrupted
   since the last comparison.
   ```shell-session
   compare-dir -c check /path/to/backup
   ```

This method is suitable for incremental backups,
as the step 2 computes hashes only for updated files.

The step 3 verifies files that are not supposed to change;
i.e., whose metadata (size and last modified time) are not changed,
since the last time their hashes are computed.
This step runs only in the backup directory,
without causing any I/O to the source directory.

### Strategy 2: Include `.hash_cache`

1. Update the [hash cache] in the source directory.
   ```shell-session
   compare-dir -c update /path/to/source
   ```
2. Backup all files, including the [hash cache] file.
   ```shell-session
   rsync -av --delete /path/to/source/ /path/to/backup/
   ```
3. Verify that the hashes of the backup files match the cached hashes.
   ```shell-session
   compare-dir -c check /path/to/backup
   ```

## Hash
[hash]: #hash

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
| hash, dup | Used if modified time doesn't change, updated otherwise. |
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
