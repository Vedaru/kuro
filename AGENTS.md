# kuro — agent notes

Native Linux installer/updater for Kuro Games titles (Wuthering Waves, PGR).
Rust workspace: `kuro-api` (launcher protocol + HTTP), `kuro-patch` (KrDiff /
HDiff engine + MD5), `kuro-core` (GameManager), `kuro-tui` (binary `kuro`).

## Commands

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release
```

Both CI workflows (`.github/workflows/release.yml`, `.forgejo/workflows/package.yml`)
run fmt, clippy and tests as gates before publishing. The Forgejo one is the job
that actually builds what machines install, so it is the one to check after CI
edits.

## Environment traps

- **Never pass `--all` to fmt/clippy.** `vendor/hdiffpatch-rs` is a path
  dependency but *not* a workspace member (see `exclude` in the root
  `Cargo.toml`). `cargo fmt --all` would reformat third-party code, and
  `--all` on clippy adds two dead-code warnings from it that are not ours.
- Toolchain is pinned at 1.98.1 in both workflows. Bumping it is a deliberate
  act with a green run behind it.
- `cargo` may be absent from `PATH` in a fresh sandbox: it lives in
  `$HOME/.cargo/bin`, and `rustup component add rustfmt clippy` is needed after
  a workspace re-clone.

## Design invariants worth knowing before editing

- **Path handling goes through `kuro-core/src/path.rs`** (`normalise`,
  `split_dest`, `path_key`, `extension_of`, `tmp_sibling`). `tmp_sibling` exists
  because `Path::with_extension` *replaces* the extension rather than appending
  to it.
- **Downloads go through `download::download_file`**, which picks single-GET vs
  parallel ranges. A manifest with no `chunkInfos` (every pack manifest, CN
  3.6.0 patch manifests) gets synthesized 4 MiB ranges — without that, a 26 GB
  pak is one throttled connection. Do not open-code the choice in a new loop;
  `tests/synth_ranges.rs` asserts the request pattern.
- **Download writes go through a 1 MiB `BufWriter`** (`WRITE_BATCH` in
  `download.rs`). `tokio::fs` turns every write into a blocking-pool task, so
  writing each network piece directly put a task and a cross-thread buffer
  handoff on every 8-16 KiB read. That churn was what grew the process's glibc
  arena pages. Keep the batching when touching either download loop.
- **`kuro` uses jemalloc with zero decay, not glibc malloc** (`#[global_allocator]`
  and `_rjem_malloc_conf` in `kuro-tui/src/main.rs`). The same 60+ thread sync
  left glibc with 69 per-thread arenas and RSS creeping ~1.9 MB/hour. jemalloc
  shares arenas, and `dirty_decay_ms:0`/`muzzy_decay_ms:0` decommit freed pages
  immediately: 168 MB flat vs 360 MB and climbing, same 3.7 MB/s. Do not remove
  it or reintroduce `MALLOC_ARENA_MAX` tuning; the allocator is the fix.
- **`Error::Unsupported` is a deliberate refusal** whose message is shown to the
  user verbatim, not a stub. Genuine failures use `Error::Patch`.
- **The MD5 cache is disposable** (`md5_cache.rs`): a poisoned mutex recovers by
  dropping its entries rather than panicking. `verify`/`sync` must never die
  because the cache did. `save` *snapshots* `touched` instead of draining it —
  a tier saves once per pack part, and a draining save would make the second
  call evict the first part's hashes (`saving_twice_keeps_the_first_saves_entries`).
- **Manifest strings are untrusted**: `validate_manifest_paths` in
  `kuro-api/src/client.rs` rejects absolute paths, drive letters, UNC paths and
  `..` at the single point a remote manifest enters the process. Any new field
  that becomes a filesystem path has to be added to it.
- **The stale-artifact sweep may only delete** files in a directory the manifest
  ships into *and* with an extension that directory uses, minus `is_protected`
  (`Client/Saved/**`, runtime logs/dumps/DBs, SDK and crash-reporter payloads).
  Widening this deletes game state that cannot be re-downloaded.
- **A pack manifest is untrusted ground truth, so `validate_pack_manifest` in
  `pack.rs` accepts it whole or refuses it whole.** No partial installs: an
  empty tier document, a `common` entry inside a tier body, an entry with no
  MD5, a dest listed twice, and any `deleteFiles` entry are all refusals
  *before* the first byte and before `unregister`. A skipped entry would
  otherwise let the tier register as healthy while a file is missing, which the
  launcher then reports as nothing to repair. (`deleteFiles` and `groupInfos`
  are `Error::Unsupported`: the packs never ship them, so refusing is honest.)
- **The tier body is matched as spelled, not case-folded.** The game reads
  `Client/Content/HD`; on a case-sensitive host a manifest spelling
  `client/content/hd` would write files the game never reads. Leading slashes
  and backslashes are still normalised first.
- **`--check` writes nothing at all**, including zero-length marker files. The
  scan only *collects* work; every filesystem write (download, marker) happens
  in the write phase. `check_does_not_create_marker_files` guards this.
- **Two launcher records, two writers.** `launcherDownloadConfig/<tier>.json`
  and the `bundles` block belong to the official updater as much as to kuro, so
  the downgrade guard reads both (`installed_versions`); kuro's own
  `.kuro_cache/packs.json` is only trusted while `bundles` still lists the tier.
  `unregister` runs before the first byte of a reinstall, so a crashed run
  leaves "not installed" rather than a record claiming missing files.

## Tests

Integration tests are per-scenario files under `crates/kuro-core/tests/` using an
in-process HTTP server from `tests/common/mod.rs` (which also holds `md5_bytes`
/ `md5_file`). `tests/common` allows dead code because each test binary gets its
own copy. There is no network in the suite — every test drives a local server
with synthetic manifests.
