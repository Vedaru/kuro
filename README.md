# kuro

A native Linux **installer and updater** for **Kuro Games** titles — Wuthering
Waves (鸣潮) and Punishing: Gray Raven (战双帕弥什). **No wine, no launcher.exe,
no hpatchz — one static Rust binary** that talks to the official CDN directly
and applies Kuro's `KrDiff` patches natively. It downloads and updates game
files; the game itself is launched via Steam + GE-Proton (see below).

Inspired by [ww-manager](https://github.com/timetetng/wutheringwaves-cli-manager);
the wine dependency is removed by applying patches via
[hdiffpatch-rs](https://github.com/TwintailTeam/hdiffpatch-rs).

```
┌status─────────────────────────────────────────────┐
│game:    punishing-gray-raven                      │
│server:  global                                    │
│local:   4.7.0                                     │
│remote:  4.7.0                                     │
│up to date                                         │
└───────────────────────────────────────────────────┘
┌task───────────────────────────────────────────────┐
│task: sync                                         │
│files done: 2429   queued: 26511                   │
│[████████░░░░░░░░░░░░░░░░░░░░░░░░░░░░]   9.8%      │
│  3.90/39.92 GiB  overall                         │
│[████████████████████████] 100.0%  25.5 MiB/…      │
└───────────────────────────────────────────────────┘
```

## Features

- **Multi-game TUI** — one binary manages WuWa and PGR side by side; installed
  games are auto-detected in your Steam libraries
- **status** — local vs remote version per game/server, live from the CDN
- **predownload** — stage the next update (`krpdiff` patches + full-file
  fallbacks) in the background, resumable
- **apply** — merge `KrDiff` patches **natively** (no wine), MD5-verify every
  output, atomic `.bak` swap, version bump; full-file fallback if a merge fails
- **sync** — parallel full-tree MD5 verify + repair of missing/corrupt files,
  resumable across restarts; followed by a **stale-artifact sweep** that only
  removes files the manifest has dropped (same directory *and* same extension as
  manifest entries). Live game state is never deleted: `Client/Saved/**` (the
  client's own resource/video channel, settings, saves, local storage, logs),
  the SDK / anti-cheat / crash-reporter payloads the client writes for itself,
  and anything in a directory the manifest doesn't ship into
- **persistent MD5 cache** — a verified file is never hashed twice: entries in
  `<game>/.kuro_cache/md5.json` are keyed by manifest path and validated on
  (size, mtime) like `make`/`git`, so re-verify costs one stat per file. Pure
  I/O savings — delete `.kuro_cache/` any time to force a full re-hash
- **install** — from-zero client download for any supported game
- **download options (quality packs)** — a channel's manifest carries the base
  paks plus one or more `Client/Content/<SD|HD|UHD>` bodies (WuWa 3.7.0 ships
  `HD` at ~42.6 GiB inside the same manifest), so `--quality sd|hd|uhd` picks
  which body an install/sync fetches instead of taking everything the channel
  serves. A body the channel does not serve is refused by name, not silently
  skipped. PGR (Unity) has no quality packs, so the option is WuWa-only
- **checkout** — CN ⇄ Bilibili channel switch for WuWa (diff-file swap + appId)
- Resumable, MD5-verified downloads with per-file progress bars

## Supported games & servers

| Game | CN 官服 | Bilibili | Global |
|------|---------|----------|--------|
| Wuthering Waves (G152/G153) | ✅ | ✅ | ✅ |
| Punishing: Gray Raven (G143/G148) | ✅ token | — | ✅ |

## Tokens

WuWa's launcher tokens are public and stable (same ones `ww-manager` ships), so
they're compiled in. **PGR's tokens are runtime-only** — they rotate, so they
live in `~/.config/kuro/tokens.toml` (or `KURO_PGR_GLOBAL_TOKEN` /
`KURO_PGR_CN_TOKEN`), never in the binary:

```toml
[pgr]
global = "…token from the launcher's WebView2 storage on a Windows install…"
cn = "…same, from a CN launcher install's cache…"
```

When PGR status starts failing (stale token), re-copy it from a real launcher
install's cache and update the file.

## Install

From a release (Linux x86_64):

```sh
curl -L -o kuro https://github.com/vedaru/kuro/releases/latest/download/kuro
chmod +x kuro
sudo mv kuro /usr/local/bin/
```

Or build from source (Rust ≥ 1.75):

```sh
cargo build --release
cp target/release/kuro ~/.local/bin/
```

## Usage

```sh
kuro                    # TUI — auto-detects installed games in Steam libraries
kuro <folder>...        # or point at game folders explicitly
kuro status <folder>    # CLI: print local/remote versions
kuro sync <folder> [--quality <sd|hd|uhd>]              # CLI: verify + repair
kuro install <wuwa|pgr> <cn|bilibili|global> <folder> [--quality <sd|hd|uhd>]
kuro quality <folder> [sd|hd|uhd]                       # CLI: which pack the client mounts
```

`--quality` on `install` / `sync` is the **download** side of that choice: only
the named `Client/Content/<TIER>` body is fetched, together with the base paks
and binaries every install needs. It defaults to everything the channel serves,
and a body the channel does not serve is an error naming what it does serve —
so "sd" on an HD-only channel tells you instead of quietly installing 42 GiB of
HD. The saved preset (`kuro quality <folder> hd`) is what a plain `sync` follows
afterwards, best-effort: if the channel currently serves no such body, the run
falls back to the full manifest rather than failing. PGR has no quality packs at
all, so there `--quality` is refused rather than ignored.

### TUI keys

| Key | Action |
|-----|--------|
| `Tab` | cycle focus: status → task → log (focused box is highlighted) |
| `←` / `→` | switch game |
| `↑` / `↓`, `PgUp` / `PgDn` | scroll the log (when focused) |
| `r` | refresh status |
| `d` | predownload update |
| `a` | apply predownloaded update |
| `s` | sync / repair files |
| `c` | checkout server (CN ⇄ Bilibili) |
| `i` | install a new game (`w`/`p` game · `c`/`b`/`g` server · `f` quality pack · `t` path · `s` Steam default) |
| `h` / `?` | help overlay |
| `q` | quit |

### Running the game (Steam + GE-Proton)

kuro installs **game files only** (no launcher — the official one doesn't run
under wine). Launch the game through Steam as a non-Steam shortcut:

1. Steam → *ADD A GAME* → *Add a Non-Steam Game…* → pick `PGR.exe` / `Wuthering Waves.exe`
2. Properties → Compatibility → force a Proton version (e.g. GE-Proton11-3)
3. Launch — first start is slow (shader compile); updates come from kuro, not Steam

> PGR ships ACE (AntiCheatExpert). Its kernel drivers can't load under Proton;
> most ACE games run fine with it absent, some refuse — no clean workaround.

## How it works

```
kuro ──► prod[-cn]-alicdn-gamestarter.kurogame.com/launcher/game/<GID>/<appId>_<token>/index.json
          │  weighted CDN list + patchConfig (old→new version transitions)
          ▼
        resource.json ──► {dest, md5, size, chunkInfos}[]
          │                  · .krpdiff entries = native patch payloads (zstd)
          │                  · everything else = full-file fallbacks
          ▼
        download (parallel ranged GETs, per-chunk MD5) → verify → atomic swap
```

- Incremental patches are **KrDiff** (Kuro's HDIFF19 variant) + zstd — applied
  natively by `kuro-patch`
- Official merge keeps originals untouched until outputs are MD5-verified, then
  swaps with `.bak` recovery — kuro mirrors that flow
- Full tree hashing streams in 1 MiB chunks (WuWa ships a 26 GB pak — reading
  whole files would OOM)

## Crate layout

```
crates/
├── kuro-api    — launcher protocol types + HTTP client (game-agnostic)
├── kuro-patch  — KrDiff / HDiff native patch engine + streaming MD5
├── kuro-core   — GameManager: download / apply / sync / checkout / install
└── kuro-tui    — ratatui frontend (binary: `kuro`)
```

Adding a new Kuro title = one entry in `kuro-api/src/config.rs`; the whole
pipeline is game-agnostic.

## Development

```sh
cargo test                      # unit + integration tests
cargo build --release           # static-ish release binary
cargo run -p kuro-core --example pgr_proof   # live CDN smoke test (PGR)
```

## Known limitations

- PGR launcher tokens rotate and are issued by Kuro's private launcher SDK at
  runtime — kuro cannot fetch them (yet), so both PGR servers need a token you
  supply yourself (`~/.config/kuro/tokens.toml`, see Tokens). PGR CN's CDN host
  is derived from the shared launcher platform, not yet confirmed against a
  live session
- ACE anti-cheat doesn't run under Proton (see above)
- Quality packs are a *download* choice for the bodies a channel serves, and they
  only cover `install` / `sync`: `predownload` / `apply` still stage the whole
  patch set for a version, because a partially-applied patch would record a
  version the install does not actually hold. A pack the channel does not serve
  cannot be installed at all — the client mounts a directory that was never
  downloaded without complaint from the CDN
- PGR has no quality packs to choose from: it is a Unity title whose assets live
  under `PGR_Data/` and whose client takes no `-krqlv` tier, so `--quality` is
  refused for it rather than pretended at
- The CDN manifest describes the base client only, so it is **not** an inventory
  of a WuWa install: the game's own resource channel (`Client/Saved/**`, i.e.
  Video/Lang/Resource packs — not the `Client/Content/Paks` base paks) is
  fetched by the client itself and is invisible to Kuro
- WuWa CN hotfixes (e.g. 3.6.0 → 3.6.1) have no `krpdiff` groups and no
  `fromFolder`: `predownload` stages the changed files as full files, and
  `apply` refuses to record the new version unless every target file is already
  on disk at its target hash — use `sync` for those, which downloads and
  verifies them directly

## License

MIT — see [LICENSE](LICENSE).
