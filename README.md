# kuro

A native Rust reimplementation of the Wuthering Waves / Kuro Games launcher
workflow for Linux — **no wine, no hpatchz.exe, one static binary**.

Inspired by [ww-manager](https://github.com/timetetng/wutheringwaves-cli-manager)
(whose CDN protocol and staging flow this project mirrors), with the wine
dependency removed by applying Kuro's `KrDiff` patches natively via
[hdiffpatch-rs](https://github.com/TwintailTeam/hdiffpatch-rs).

## Status

Scaffolded workspace. What works today:

- `kuro-api` — serde types for the live launcher protocol (`index.json`,
  `patchConfig`, `groupInfos`, `chunkInfos`), weighted CDN selection, krpdiff
  URL building. Verified against the real WuWa CN CDN (3.5.3 → 3.6.0, 2026-08).
- `kuro-patch` — native KrDiff/HDiff application + patch inspection. A real
  krpdiff from the CDN parses as `KrDiff` + `zstd`; synthetic round-trips
  byte-match the reference `hpatchz` binary.
- `kuro-core` — `GameManager`: auto-detect game/server from
  `launcherDownloadConfig.json`, live status, predownload planning +
  resumable download (parallel chunked range GETs with per-chunk MD5),
  **apply pipeline** (parallel native KrDiff merges -> MD5 verify -> atomic
  `.bak` swaps -> `deleteFiles` -> version bump; full-file fallback on merge
  failure), atomic `.bak`-swap primitives.
- `kuro-tui` — minimal ratatui UI showing game/server/version status.

Not yet: `sync` (full-tree verify), `checkout` (server switch), PGR server
endpoints, TUI progress rendering for downloads.

## Crate layout

```
crates/
├── kuro-api    — launcher protocol types + HTTP client (game-agnostic)
├── kuro-patch  — KrDiff / HDiff native patch engine
├── kuro-core   — game manager: download / apply / sync / checkout
└── kuro-tui    — ratatui frontend (binary: `kuro`)
```

Adding a new Kuro title (e.g. Punishing: Gray Raven) = one entry in
`kuro-api/src/config.rs`; the pipeline is game-agnostic.

## Usage

```sh
cargo run -p kuro-tui -- [game-folder]
# default: ~/.local/share/Steam/steamapps/common/Wuthering Waves
```

## Key facts learned from the wire (2026-08)

- Launcher API: `https://prod[-cn]-alicdn-gamestarter.kurogame.com/launcher/game/<GID>/<appId>_<token>/index.json`
- Incremental patches are `KrDiff` (Kuro's HDIFF19 variant) with **zstd**
  compression — fully supported by `hdiffpatch-rs`, so no fallback engine needed.
- krpdiff URL: `{cdn}/{patchConfig.baseUrl}/{group.dest}`
  (`baseUrl` ≈ `resource/<appId>/<newVer>/<oldVer>/resources/`)
- `resource[]` entries ending in `.krpdiff` are patch payloads (size/md5 +
  `chunkInfos` for parallel range download); the rest are full-file fallbacks.
- Official merge needs ~2× patched-file size temporarily (old + new coexist);
  the flow keeps originals untouched until outputs are MD5-verified, then
  swaps with `.bak` recovery.

## License

MIT
