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
- Writing blobs via **16 parallel `git fast-import`** processes
- Writing trees and commits with a **native Rust pack writer** (no subprocess)
- Merging all shard packs into one with a **Rust pack merger** (verbatim byte copy,
  no decompression or recompression)
- Sharing objects with the upstream repo via **git alternates** (minimal extra disk use)

### Performance (SLE15-SP7, 60 888 patches)

Default mode (one commit per patch):

| Phase | Time |
|---|---|
| Read + parse patches | ~0.2 s |
| Pre-load source files | ~1.3 s |
| Apply patches + emit blobs | ~6.2 s |
| Drain blob shards | ~2 s |
| Emit commits (native Rust) | ~10.1 s |
| Repack (Rust pack merger) | ~0.7 s |
| **Total** | **~19 s** |

Times measured on a 16-core AMD Ryzen AI 7 PRO with warm page cache.
The apply phase is the dominant cost and is inherently serial (each patch
must see the results of all prior patches).

### Performance tuning

Two options offer large speedups at the cost of output fidelity.

**`-R` / `--no-repack`** — skip pack merge

The final repack step merges 17 separate pack files (16 blob shards + 1 commit
pack) into one.  With the native Rust merger this takes only ~0.6 s, so skipping
it saves little time.  The flag is mainly useful for debugging or when you intend
to run `git gc` later.

**`-1` / `--one-shot`** — single squashed commit

In normal mode kexpand writes one commit per patch, which means it also emits a
git blob for every intermediate state of every file — including all the versions
a heavily-patched file goes through before reaching its final content.  With
`--one-shot` all patches are applied to an in-memory tree first, and then only
the **final state** of each modified file is emitted as a blob; the result is a
single commit on top of the base tag.

Because many source files are touched by multiple patches across a large series,
skipping intermediate blob states cuts the number of blobs dramatically (23 636
vs several hundred thousand for SLE15-SP7), which slashes fast-import I/O and
makes the apply phase an order of magnitude faster.

The trade-off is the loss of per-patch git history: `git log -p`, `git blame`,
and `git bisect` at the patch level are no longer possible.  Use it when you
only need a snapshot of the fully-patched tree rather than the full history.

Combining both flags gives the fastest possible result:

| Configuration | Time (SLE15-SP7) |
|---|---|
| Default | ~19 s |
| `-R` (no repack) | ~18 s |
| `-1` (one-shot) | ~3.4 s |
| `-1 -R` | ~3.3 s |

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
branch you want to expand must be checked out there), or the `-C` option can be
used to specify that directory instead.

```
kexpand [OPTIONS] <OUTPUT>

Arguments:
  <OUTPUT>                         Output git repo to create

Options:
  -b, --branch <BRANCH>            Output branch name in the destination repo [default: master]
  -l, --linux <LINUX>              Upstream linux git repo (read-only) [$LINUX_GIT]
  -C, --directory <DIR>            Change to DIR before doing anything (like make -C)
  -j, --jobs <JOBS>                Threads for parallel parsing (0 = auto) [default: 0]
      --max-fuzz <FUZZ>            Hunk fuzz level [default: 2]
      --skip-failed                Warn and skip patches that fail to apply
  -v, --verbose                    Print each patch as it is applied
      --dry-run                    Parse and apply in memory without writing git output
  -q, --quiet                      Suppress progress output
  -R, --no-repack                  Skip the final pack merge step
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

Or equivalently, using `-C` to avoid changing directory first:

```sh
git -C ~/kernel/suse/kernel-source checkout SLE15-SP7

kexpand \
  -C ~/kernel/suse/kernel-source \
  --linux ~/git/linux \
  ~/git/suse-expanded \
  --branch SLE15-SP7
```

This reads `series.conf`, all patch files, and `rpm/config.sh` from the
kernel-source working tree, then creates `~/git/suse-expanded` with a branch
`SLE15-SP7` containing one commit per non-guarded patch.

The output repo uses git alternates so it shares all upstream objects with the
linux clone; only the new blobs and commit/tree objects are stored locally.

## Incremental update (`--update`)

Given an existing expanded repo whose commits carry a `suse-commit: <sha>` trailer
(one commit per kernel-source commit), `--update` appends the newer kernel-source
commits on top of it, mirroring the kernel-source DAG (merges, author/committer,
message plus `suse-commit:` trailer).  Only the files touched by changed patches are
recomputed; existing packs in OUTPUT are left untouched.

```sh
kexpand -C ~/kernel/suse/kernel-source --update -b SLE15-SP7 ~/kernel/suse/expand/kernel
```

- `-b` branch in OUTPUT to build on and update (default base: that branch)
- `--base <rev>`: build on another commit-ish of OUTPUT instead
- `--rev <rev>`: kernel-source revision to expand up to (default `HEAD`)
- `--force`: allow moving the branch to a non-descendant commit

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
