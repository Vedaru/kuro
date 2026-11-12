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
- **quality packs and the official pack update (SD / HD / UHD)** — WuWa
  CN/Bilibili publish the game version and the tiers through the launcher's
  pack document. `kuro update` downloads and applies `common` + the active
  tier through their `patchConfig` deltas, which is the official 3.7.1 → 3.8.0
  path: ~12 GiB of patches instead of a 54 GiB full body. `--download` stages
  a resumable predownload, `--apply` applies it, and `--check` previews it.
  `kuro sync` uses the same path for those channels. `kuro pack <folder>
  <sd|hd|uhd>` installs one tier from its full `resourcePacks[<tier>]` manifest
  and writes the two launcher records. Channels without a pack document
  (Global, PGR) stay on the base index. PGR (Unity) has no quality packs at all
- **checkout** — CN ⇄ Bilibili channel switch for WuWa (diff-file swap + appId)
- **built-in launcher** — `kuro play` starts the game itself. Since WuWa 3.7
  the client refuses to boot without `-krqlv=<SD|HD|UHD>` on its command line,
  and that argument can only be given at `exec` time, so kuro owns the spawn:
  `proton run`, or `umu-run` when no Proton is installed, in a kuro-managed
  prefix under the install. `kuro kill` then finds the client again by its
  recorded process group when it hangs on exit
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
kuro sync <folder> [--quality <sd|hd|uhd>]              # CLI: pack update + verify
kuro update <folder> [--check | --download | --apply]   # CLI: common + tier deltas
kuro install <wuwa|pgr> <cn|bilibili|global> <folder> [--quality <sd|hd|uhd>]
kuro quality <folder> [sd|hd|uhd]                       # CLI: which pack the client mounts
kuro pack [folder] <sd|hd|uhd> [--check|--remove]       # CLI: install or delete one tier
kuro --debug [command...]                               # write debug logs to a file
```

For WuWa CN/Bilibili the pack document is the single update path. `kuro sync`
applies `common` + the active tier's `patchConfig` deltas and reports the
result. `kuro update` is the same operation as an explicit command: `--check`
previews it, `--download` stages it, `--apply` applies the staged plan, and no
flag does both. `common` is the base client, so a tier install (`kuro pack`)
never touches it. The pack document's nested `predownload` block is consumed
only once the formal packs are current, and the resulting preinstall plan is
applied only on an explicit `a` / `--apply`.

`kuro sync` draws one overall progress bar on stderr: the verify count while
hashing, then bytes for the whole download (`common` + tier together). Piped
output gets no bar, only the final summary.

`kuro install` also works on an empty folder: it writes
`launcherDownloadConfig.json` only after the whole sync verifies, then records
the installed quality tier with the launcher. For WuWa CN/Bilibili the channel
index is the current base version (3.7.1 today), so follow a fresh install with
`kuro update` to reach 3.8.0. In the TUI, `i` defaults its target to the blank
row kuro opened with; the finished install becomes that row.

With nothing installed, the TUI lists both Wuthering Waves and Punishing: Gray
Raven as `not installed`; `Enter` or `i` on either row opens the install prompt
for that game (PGR defaults to the Global server).

A pack manifest has exactly one path rule: a tier owns
`Client/Content/<TIER>/`, and any pack may write the target version's
`Client/Saved/Resources/<version>/`. That is the official rule, and the version
directory is the only discriminator. A missing `patchConfig` entry for the
installed version is a refusal, not a fallback to a full body or another tier.
Every launcher record is written only after every file verifies, and an offered
version older than the installed one is refused rather than rolled back.

`--quality` on `install` still selects a body the channel manifest serves. Use
`kuro pack` for a tier the channel does not serve, and `kuro update` for a
pack-document version update.

Switching CN ⇄ Bilibili (`c` on the game page) compares the two channels'
manifests at the installed version, downloads the files that differ, and
deletes files only the old channel has. A version skew is refused with a
"sync first" message rather than downgrading the install.

### TUI

The TUI is a drill-down: a **games list**, then one game per page. `Esc` backs
out. Only the open game's status and activity are on screen, so PGR and WuWa are
never compared side by side. There is no log panel.

**Games list**

| Key | Action |
|-----|--------|
| `↑` / `↓` (or `j` / `k`) | select a game |
| `Enter` | open the selected game |
| `i` | install the highlighted game (`↑↓` select · `←→` change server/pack · Enter edits folder or installs) |
| `r` | refresh every game's status |
| `h` / `?` | help overlay |
| `q` / `Esc` | quit |

**Game detail**

| Key | Action |
|-----|--------|
| `Esc` | back to the games list (a running task keeps going) |
| `p` | play |
| `d` | predownload (pack document for WuWa CN/Bilibili, base index otherwise) |
| `a` | apply the downloaded pack predownload, or the base predownload on PGR / WuWa Global |
| `s` | sync / repair |
| `c` | checkout server (CN ⇄ Bilibili) |
| `k` | force-kill a hung client |
| `Q` | quality preset (`s`/`h`/`u` pick · `i` install · `x` remove · Enter pick + install · saved for `p`) |
| `r` | refresh this game's status |
| `Tab` | cycle focus: status → activity |
| `q` | quit |

Each game has its own task, so WuWa and PGR can install at the same time;
the games list shows each row's progress and the activity panel follows the
selected row. `Ctrl+C` stops the selected game's task, `q` quits all of them,
and `Esc` backs out while they keep running. Stop waits for the task and every
chunk download to be gone before the panel says `stopped`; partial files stay
on disk and the next run continues from them.

### Cleaning packs

`kuro pack FOLDER hd --remove` deletes `Client/Content/HD` and clears the two
launcher records. In the TUI, `Q` opens the quality panel and `x` removes the
highlighted installed tier after a second `x` confirmation. Removing the tier
that is the saved preset also clears the preset, so `play` falls back to an
installed one.

### Debug log

The TUI has no log section. `--debug` or `KURO_DEBUG=1` appends events to
`$XDG_STATE_HOME/kuro/kuro.log` (usually `~/.local/state/kuro/kuro.log`), one
line per event. `KURO_DEBUG_LOG` overrides the path. Without debug mode,
nothing is buffered and no log file is opened.

### Running the game

kuro installs **game files only** and launches them itself — the official
launcher doesn't run under wine, and dropping the game into Steam as a
non-Steam shortcut loses the `-krqlv=` argument the client now demands:

```sh
kuro play <folder>              # saved preset / what is on disk / HD
kuro play <folder> --quality hd # this run only
kuro kill <folder>              # a client that hung on exit
```

It runs under Proton (GE or dwproton, whichever `$HOME` holds) and falls back
to `umu-run`. Overrides: `KURO_PROTON` (a directory containing `proton`),
`KURO_PREFIX`, `STEAM_ROOT`. Only WuWa takes a tier argument; PGR launches
plain. First start is slow (shader compile); updates come from kuro, not Steam.

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
cargo run -p kuro-core --example status -- <folder>   # live status, one install
```

