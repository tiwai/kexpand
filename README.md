# kexpand

Fast tool to expand SUSE kernel-source.git patches into a standard Linux kernel git
repository, creating one git commit per patch (or a single squashed commit with
`--one-shot`).

## Background

SUSE's [kernel-source.git](https://github.com/SUSE/kernel-source) contains thousands
of patches applied on top of an upstream Linux kernel release.  The existing
`scripts/series2git` shell script applies them one at a time with `git am`, which is
slow for a large series.

`kexpand` does the same job much faster by:

- Reading and parsing all patches **directly from the working tree** (plain file reads,
  no git object inflation)
- Parsing all patches **in parallel** (via [rayon](https://github.com/rayon-rs/rayon)
  and [rapidquilt](https://github.com/openSUSE/rapidquilt)'s `libpatch`)
- Applying patches to an **in-memory file state** (no working-tree checkout needed)
- Loading source files **lazily** from the upstream git tree via
  [git2](https://github.com/rust-lang/git2-rs)
- Writing all commits in one pass with **`git fast-import`**
- Sharing objects with the upstream repo via **git alternates** (minimal extra disk use)

### Performance (SLE15-SP7, 60 857 patches)

| Phase | Time |
|---|---|
| Read patches | ~1.5 s |
| Parallel parse | ~0.3 s |
| Apply + git fast-import | ~30 s |
| **Total** | **~45 s** |

## Prerequisites

- Rust toolchain (edition 2021)
- A clone of [torvalds/linux](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git)
  (read-only access is sufficient)
- A checkout of SUSE's kernel-source.git at the branch you want to expand

## Build

```sh
cargo build --release
```

The binary is placed at `target/release/kexpand`.

## Usage

`kexpand` must be run from the root of a kernel-source.git working tree (i.e. the
branch you want to expand must be checked out there).

```
kexpand [OPTIONS] <OUTPUT>

Arguments:
  <OUTPUT>                         Output git repo to create

Options:
  -b, --branch <BRANCH>            Output branch name in the destination repo [default: master]
  -l, --linux <LINUX>              Upstream linux git repo (read-only) [$LINUX_GIT]
  -j, --jobs <JOBS>                Threads for parallel parsing (0 = auto) [default: 0]
      --max-fuzz <FUZZ>            Hunk fuzz level [default: 2]
      --skip-failed                Warn and skip patches that fail to apply
  -v, --verbose                    Print each patch as it is applied
      --dry-run                    Parse and apply in memory without writing git output
  -q, --quiet                      Suppress progress output
  -R, --no-repack                  Skip the final git repack step
      --repack-threads <THREADS>   Threads for git repack (0 = auto) [default: 0]
      --blob-shards <N>            Parallel blob fast-import processes (0 = auto) [default: 0]
  -1, --one-shot                   Squash all patches into a single commit
  -h, --help                       Print help
```

`-l` is required unless `$LINUX_GIT` is set.

### Example

```sh
cd ~/kernel/suse/kernel-source
git checkout SLE15-SP7

kexpand \
  --linux ~/git/linux \
  ~/git/suse-expanded \
  --branch SLE15-SP7
```

This reads `series.conf`, all patch files, and `rpm/config.sh` from the current
working tree, then creates `~/git/suse-expanded` with a branch `SLE15-SP7`
containing one commit per non-guarded patch.

The output repo uses git alternates so it shares all upstream objects with the
linux clone; only the new blobs and commit/tree objects are stored locally.

## How series.conf guards work

Lines in `series.conf` are classified as follows:

| First character | Meaning |
|---|---|
| `#` or whitespace+`#` | Comment — skipped |
| `+` or `-` | Guarded — skipped (patch is conditional on a build symbol) |
| Whitespace | Applied — the first token is the patch path |

Guarded patches are always skipped regardless of symbol; this matches the
behaviour expected for a vanilla (no extra symbols) build.

## Patch commit format

Each commit records:

- **Author / date** from the `From:` and `Date:` email headers
- **Subject** from the `Subject:` header (with `[PATCH ...]` prefix stripped)
- **Body** with `References:` and `Patch-mainline:` values

## License

MIT — see [LICENSE](LICENSE).

## Dependencies

- [rapidquilt](https://github.com/openSUSE/rapidquilt) `libpatch` — unified diff
  parser and hunk applicator (MIT)
- [git2](https://crates.io/crates/git2) — libgit2 bindings for reading upstream
  objects (MIT/Apache-2.0)
- [rayon](https://crates.io/crates/rayon) — data parallelism (MIT/Apache-2.0)
- [clap](https://crates.io/crates/clap) — CLI argument parsing (MIT/Apache-2.0)
- [chrono](https://crates.io/crates/chrono) — RFC 2822 date parsing (MIT/Apache-2.0)
- [memchr](https://crates.io/crates/memchr) — fast byte search (MIT/Unlicense)
- [anyhow](https://crates.io/crates/anyhow) — error handling (MIT/Apache-2.0)
