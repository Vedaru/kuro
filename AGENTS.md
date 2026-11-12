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
- **`kuro` uses jemalloc, not glibc malloc** (`#[global_allocator]` in
  `kuro-tui/src/main.rs`). The same 60+ thread sync left glibc with 69 per-thread
  arenas and RSS creeping ~1.9 MB/hour. jemalloc shares arenas and returns pages
  on its default 10 s decay: 214 MB flat vs 360 MB and climbing, same 3.7 MB/s.
  Do not remove it or reintroduce `MALLOC_ARENA_MAX` tuning; the allocator is the
  fix. Zero decay (163 MB) is possible but costs ~10% more CPU; default is the
  power-friendly choice.
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
- **A pack manifest is untrusted ground truth, so `validate_pack_manifest_for`
  in `pack.rs` accepts it whole or refuses it whole.** No partial installs: an
  empty tier document, a `common` entry inside a tier body, an entry with no
  MD5, and a dest listed twice are all refusals *before* the first byte and
  before `unregister`. A skipped entry would otherwise let the tier register as
  healthy while a file is missing, which the launcher then reports as nothing to
  repair. A *full* manifest still refuses `deleteFiles`/`groupInfos`; the
  `patchConfig` update index is the one that carries them and has its own
  validator (`validate_pack_update_index`).
- **One path rule for packs, the official one.** A tier writes
  `Client/Content/<TIER>/`, and any pack writes
  `Client/Saved/Resources/<pack version>/`. The version directory is the only
  discriminator; there is no per-option-pack tag or tier-suffix check. The rest
  of `Client/Saved` is game state. `common` is the base client, so a tier
  install (`kuro pack`) never touches it.
- **Pack updates go through `patchConfig`, not a full body.** `pack_download`
  stages into `.incremental_download/packs/<name>` and writes a resumable
  `plan.json`; `pack_apply` applies `common` first, then the active tier, and
  writes `launcherDownloadConfig.version` only after both land. The nested
  `predownload` block is used only when the formal packs are current. A
  missing source-version delta is a refusal, never a full-body or other-tier
  fallback.
- **Tasks are per game.** `UiState.tasks` has one slot per game, so WuWa and
  PGR install/download at the same time. A task owns its `JoinHandle` only
  when it is safe to stop (predownload, sync, install, pack, remove); apply
  and checkout have none. `Ctrl+C` stops the active game's task and `q` quits
  all of them.
  `remove_pack` deletes the tier directory and both launcher records, and
  leaves the versioned `Client/Saved/Resources` payloads alone.
- **A cancelled download must abort its chunk tasks.** `download_chunked` owns
  byte-range tasks and its progress ticker in `JoinSet`s; dropping the future
  aborts them. Detached `tokio::spawn` tasks here survive the parent and keep
  streaming after the UI says the task stopped.
- **`Esc` is navigation, never a stop.** It pops the detail screen and leaves
  the task running. `Ctrl+C` is the only stop key; while a task runs it stops
  it, and at idle it quits.
- **Checkout is the manifest delta, not a hardcoded file list.** CN ⇄ Bilibili
  fetches both channels' full manifests at the installed version, downloads
  every target-only or changed file, deletes source-only files, then writes the
  target appId. A version skew is refused, not silently downgraded. The old
  `diff_files` list outlived the files it named and made every switch fail.
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
- **A from-zero install writes its identity last.** `install_with_progress`
  builds a manager from the requested game/server instead of reading
  `launcherDownloadConfig.json`, so an aborted install leaves no config claiming
  a version that is not complete. The first successful sync writes it, then the
  installed tier(s) are registered. The TUI's blank row is the default install
  target and becomes a real row when the install finishes.

## Tests

Integration tests are per-scenario files under `crates/kuro-core/tests/` using an
in-process HTTP server from `tests/common/mod.rs` (which also holds `md5_bytes`
/ `md5_file`). `tests/common` allows dead code because each test binary gets its
own copy. There is no network in the suite — every test drives a local server
with synthetic manifests.