Formatting and lints are gates, not advice — both CI workflows run them before
anything is published, so a PR that fails either never reaches a release:

```sh
cargo fmt --check                                        # rustfmt.toml: width 120
cargo clippy --workspace --all-targets -- -D warnings
```

Neither uses `--all`: `vendor/hdiffpatch-rs` is a path dependency, not a
workspace member, and its two dead-code warnings are not ours to fix.

## Known limitations

- PGR launcher tokens rotate and are issued by Kuro's private launcher SDK at
  runtime — kuro cannot fetch them (yet), so both PGR servers need a token you
  supply yourself (`~/.config/kuro/tokens.toml`, see Tokens). PGR CN's CDN host
  is derived from the shared launcher platform, not yet confirmed against a
  live session
- ACE anti-cheat doesn't run under Proton (see above)
- The pack update path needs a `patchConfig` entry matching the installed
  version. If the channel has not published one, kuro refuses rather than
  downloading a full body or switching tiers. The nested `predownload` block
  is consumed only once the formal packs are current, and a preinstall plan is
  applied only on an explicit `a` / `--apply`, with a warning that the version
  is not formal yet.
- `kuro pack` installs one tier at a time; nothing switches between installed
  tiers on disk. Each tier is a disjoint ~20–62 GiB body, so keeping more than
  one installed is a disk-space decision, and the `-krqlv=` argument is what
  picks which is mounted
- PGR has no quality packs to choose from: it is a Unity title whose assets live
  under `PGR_Data/` and whose client takes no `-krqlv` tier, so `--quality` is
  refused for it rather than pretended at
- The CDN manifest describes the base client only, so it is **not** an inventory
  of a WuWa install: the game's own resource channel (`Client/Saved/**`, i.e.
  Video/Lang/Resource packs — not the `Client/Content/Paks` base paks) is
  fetched by the client itself and is invisible to Kuro. The one exception is
  the launcher's *pack* document: since 3.8.0 a tier's manifest also lists its
  optional map/event packs under `Client/Saved/Resources/<version>/`, and the
  pack install/update writes those. The client's own Resource/Lang/Video
  channel is still fetched by the game.
- WuWa CN hotfixes (e.g. 3.6.0 → 3.6.1) have no `krpdiff` groups and no
  `fromFolder`: `predownload` stages the changed files as full files, and
  `apply` refuses to record the new version unless every target file is already
  on disk at its target hash — use `sync` for those, which downloads and
  verifies them directly

## License

MIT — see [LICENSE](LICENSE).
