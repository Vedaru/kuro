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
- **`Error::Unsupported` is a deliberate refusal** whose message is shown to the
  user verbatim, not a stub. Genuine failures use `Error::Patch`.
- **The MD5 cache is disposable** (`md5_cache.rs`): a poisoned mutex recovers by
  dropping its entries rather than panicking. `verify`/`sync` must never die
  because the cache did.
- **Manifest strings are untrusted**: `validate_manifest_paths` in
  `kuro-api/src/client.rs` rejects absolute paths, drive letters, UNC paths and
  `..` at the single point a remote manifest enters the process. Any new field
  that becomes a filesystem path has to be added to it.
- **The stale-artifact sweep may only delete** files in a directory the manifest
  ships into *and* with an extension that directory uses, minus `is_protected`
  (`Client/Saved/**`, runtime logs/dumps/DBs, SDK and crash-reporter payloads).
  Widening this deletes game state that cannot be re-downloaded.

## Tests

Integration tests are per-scenario files under `crates/kuro-core/tests/` using an
in-process HTTP server from `tests/common/mod.rs` (which also holds `md5_bytes`
/ `md5_file`). `tests/common` allows dead code because each test binary gets its
own copy. There is no network in the suite — every test drives a local server
with synthetic manifests.
