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
  failure), **sync** (parallel full-tree MD5 verify + repair), **checkout**
  (CN <-> Bilibili channel switch via diff-file swap + appId).
- `kuro-tui` — ratatui UI with live status and task progress.
  Keys: `r` refresh · `d` predownload · `a` apply · `s` sync ·
  `c` checkout (CN<->bilibili) · `q` quit.

Not yet: PGR server endpoints (discovery procedure below), global checkout
(package differences), per-group progress gauges (names shown for now),
persistent MD5 cache for sync speedups.

## Adding PGR (Punishing: Gray Raven) — endpoint discovery

Status (2026-08): **partially done.** Verified from the official CN installer
(`PGR_install_2.6.3.0.exe` → `KRPluginConfig.json` + embedded FE bundle):

- GameId **G148**, appId **10012**, pkgId A1472 (see `kuro-api::config::pgr_cn_meta`)
- Same gamestarter CDN platform as WuWa
- Official installer gateway: `download.kurogames.com/pns/official/cn/zh-Hans/pc_app.json`

**Blocked on one piece:** unlike WuWa (static token `10003_Y8xXrXk…`), PGR's
launcher fetches its config URL at runtime from
`pc-launcher-sdk-haru-api.kurogames.com` (Spring Boot; endpoint path not
embedded in the launcher — verified by exhaustive string scans of every
launcher file). Completion options:

1. Run the official launcher once under wine with a MITM proxy (install
   mitmproxy, import its CA into the wine prefix), capture the SDK response
   → the `launcherConfigUrl` containing the token
2. Run the launcher on a Windows machine with Fiddler/Charles once, paste the
   `launcherConfigUrl`
3. Reverse the SDK API endpoint (clientId/Secret are known; path guessing so
   far unsuccessful)

**Community check:** no public PGR updater/launcher-replacement project exists
(GitHub repos+issues, GameBox, KuroLauncher, ww-manager issues, moyingji,
52pojie, bilibili all checked) — the official launcher is the only update
mechanism the community has, unlike WuWa's ww-manager.

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
