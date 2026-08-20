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
  resumable across restarts
- **install** — from-zero client download for any supported game
- **checkout** — CN ⇄ Bilibili channel switch for WuWa (diff-file swap + appId)
- Resumable, MD5-verified downloads with per-file progress bars

## Supported games & servers

| Game | CN 官服 | Bilibili | Global |
|------|---------|----------|--------|
| Wuthering Waves (G152/G153) | ✅ | ✅ | ✅ |
| Punishing: Gray Raven (G143/G148) | ⏳ token pending | — | ✅ |

PGR global's launcher token was recovered from a real Windows install's
WebView2 storage (the launcher fetches it at runtime via a private SDK API —
see `kuro-api/src/config.rs` for details).

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
kuro sync <folder>      # CLI: verify + repair
kuro install <wuwa|pgr> <cn|bilibili|global> <folder>   # CLI: fresh install
```

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
| `i` | install a new game |
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

- PGR CN (`G148`) launcher token not yet recovered (private SDK runtime flow)
- ACE anti-cheat doesn't run under Proton (see above)
- No persistent MD5 cache yet — sync re-hashes the tree each run (fast on NVMe)

## License

MIT — see [LICENSE](LICENSE).
