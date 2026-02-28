# compare-dir

Tool to compare files in two directories.

# Installation

It is recommended to install using `pipx` or `uv`.

```shell-session
pipx install compare-dir
```
```shell-session
uv tool install compare-dir
```

# Usages

```shell-session
compare-dir dir1 dir2
```

For files that exist in both directories:
* Modified time are compared.
* Sizes are compared.
* If sizes are the same, content is compared.
