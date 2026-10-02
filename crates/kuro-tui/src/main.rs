//! `kuro` — ratatui terminal UI.
//!
//! Keys: `r` refresh status · `p` play (launch) · `d` predownload · `a` apply ·
//! `s` sync · `c` checkout CN<->bilibili · `Q` quality preset · `q` quit

use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{init, restore, Frame, Terminal};

use kuro_core::{
    default_game_dir, detect_game, detect_steam, launch, quality, BodyChoice, Game, GameManager,
    GameStatus, ProgressEvent, Quality, QualityInfo, Server, SteamInfo,
};

/// Default game folder (the user's known install).
const DEFAULT_GAME_DIR: &str = "/home/vedaru/Games/Wuthering Waves";

enum UiEvent {
    /// Status result for one game (index into `UiState::statuses`).
    Status(usize, Result<GameStatus, String>),
    Progress(ProgressEvent),
    TaskDone(Result<String, String>),
}

#[derive(Default)]
struct TaskUi {
    kind: String,
    done: usize,
    /// Items announced but not yet finished (GroupStart - GroupDone).
    queued: usize,
    finished: Option<Result<String, String>>,
    total_bytes: u64,
    done_bytes: u64,
    /// In-flight files (parallel downloads), each with its own bar.
    files: Vec<FileState>,
}

#[derive(Clone, Default)]
struct FileState {
    name: String,
    done: u64,
    total: u64,
}

/// Which section the Tab-focus is on; highlighted border + section-scoped keys
/// (PgUp/PgDn scroll the log only when it is focused).
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Focus {
    #[default]
    Status,
    Task,
    Log,
}

#[derive(Default)]
struct UiState {
    logs: Vec<String>,
    task: Option<TaskUi>,
    busy: bool,
    /// Game folders in the manager; ←/→ switches between them.
    paths: Vec<String>,
    active: usize,
    /// Focused section (Tab cycles; PgUp/PgDn only scroll the log when focused).
    focus: Focus,
    /// Log panel scroll offset (lines from the newest).
    log_scroll: usize,
    /// Open install modal (game + server selection).
    install: Option<InstallDraft>,
    /// Help overlay open.
    show_help: bool,
    /// Detected Steam + Proton (for install targets).
    steam: Option<SteamInfo>,
    /// Cached status per game path.
    statuses: Vec<Option<Result<GameStatus, String>>>,
    /// Saved quality preset per game path (mirrors `.kuro_cache/quality.json`).
    quality_sel: Vec<Option<Quality>>,
    /// Open quality-preset modal.
    quality_modal: Option<QualityDraft>,
}

/// In-progress quality selection: what to write, plus the install's packs.
#[derive(Clone)]
struct QualityDraft {
    game: Game,
    choice: Quality,
    info: QualityInfo,
}

/// In-progress install selection.
#[derive(Clone)]
struct InstallDraft {
    game: Game,
    server: Server,
    /// Install target folder (editable).
    target: String,
    /// True while typing the target path.
    edit_target: bool,
    /// Which quality pack's body to fetch. WuWa only — PGR (Unity) has no
    /// `Client/Content/<TIER>` bodies, so it stays on `All`.
    body: BodyChoice,
}

impl InstallDraft {
    fn new(target: String) -> Self {
        Self {
            game: Game::WuWa,
            server: Server::Cn,
            target,
            edit_target: false,
            body: BodyChoice::All,
        }
    }

    /// Cycle the pack this install fetches: all → sd → hd → uhd → all.
    ///
    /// PGR has no bodies at all, so there is nothing to cycle and the choice
    /// stays `All` — picking one would only earn a manifest that cannot serve
    /// it (see `body_choice`).
    fn cycle_body(&mut self) {
        if !self.game.uses_quality_tiers() {
            self.body = BodyChoice::All;
            return;
        }
        self.body = match self.body {
            BodyChoice::All | BodyChoice::Prefer(_) => BodyChoice::Only(Quality::Sd),
            BodyChoice::Only(Quality::Sd) => BodyChoice::Only(Quality::Hd),
            BodyChoice::Only(Quality::Hd) => BodyChoice::Only(Quality::Uhd),
            BodyChoice::Only(Quality::Uhd) => BodyChoice::All,
        };
    }
}

/// How a body choice reads in the UI.
fn body_label(body: BodyChoice) -> String {
    match body {
        BodyChoice::All => "all (whatever the channel serves)".to_string(),
        BodyChoice::Only(q) | BodyChoice::Prefer(q) => q.as_arg().to_ascii_lowercase(),
    }
}

fn push_log(state: &mut UiState, msg: impl Into<String>) {
    let msg = msg.into();
    state.logs.push(msg);
    if state.logs.len() > 200 {
        state.logs.drain(0..state.logs.len() - 200);
    }
}

/// Human-friendly game name for logs.
fn pretty_game(game: Game) -> &'static str {
    match game {
        Game::WuWa => "Wuthering Waves",
        Game::Pgr => "Punishing: Gray Raven",
    }
}

/// Turn known raw errors into friendly, human-readable messages.
fn friendly_error(raw: &str) -> String {
    if raw.contains("no channel files could be swapped") || raw.contains("checkout:") {
        "checkout isn't possible for this server — its channel files aren't in the manifest; your install is unchanged".to_string()
    } else if raw.contains("patchConfig entry") {
        "already on the latest version — nothing to do".to_string()
    } else if raw.contains("run predownload first") || raw.contains("incremental_download") {
        "no downloaded update found — press 'd' first to download it".to_string()
    } else if raw.contains("NoLocalConfig") || raw.contains("launcher config not found") {
        "no game found in this folder (launcherDownloadConfig.json missing)".to_string()
    } else if raw.contains("global fast-switch") {
        "the global server can't be fast-switched — use 's' to sync instead".to_string()
    } else if raw.contains("already up to date") {
        "already up to date".to_string()
    } else {
        raw.to_string()
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // one-shot CLI subcommands
    match args.first().map(|s| s.as_str()) {
        // Answer these before touching the terminal: without a TTY the TUI can
        // neither init nor read a keypress to quit, so falling through here is
        // a hard panic instead of help.
        Some("--help" | "-h" | "help") => {
            print_usage();
            return Ok(());
        }
        Some("--version" | "-V") => {
            println!("kuro {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("install") => {
            return cli_install(&args).await;
        }
        Some("status") => {
            let folder = args.get(1).cloned().unwrap_or_else(|| DEFAULT_GAME_DIR.to_string());
            return cli_status(&folder).await;
        }
        Some("sync") => {
            return cli_sync(&args).await;
        }
        Some("quality") => {
            return cli_quality(&args);
        }
        Some("play") => {
            return cli_play(&args);
        }
        Some("kill") => {
            return cli_kill(&args);
        }
        _ => {}
    }

    // TUI: one or more game folders (default: auto-detect installed games)
    let mut paths = args;
    if paths.is_empty() {
        paths = auto_detect_games();
    }
    if paths.is_empty() {
        paths.push(DEFAULT_GAME_DIR.to_string());
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<UiEvent>(64);

    // initial status for every game
    for (idx, path) in paths.iter().enumerate() {
        let tx = tx.clone();
        let path = path.clone();
        tokio::spawn(async move {
            let result = match GameManager::open(PathBuf::from(path)).await {
                Ok(m) => m.status().await.map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(UiEvent::Status(idx, result)).await;
        });
    }

    let terminal = init();
    let res = run(terminal, &mut rx, tx, paths).await;
    restore();
    res
}

fn print_usage() {
    println!(
        "kuro {ver} — Kuro Games launcher (Wuthering Waves / Punishing: Gray Raven)

usage: kuro [game-folder ...]            launch the TUI (default: auto-detect)
       kuro install <wuwa|pgr> <cn|bilibili|global> <folder> [--quality sd|hd|uhd]
       kuro status [folder]
       kuro sync [folder] [--quality sd|hd|uhd]
       kuro quality <folder> [sd|hd|uhd]
       kuro play <folder> [--quality <sd|hd|uhd>]
       kuro kill <folder>
       kuro help | --help | -h
       kuro --version | -V",
        ver = env!("CARGO_PKG_VERSION")
    );
}

/// Resolve `--quality <sd|hd|uhd>` (or the bare positional form) into the body
/// choice an install / sync should make.
///
/// Games without quality packs (PGR) have no `Client/Content/<TIER>` bodies at
/// all, so asking for one there is a mistake rather than a no-op: say so and
/// stop, instead of quietly downloading the whole client.
fn body_choice(game: Option<Game>, rest: &[String]) -> Result<BodyChoice, String> {
    let asked = parse_quality_arg(rest)?;
    let Some(raw) = asked else {
        return Ok(BodyChoice::All);
    };
    if game.is_some_and(|g| !g.uses_quality_tiers()) {
        let name = game.map(pretty_game).unwrap_or("this game");
        return Err(format!(
            "{name} has no quality packs — drop --quality (its manifest has no Client/Content/<SD|HD|UHD> bodies)"
        ));
    }
    match Quality::parse(raw) {
        Some(q) => Ok(BodyChoice::Only(q)),
        None => Err(format!("unknown quality `{raw}` (use sd|hd|uhd)")),
    }
}

async fn cli_install(args: &[String]) -> std::io::Result<()> {
    let game = match args.get(1).map(|s| s.as_str()) {
        Some("wuwa") => Game::WuWa,
        Some("pgr") => Game::Pgr,
        _ => {
            println!("usage: kuro install <wuwa|pgr> <cn|bilibili|global> <folder>");
            return Ok(());
        }
    };
    let server = match args.get(2).map(|s| s.as_str()) {
        Some("cn") => Server::Cn,
        Some("bilibili") => Server::Bilibili,
        Some("global") => Server::Global,
        _ => {
            println!("bad server (cn|bilibili|global)");
            return Ok(());
        }
    };
    let Some(folder) = args.get(3) else {
        println!("missing folder");
        return Ok(());
    };
    let body = match body_choice(Some(game), args.get(4..).unwrap_or(&[])) {
        Ok(b) => b,
        Err(msg) => {
            println!("{msg}");
            return Ok(());
        }
    };

    match GameManager::install_with_progress(game, server, folder.into(), body, None).await {
        Ok(r) => println!(
            "installed v{}: checked={} ok={} repaired={} failed={}",
            r.version,
            r.sync.checked,
            r.sync.ok,
            r.sync.repaired,
            r.sync.failed.len()
        ),
        Err(e) => println!("install failed: {e}"),
    }
    Ok(())
}

async fn cli_status(folder: &str) -> std::io::Result<()> {
    match GameManager::open(PathBuf::from(folder)).await {
        Ok(m) => match m.status().await {
            Ok(s) => println!(
                "game={} server={} local={:?} remote={} update_available={}",
                s.game, s.server, s.local_version, s.remote_version, s.update_available
            ),
            Err(e) => println!("status error: {e}"),
        },
        Err(e) => println!("open error: {e}"),
    }
    Ok(())
}

/// `kuro sync [folder] [--quality <sd|hd|uhd>]` — verify + repair, optionally
/// against a body other than the saved preset's.
///
/// With no flag the saved preset stands as the body this install tracks
/// (`BodyChoice::Prefer` from `open`, so an unserved preset falls back to
/// everything); an explicit `--quality` replaces it for this run and, being
/// explicit, fails loudly when the channel does not serve that body.
async fn cli_sync(args: &[String]) -> std::io::Result<()> {
    let (folder, rest) = split_play_args(args);
    let body = match body_choice(None, &rest) {
        Ok(b) => b,
        Err(msg) => {
            println!("{msg}");
            return Ok(());
        }
    };
    match GameManager::open(PathBuf::from(&folder)).await {
        Ok(mut m) => {
            if body != BodyChoice::All {
                m.set_body(body);
            }
            match m.sync().await {
                Ok(r) => println!(
                    "checked={} ok={} repaired={} stale_removed={} failed={}",
                    r.checked,
                    r.ok,
                    r.repaired,
                    r.orphans_removed,
                    r.failed.len()
                ),
                Err(e) => println!("sync error: {e}"),
            }
        }
        Err(e) => println!("open error: {e}"),
    }
    Ok(())
}

/// `kuro quality [folder] [sd|hd|uhd]` — with no preset, print the current
/// choice, what is on disk and the prefix it will launch in; with a preset,
/// save it.
fn cli_quality(args: &[String]) -> std::io::Result<()> {
    let folder = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| DEFAULT_GAME_DIR.to_string());
    let path = std::path::Path::new(&folder);
    let info = quality::info(path);

    let preset = match args.get(2) {
        Some(s) => match Quality::parse(s) {
            Some(q) => Some(q),
            None => {
                println!("unknown quality `{s}` (use sd|hd|uhd)");
                return Ok(());
            }
        },
        None => None,
    };

    let Some(preset) = preset else {
        println!("folder:   {folder}");
        println!(
            "selected: {}",
            info.selected.map(|q| q.as_arg().to_string()).unwrap_or_else(|| "none".to_string())
        );
        for q in Quality::ALL {
            match info.pack(q) {
                Some(p) if p.present => println!(
                    "  {:<3} {:<9} {} paks, {}",
                    q.dir_name(),
                    q.label(),
                    p.files,
                    fmt_bytes(p.bytes)
                ),
                _ => println!("  {:<3} {:<9} not installed", q.dir_name(), q.label()),
            }
        }
        println!("prefix:   {}", launch::resolve_prefix(path).display());
        println!("\nusage: kuro quality <folder> <sd|hd|uhd>  to set");
        println!("       kuro play [folder] [sd|hd|uhd]     to launch");
        return Ok(());
    };

    match quality::set_selected(path, preset) {
        Ok(()) => println!(
            "selected {} — launch with `kuro play \"{folder}\" {}`",
            preset.as_arg(),
            preset.as_arg().to_ascii_lowercase()
        ),
        Err(e) => println!("could not save preset: {e}"),
    }
    Ok(())
}

/// Read an optional quality tier from `play`'s trailing arguments. Both the
/// positional form (`hd`) and the flag forms `--quality hd` and `--quality=hd`
/// are accepted — `--help` advertises the flag, so it has to work, and the
/// positional form is the natural spelling users reach for first.
/// Returns `Err(message)` when `--quality` is given without a value.
fn parse_quality_arg(rest: &[String]) -> Result<Option<&str>, &'static str> {
    let mut found = None;
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        if a == "--quality" {
            match rest.get(i + 1) {
                Some(v) => {
                    found = Some(v.as_str());
                    i += 2;
                }
                None => return Err("--quality needs a value (sd|hd|uhd)"),
            }
        } else if let Some(v) = a.strip_prefix("--quality=") {
            found = Some(v);
            i += 1;
        } else {
            found = Some(a);
            i += 1;
        }
    }
    Ok(found)
}

/// Split a subcommand's arguments into the install folder and the rest (the
/// quality/body flag). Shared by `play` and `sync`. The folder is optional —
/// `kuro play` / `kuro sync` fall back to the default — so this must never
/// index past the end (a bare `args[2..]` panicked on `kuro play`). A leading
/// `--flag` is a flag, not a folder, so `kuro play --quality hd` reads as
/// "default folder, quality hd".
fn split_play_args(args: &[String]) -> (String, Vec<String>) {
    let rest = args.get(1..).unwrap_or(&[]);
    match rest.first() {
        Some(folder) if !folder.starts_with("--") => (folder.clone(), rest[1..].to_vec()),
        _ => (DEFAULT_GAME_DIR.to_string(), rest.to_vec()),
    }
}

/// `kuro play [folder] [--quality <sd|hd|uhd>]` — start the game with kuro's
/// own launcher. The preset defaults to the saved choice (else what is on
/// disk, else HD); a preset whose paks are not installed is refused rather
/// than mounted empty. Games without tiers (PGR) ignore the preset entirely.
fn cli_play(args: &[String]) -> std::io::Result<()> {
    let (folder, rest) = split_play_args(args);
    let path = std::path::Path::new(&folder);
    let game = detect_game(path).unwrap_or(Game::WuWa);
    let info = quality::info(path);

    let quality_arg = match parse_quality_arg(&rest) {
        Ok(q) => q,
        Err(msg) => {
            println!("{msg}");
            return Ok(());
        }
    };
    let quality = match quality_arg {
        Some(s) if game.uses_quality_tiers() => match Quality::parse(s) {
            Some(q) => q,
            None => {
                println!("unknown quality `{s}` (use sd|hd|uhd)");
                return Ok(());
            }
        },
        _ => info.default_choice(),
    };

    if game.uses_quality_tiers() && !info.is_installed(quality) {
        println!(
            "{} paks are not installed in {} — download them or pick another preset",
            quality.as_arg(),
            quality::content_dir(path).join(quality.dir_name()).display()
        );
        return Ok(());
    }

    let plan = match launch::plan(path, game, quality) {
        Ok(p) => p,
        Err(e) => {
            println!("cannot launch: {e}");
            return Ok(());
        }
    };
    println!("{}", plan.describe());
    match launch::spawn(&plan) {
        Ok(pid) => println!("started (pid {pid})"),
        Err(e) => println!("launch failed: {e}"),
    }
    Ok(())
}

/// `kuro kill [folder]` — SIGKILL the game kuro last launched for `folder`.
/// For a client that hangs on shutdown and never lets go of the prefix.
fn cli_kill(args: &[String]) -> std::io::Result<()> {
    let folder = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| DEFAULT_GAME_DIR.to_string());
    match launch::kill(std::path::Path::new(&folder)) {
        Ok(r) if r.was_running => println!("killed game process group {}", r.pid),
        Ok(r) => println!("no running game (group {} already exited)", r.pid),
        Err(e) => println!("kill: {e}"),
    }
    Ok(())
}

async fn run(
    mut terminal: Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    rx: &mut tokio::sync::mpsc::Receiver<UiEvent>,
    tx: tokio::sync::mpsc::Sender<UiEvent>,
    paths: Vec<String>,
) -> std::io::Result<()> {
    let mut state = UiState {
        paths,
        steam: detect_steam(),
        statuses: Vec::new(),
        ..Default::default()
    };
    state.statuses = vec![None; state.paths.len()];
    state.quality_sel = state
        .paths
        .iter()
        .map(|p| quality::selected(std::path::Path::new(p)))
        .collect();

    loop {
        terminal.draw(|f| ui(f, &state))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let path = state.paths[state.active].clone();

                // modal shortcuts take priority
                if state.show_help {
                    match key.code {
                        KeyCode::Char('h') | KeyCode::Char('?') | KeyCode::Esc => {
                            state.show_help = false
                        }
                        KeyCode::Char('q') => break,
                        _ => {}
                    }
                    continue;
                }

                // quality-preset modal
                if state.quality_modal.is_some() {
                    let mut close = false;
                    match key.code {
                        KeyCode::Char('s') => {
                            state.quality_modal.as_mut().unwrap().choice = Quality::Sd
                        }
                        KeyCode::Char('h') => {
                            state.quality_modal.as_mut().unwrap().choice = Quality::Hd
                        }
                        KeyCode::Char('u') => {
                            state.quality_modal.as_mut().unwrap().choice = Quality::Uhd
                        }
                        KeyCode::Enter => {
                            let choice = state.quality_modal.as_ref().unwrap().choice;
                            apply_quality(&mut state, &path, choice);
                            close = true;
                        }
                        KeyCode::Esc => close = true,
                        _ => {}
                    }
                    if close {
                        state.quality_modal = None;
                    }
                    continue;
                }

                let mut start_install: Option<(Game, Server, String, BodyChoice)> = None;
                if let Some(draft) = state.install.as_mut() {
                    if draft.edit_target {
                        // typing the target path
                        match key.code {
                            KeyCode::Char(c) => draft.target.push(c),
                            KeyCode::Backspace => {
                                draft.target.pop();
                            }
                            KeyCode::Enter | KeyCode::Esc => draft.edit_target = false,
                            _ => {}
                        }
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('w') => draft.game = Game::WuWa,
                        KeyCode::Char('p') => {
                            // PGR has no quality packs: whatever was picked for
                            // WuWa must not follow the selection over, or the
                            // install would ask for a body PGR's manifest cannot
                            // serve and fail before downloading anything.
                            draft.game = Game::Pgr;
                            draft.body = BodyChoice::All;
                        }
                        KeyCode::Char('c') => draft.server = Server::Cn,
                        KeyCode::Char('b') => draft.server = Server::Bilibili,
                        KeyCode::Char('g') => draft.server = Server::Global,
                        KeyCode::Char('f') => draft.cycle_body(),
                        KeyCode::Char('s') => {
                            if let Some(steam) = &state.steam {
                                draft.target =
                                    default_game_dir(steam, draft.game).to_string_lossy().into_owned();
                            }
                        }
                        KeyCode::Char('t') => draft.edit_target = true,
                        KeyCode::Enter => {
                            start_install =
                                Some((draft.game, draft.server, draft.target.clone(), draft.body));
                        }
                        KeyCode::Esc => state.install = None,
                        _ => {}
                    }
                    // stay in the modal unless Enter was pressed
                    if start_install.is_none() {
                        continue;
                    }
                }
                if let Some((game, server, target, body)) = start_install {
                    state.install = None;
                    if !state.busy {
                        state.busy = true;
                        state.task = Some(TaskUi {
                            kind: "install".into(),
                            ..Default::default()
                        });
                        push_log(
                            &mut state,
                            match body {
                                BodyChoice::All => {
                                    format!("installing {} ({}) into {target}", pretty_game(game), server)
                                }
                                _ => format!(
                                    "installing {} ({}) into {target} — quality pack {}",
                                    pretty_game(game),
                                    server,
                                    body_label(body)
                                ),
                            },
                        );
                        spawn_install(&tx, &target, game, server, body);
                    }
                }

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('h') | KeyCode::Char('?') => state.show_help = true,
                    KeyCode::Char('i') => {
                        if !state.busy {
                            state.install =
                                Some(InstallDraft::new(state.paths[state.active].clone()));
                        }
                    }
                    KeyCode::Char('p') => {
                        if !state.busy {
                            play(&mut state, &path);
                        }
                    }
                    KeyCode::Char('k') => kill_game(&mut state, &path),
                    KeyCode::Char('Q') => {
                        if !state.busy {
                            let game = active_game(&state);
                            if game.uses_quality_tiers() {
                                state.quality_modal = Some(open_quality_draft(&path, &state));
                            } else {
                                push_log(
                                    &mut state,
                                    format!(
                                        "{} has no quality tiers — it launches as-is",
                                        pretty_game(game)
                                    ),
                                );
                            }
                        }
                    }
                    KeyCode::Tab => {
                        state.focus = match state.focus {
                            Focus::Status => Focus::Task,
                            Focus::Task => Focus::Log,
                            Focus::Log => Focus::Status,
                        };
                    }
                    KeyCode::Left => switch_game(&mut state, &tx, -1),
                    KeyCode::Right => switch_game(&mut state, &tx, 1),
                    KeyCode::Char('r') => {
                        if !state.busy {
                            spawn_status(&tx, &path, state.active);
                        }
                    }
                    KeyCode::Char('d') => {
                        if !state.busy {
                            state.busy = true;
                            state.task = Some(TaskUi {
                                kind: "predownload".into(),
                                ..Default::default()
                            });
                            state.focus = Focus::Task;
                            spawn_predownload(&tx, &path);
                        }
                    }
                    KeyCode::Char('a') => {
                        if !state.busy {
                            state.busy = true;
                            state.task = Some(TaskUi {
                                kind: "apply".into(),
                                ..Default::default()
                            });
                            state.focus = Focus::Task;
                            spawn_simple(&tx, &path, TaskKind::Apply);
                        }
                    }
                    KeyCode::Char('s') => {
                        if !state.busy {
                            state.busy = true;
                            state.task = Some(TaskUi {
                                kind: "sync".into(),
                                ..Default::default()
                            });
                            state.focus = Focus::Task;
                            spawn_simple(&tx, &path, TaskKind::Sync);
                        }
                    }
                    KeyCode::Char('c') => {
                        if !state.busy {
                            state.busy = true;
                            state.task = Some(TaskUi {
                                kind: "checkout".into(),
                                ..Default::default()
                            });
                            state.focus = Focus::Task;
                            spawn_simple(&tx, &path, TaskKind::Checkout);
                        }
                    }
                    KeyCode::Up => {
                        if state.focus == Focus::Log {
                            state.log_scroll += 1;
                        }
                    }
                    KeyCode::Down => {
                        if state.focus == Focus::Log {
                            state.log_scroll = state.log_scroll.saturating_sub(1);
                        }
                    }
                    KeyCode::PageUp => {
                        if state.focus == Focus::Log {
                            state.log_scroll += 10;
                        }
                    }
                    KeyCode::PageDown => {
                        if state.focus == Focus::Log {
                            state.log_scroll = state.log_scroll.saturating_sub(10);
                        }
                    }
                    _ => {}
                }
            }
        }

        while let Ok(ev) = rx.try_recv() {
            match ev {
                UiEvent::Status(idx, s) => {
                    let line = match &s {
                        Ok(gs) => {
                            let local = gs.local_version.as_deref().unwrap_or("not installed");
                            let state = if gs.update_available {
                                "update available"
                            } else {
                                "up to date"
                            };
                            format!(
                                "status: {} ({}) — local {local}, remote {} · {state}",
                                pretty_game(gs.game),
                                gs.server,
                                gs.remote_version
                            )
                        }
                        Err(e) => format!("status error: {}", friendly_error(e)),
                    };
                    push_log(&mut state, line);
                    if idx < state.statuses.len() {
                        state.statuses[idx] = Some(s);
                    }
                }
                UiEvent::Progress(p) => match p {
                    ProgressEvent::Log(m) => push_log(&mut state, m),
                    ProgressEvent::SetTotal { bytes } => {
                        if let Some(t) = state.task.as_mut() {
                            t.total_bytes = bytes;
                            t.files.clear(); // repair phase starts; drop the verify bar
                        }
                    }
                    ProgressEvent::SetQueued { count } => {
                        if let Some(t) = state.task.as_mut() {
                            t.queued = count;
                        }
                    }
                    ProgressEvent::GroupStart { name: _ } => {
                        if let Some(t) = state.task.as_mut() {
                            // queued for download; a row appears once it starts
                            t.queued += 1;
                        }
                    }
                    ProgressEvent::FileProgress { name, bytes, total } => {
                        if let Some(t) = state.task.as_mut() {
                            match t.files.iter_mut().find(|f| f.name == name) {
                                Some(f) => {
                                    f.done = bytes;
                                    f.total = total;
                                }
                                None => t.files.push(FileState { name, done: bytes, total }),
                            }
                        }
                    }
                    ProgressEvent::GroupDone { name, bytes } => {
                        if let Some(t) = state.task.as_mut() {
                            t.files.retain(|f| f.name != name);
                            t.done += 1;
                            t.done_bytes += bytes;
                            t.queued = t.queued.saturating_sub(1);
                        }
                    }
                    ProgressEvent::Done => {
                        if let Some(t) = state.task.as_mut() {
                            t.finished = Some(Ok("download complete — press 'a' to apply".into()));
                        }
                        state.busy = false;
                    }
                },
                UiEvent::TaskDone(r) => {
                    match &r {
                        Ok(msg) => push_log(&mut state, msg.clone()),
                        Err(e) => push_log(&mut state, format!("failed: {}", friendly_error(e))),
                    }
                    if let Some(t) = state.task.as_mut() {
                        t.finished = Some(r);
                    }
                    state.busy = false;
                }
            }
        }
    }
    Ok(())
}

fn spawn_status(tx: &tokio::sync::mpsc::Sender<UiEvent>, path: &str, idx: usize) {
    let tx = tx.clone();
    let path = path.to_string();
    tokio::spawn(async move {
        let result = match GameManager::open(PathBuf::from(path)).await {
            Ok(m) => m.status().await.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        let _ = tx.send(UiEvent::Status(idx, result)).await;
    });
}

/// Switch the active game (←/→); wraps around, refreshes its status box.
fn switch_game(state: &mut UiState, tx: &tokio::sync::mpsc::Sender<UiEvent>, delta: isize) {
    if state.paths.len() < 2 || state.busy {
        return;
    }
    let n = state.paths.len() as isize;
    state.active = (state.active as isize + delta).rem_euclid(n) as usize;
    if state.active < state.statuses.len() {
        state.statuses[state.active] = None;
    }
    let name = state.paths[state.active]
        .rsplit('/')
        .next()
        .unwrap_or("game");
    push_log(state, format!("→ {name} (game {}/{})", state.active + 1, state.paths.len()));
    spawn_status(tx, &state.paths[state.active], state.active);
}

/// The game behind the active folder, taken from its status (WuWa by default
/// while the status is still loading).
fn active_game(state: &UiState) -> Game {
    state
        .statuses
        .get(state.active)
        .and_then(|s| s.as_ref())
        .and_then(|r| r.as_ref().ok())
        .map(|gs| gs.game)
        .or_else(|| detect_game(std::path::Path::new(&state.paths[state.active])))
        .unwrap_or(Game::WuWa)
}

/// Read the install's packs and saved choice for the modal.
fn open_quality_draft(folder: &str, state: &UiState) -> QualityDraft {
    let p = std::path::Path::new(folder);
    let game = active_game(state);
    let info = quality::info(p);
    let choice = info.default_choice();
    QualityDraft { game, choice, info }
}

/// Save the chosen preset; log it (the writable marker `-krqlv=` is added
/// afresh on every kuro launch, so nothing else needs updating).
fn apply_quality(state: &mut UiState, folder: &str, choice: Quality) {
    match quality::set_selected(std::path::Path::new(folder), choice) {
        Ok(()) => push_log(
            state,
            format!("quality: set {} ({}) — press 'p' to play", choice.as_arg(), choice.label()),
        ),
        Err(e) => push_log(state, format!("quality save failed: {e}")),
    }
    if let Some(slot) = state.quality_sel.get_mut(state.active) {
        *slot = Some(choice);
    }
}

/// Launch the active game with kuro's built-in launcher ('p'). Uses the saved
/// preset (else what is on disk, else HD) and refuses a preset whose paks are
/// not installed rather than mounting an empty directory. Games without tiers
/// (PGR) skip the preset entirely.
fn play(state: &mut UiState, folder: &str) {
    let p = std::path::Path::new(folder);
    let game = active_game(state);
    let info = quality::info(p);
    let quality = state
        .quality_sel
        .get(state.active)
        .copied()
        .flatten()
        .or(info.selected)
        .unwrap_or_else(|| info.default_choice());
    if game.uses_quality_tiers() && !info.is_installed(quality) {
        push_log(state, format!("cannot play: {} paks are not installed", quality.as_arg()));
        return;
    }
    match launch::plan(p, game, quality) {
        Ok(plan) => match launch::spawn(&plan) {
            Ok(pid) => push_log(state, format!("launching {} — pid {pid}", plan.brief())),
            Err(e) => push_log(state, format!("launch failed: {e}")),
        },
        Err(e) => push_log(state, format!("cannot launch: {e}")),
    }
}

/// Force-kill the active game ('k') — SIGKILL to the whole Proton/wine/client
/// process group, for a client that hangs on shutdown.
fn kill_game(state: &mut UiState, folder: &str) {
    match launch::kill(std::path::Path::new(folder)) {
        Ok(r) if r.was_running => {
            push_log(state, format!("force-killed game (group {})", r.pid))
        }
        Ok(r) => push_log(state, format!("no running game (group {} already exited)", r.pid)),
        Err(e) => push_log(state, format!("kill: {e}")),
    }
}

/// Find installed Kuro games: `~/Games` (the non-Steam layout this box uses)
/// plus the standard Steam library folders — anything carrying the official
/// launcher's `launcherDownloadConfig.json` counts as an install.
fn auto_detect_games() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let home = std::env::var("HOME").unwrap_or_default();
    let mut roots: Vec<PathBuf> = Vec::new();
    roots.push(PathBuf::from(&home).join("Games"));
    if let Some(steam) = detect_steam() {
        for lib in &steam.libraries {
            roots.push(lib.join("common"));
        }
    }
    for root in roots {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in rd.flatten() {
            let dir = entry.path();
            if dir.is_dir() && dir.join("launcherDownloadConfig.json").is_file() {
                out.push(dir.to_string_lossy().into_owned());
            }
        }
    }
    // Dedupe by CANONICAL path: `~/Games/<Game>` and a symlink alias
    // (`steamapps/common/<Game>` -> `~/Games/<Game>`) are different strings
    // but the same game — string dedup misses them. Keep the first (real,
    // earlier-root) path, drop the symlink alias.
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut deduped: Vec<String> = Vec::new();
    for p in out {
        let canon = std::fs::canonicalize(&p).unwrap_or_else(|_| PathBuf::from(&p));
        if seen.insert(canon) {
            deduped.push(p);
        }
    }
    deduped.sort();
    deduped
}

fn spawn_predownload(tx: &tokio::sync::mpsc::Sender<UiEvent>, path: &str) {
    let tx = tx.clone();
    let path = path.to_string();
    tokio::spawn(async move {
        let result = async {
            let mgr = GameManager::open(PathBuf::from(path)).await.map_err(|e| e.to_string())?;
            let plan = mgr.plan_predownload().await.map_err(|e| e.to_string())?;
            let (ptx, mut prx) = tokio::sync::mpsc::channel(256);
            let tx2 = tx.clone();
            tokio::spawn(async move {
                while let Some(ev) = prx.recv().await {
                    let _ = tx2.send(UiEvent::Progress(ev)).await;
                }
            });
            let _ = ptx.send(ProgressEvent::SetTotal { bytes: plan.total_bytes }).await;
            mgr.predownload(&plan, ptx).await.map_err(|e| e.to_string())?;
            if plan.total_bytes == 0 {
                Ok::<_, String>("already up to date — nothing to download".to_string())
            } else {
                Ok::<_, String>(format!(
                    "predownload complete — {} → {} staged ({} groups, {} files, {:.1} GiB)",
                    plan.from_version,
                    plan.to_version,
                    plan.patch_groups.len(),
                    plan.full_files.len(),
                    plan.total_bytes as f64 / (1 << 30) as f64
                ))
            }
        }
        .await;
        let _ = tx.send(UiEvent::TaskDone(result)).await;
    });
}

fn spawn_install(
    tx: &tokio::sync::mpsc::Sender<UiEvent>,
    path: &str,
    game: Game,
    server: Server,
    body: BodyChoice,
) {
    let tx = tx.clone();
    let path = path.to_string();
    tokio::spawn(async move {
        let (ptx, mut prx) = tokio::sync::mpsc::channel::<ProgressEvent>(256);
        let tx2 = tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = prx.recv().await {
                let _ = tx2.send(UiEvent::Progress(ev)).await;
            }
        });
        let folder = PathBuf::from(path);
        let result =
            match GameManager::install_with_progress(game, server, folder, body, Some(ptx)).await {
            Ok(r) => {
                let exe = r
                    .game_exe
                    .map(|e| format!(" — exe: {e}"))
                    .unwrap_or_default();
                Ok(format!(
                    "install complete — {} v{} (game files){exe} · press 'p' to play",
                    pretty_game(game),
                    r.version
                ))
            }
            Err(e) => Err(e.to_string()),
        };
        let _ = tx.send(UiEvent::TaskDone(result)).await;
    });
}

enum TaskKind {
    Apply,
    Sync,
    Checkout,
}

fn spawn_simple(tx: &tokio::sync::mpsc::Sender<UiEvent>, path: &str, kind: TaskKind) {
    let tx = tx.clone();
    let path = path.to_string();
    tokio::spawn(async move {
        // progress relay (sync emits per-file progress)
        let (ptx, mut prx) = tokio::sync::mpsc::channel::<ProgressEvent>(256);
        let tx2 = tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = prx.recv().await {
                let _ = tx2.send(UiEvent::Progress(ev)).await;
            }
        });
        let mut ptx = Some(ptx);
        let result = async {
            let mgr = GameManager::open(PathBuf::from(path)).await.map_err(|e| e.to_string())?;
            match kind {
                TaskKind::Apply => {
                    let report = mgr
                        .apply_with_progress(ptx.take())
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok(format!(
                        "apply complete — merged {}, skipped {}, fallback {}, swapped {}, deleted {}",
                        report.merged,
                        report.skipped,
                        report.fallback,
                        report.swapped,
                        report.deleted.len()
                    ))
                }
                TaskKind::Sync => {
                    let report = mgr
                        .sync_with_progress(ptx.take())
                        .await
                        .map_err(|e| e.to_string())?;
                    let failed = if report.failed.is_empty() {
                        String::new()
                    } else {
                        format!(", {} failed", report.failed.len())
                    };
                    Ok(format!(
                        "sync complete — {} repaired ({:.1} GiB), {} ok, {} stale removed{failed}",
                        report.repaired,
                        report.repaired_bytes as f64 / (1 << 30) as f64,
                        report.ok,
                        report.orphans_removed
                    ))
                }
                TaskKind::Checkout => {
                    // toggle between cn and bilibili
                    let target = match mgr.server {
                        Server::Cn => Server::Bilibili,
                        _ => Server::Cn,
                    };
                    let report = mgr.checkout(target).await.map_err(|e| e.to_string())?;
                    Ok(format!(
                        "checkout complete — {} → {}, {} files swapped, now v{}",
                        report.from_server, report.to_server, report.swapped_files, report.new_version
                    ))
                }
            }
        }
        .await;
        let _ = tx.send(UiEvent::TaskDone(result)).await;
    });
}

fn ui(f: &mut Frame, state: &UiState) {
    // stacked sections: status / task / log / footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9),      // status: one box per game
            Constraint::Percentage(45), // task: overall + per-file bars
            Constraint::Min(4),         // log
            Constraint::Length(1),      // footer
        ])
        .split(f.area());

    // ---- status section: separate box per game ----
    let n = state.paths.len().max(1);
    if n > 1 {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(std::iter::repeat_n(Constraint::Ratio(1, n as u32), n).collect::<Vec<_>>())
            .split(chunks[0]);
        for (i, col) in cols.iter().enumerate() {
            let path = &state.paths[i];
            let active = i == state.active;
            let s = state.statuses.get(i).and_then(|x| x.as_ref());
            let name = path.rsplit('/').next().unwrap_or("game").to_string();
            let border = if active {
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("▶ {name}"))
                    .border_style(Style::default().fg(if state.focus == Focus::Status {
                        Color::Yellow
                    } else {
                        Color::Cyan
                    }))
            } else {
                Block::default().borders(Borders::ALL).title(name)
            };
            f.render_widget(
                Paragraph::new(status_box_lines(
                    s,
                    active && state.busy,
                    state.quality_sel.get(i).copied().flatten(),
                ))
                .wrap(Wrap { trim: true })
                .block(border),
                *col,
            );
        }
    } else {
        let s = state.statuses.first().and_then(|x| x.as_ref());
        f.render_widget(
            Paragraph::new(status_box_lines(
                s,
                state.busy,
                state.quality_sel.first().copied().flatten(),
            ))
                .wrap(Wrap { trim: true })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("status")
                        .border_style(Style::default().fg(if state.focus == Focus::Status {
                            Color::Yellow
                        } else {
                            Color::Reset
                        })),
                ),
            chunks[0],
        );
    }

    // ---- task section: overall bar + one bar per in-flight file ----
    let task_lines: Vec<Line> = match &state.task {
        None => vec![Line::raw("idle")],
        Some(t) => {
            let mut v = vec![
                Line::raw(format!("task: {}", t.kind)),
                Line::raw(format!("files done: {}   queued: {}", t.done, t.queued)),
            ];
            if t.total_bytes > 0 {
                // Include in-flight bytes: `done_bytes` only moves on GroupDone
                // (a whole file), so the overall bar used to sit still for the
                // entire duration of one file and jump when it landed.
                let in_flight: u64 = t.files.iter().map(|f| f.done).sum();
                let shown = (t.done_bytes + in_flight).min(t.total_bytes);
                v.push(Line::raw(bar_line("overall", shown, t.total_bytes, 34)));
            }
            for f in t.files.iter().take(8) {
                let name = shorten(&f.name, 42);
                v.push(Line::raw(format!("  {}", bar_line(&name, f.done, f.total, 24))));
            }
            if let Some(f) = &t.finished {
                v.push(Line::styled(
                    match f {
                        Ok(m) => format!("✔ {m}"),
                        Err(e) => format!("✘ {}", friendly_error(e)),
                    },
                    Style::default().fg(match f {
                        Ok(_) => Color::Green,
                        Err(_) => Color::Red,
                    }),
                ));
            }
            v
        }
    };
    f.render_widget(
        Paragraph::new(task_lines)
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("task")
                    .border_style(Style::default().fg(if state.focus == Focus::Task {
                        Color::Yellow
                    } else {
                        Color::Reset
                    })),
            ),
        chunks[1],
    );

    // ---- log panel: wrap + scroll window (PgUp/PgDn) ----
    // Build only the 60 rows on screen. Materialising the whole 200-line window
    // and cloning it every frame was work the Paragraph never sees.
    let max_scroll = state.logs.len().min(200).saturating_sub(1);
    let scroll = state.log_scroll.min(max_scroll);
    let log_lines: Vec<Line> = state
        .logs
        .iter()
        .rev()
        .take(200)
        .skip(scroll)
        .take(60)
        .map(|l| Line::raw(l.as_str()))
        .collect();
    f.render_widget(
        Paragraph::new(log_lines)
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("log (↑/↓ PgUp/PgDn)")
                    .border_style(Style::default().fg(if state.focus == Focus::Log {
                        Color::Yellow
                    } else {
                        Color::Reset
                    })),
            ),
        chunks[2],
    );

    let footer = format!(
        "{} | p: play  k: kill  r: refresh  s: sync  i: install  Q: quality  h: help  q: quit{}",
        if state.paths.len() > 1 {
            format!(
                "Tab: focus  ←/→: game ({}/{})",
                state.active + 1,
                state.paths.len()
            )
        } else {
            "Tab: focus".to_string()
        },
        if state.busy { "   [busy]" } else { "" }
    );
    f.render_widget(Paragraph::new(Line::raw(footer)), chunks[3]);

    // overlays: help / install modal
    if state.show_help {
        let help_lines: Vec<Line> = vec![
            Line::raw("kuro — Kuro Games launcher"),
            Line::raw(""),
            Line::raw("  p play         k force-kill      Q quality"),
            Line::raw("  i install      r refresh         a apply update"),
            Line::raw("  d predownload  s sync / repair   c switch server"),
            Line::raw("  Tab focus   <-/-> game   Up/Down scroll log   q quit"),
            Line::raw(""),
            Line::raw("Install dialog"),
            Line::raw("  w/p game   c/b/g server   f pack   t path   s Steam   Enter go"),
            Line::raw(""),
            Line::raw("From a shell"),
            Line::raw("  kuro install wuwa cn ~/Games/WutheringWaves --quality hd"),
            Line::raw("  kuro sync ~/Games/WutheringWaves --quality uhd"),
            Line::raw("  kuro install pgr global ~/PGR"),
            Line::raw(""),
            Line::raw("h / ? / Esc to close"),
        ];
        let area = centered_rect(70, 60, f.area());
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(help_lines).block(Block::default().borders(Borders::ALL).title("help")),
            area,
        );
    } else if let Some(draft) = state.quality_modal.as_ref() {
        let mut lines: Vec<Line> = vec![
            Line::raw(format!("Quality preset — {}", pretty_game(draft.game))),
            Line::raw(""),
            Line::raw("  Pick the art size kuro mounts at launch."),
            Line::raw(""),
        ];
        for q in Quality::ALL {
            let key = q.as_arg().to_ascii_lowercase();
            let key = key.chars().next().unwrap_or('?');
            let mark = if draft.choice == q { "▶" } else { " " };
            let current = if draft.info.selected == Some(q) {
                "  (current)"
            } else {
                ""
            };
            let status = match draft.info.pack(q) {
                Some(p) if p.present => format!("{} paks, {}", p.files, fmt_bytes(p.bytes)),
                _ => "not installed".to_string(),
            };
            lines.push(Line::styled(
                format!(" {mark} [{key}] {:<9} {:<20}{current}", q.label(), status),
                if draft.choice == q {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default()
                },
            ));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "  [s] [h] [u] choose      Enter apply      Esc cancel",
            Style::default().fg(Color::Cyan),
        ));
        let area = centered_rect(68, 46, f.area());
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title("quality")),
            area,
        );
    } else if let Some(draft) = state.install.as_ref() {
        let mut modal_lines: Vec<Line> = vec![
            Line::raw("Install a new game"),
            Line::raw(""),
            Line::raw(format!(
                "  game    [w] wuwa  [p] pgr                → {}",
                if matches!(draft.game, Game::WuWa) {
                    "wuwa"
                } else {
                    "pgr"
                }
            )),
            Line::raw(format!(
                "  server  [c] cn  [b] bilibili  [g] global  → {}",
                draft.server
            )),
            Line::raw(if draft.game.uses_quality_tiers() {
                format!(
                    "  pack    [f] all / sd / hd / uhd          → {}",
                    body_label(draft.body)
                )
            } else {
                format!("  pack    n/a — {} has no quality packs", pretty_game(draft.game))
            }),
            Line::raw(if draft.edit_target {
                "  target  [typing — Enter/Esc to stop]"
            } else {
                "  target"
            }),
            Line::raw(format!("          {}", draft.target)),
            Line::raw("          [t] type path    [s] Steam default"),
            Line::raw(""),
        ];
        match &state.steam {
            Some(steam) => modal_lines.push(Line::raw(format!(
                "  Steam:  {}",
                steam.steam_root.display()
            ))),
            None => modal_lines.push(Line::raw("  Steam:  not detected")),
        }
        modal_lines.push(Line::raw(""));
        modal_lines.push(Line::styled(
            "  Enter install      Esc cancel",
            Style::default().fg(Color::Cyan),
        ));
        modal_lines.push(Line::raw(""));
        modal_lines.push(Line::raw(
            "  Game files only. `all` takes every pack the channel serves; a single",
        ));
        modal_lines.push(Line::raw(
            "  pack (sd/hd/uhd) fetches just that body — the channel must serve it.",
        ));
        let area = centered_rect(70, 48, f.area());
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(modal_lines)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title("install")),
            area,
        );
    }
}

/// Lines for a per-game status box. `busy` marks the active game's box while
/// a task is running, so "up to date" never lies about in-flight work.
fn status_box_lines(
    s: Option<&Result<GameStatus, String>>,
    busy: bool,
    quality: Option<Quality>,
) -> Vec<Line<'_>> {
    match s {
        Some(Ok(s)) => {
            let mut lines = vec![
                Line::raw(format!("game:    {}", s.game)),
                Line::raw(format!("server:  {}", s.server)),
                Line::raw(format!("local:   {}", s.local_version.as_deref().unwrap_or("none"))),
                Line::raw(format!("remote:  {}", s.remote_version)),
            ];
            // PGR (Unity) takes no `-krqlv=` tier, so it gets no quality line.
            if s.game.uses_quality_tiers() {
                lines.push(Line::raw(format!(
                    "quality: {}",
                    quality
                        .map(|q| format!("{} ({})", q.as_arg(), q.label()))
                        .unwrap_or_else(|| "not set".to_string())
                )));
            }
            lines.push(if busy {
                Line::styled("updating…", Style::default().fg(Color::Yellow))
            } else if s.update_available {
                Line::styled("UPDATE AVAILABLE", Style::default().fg(Color::Yellow))
            } else {
                Line::styled("up to date", Style::default().fg(Color::Green))
            });
            lines
        }
        Some(Err(e)) => vec![Line::styled(
            format!("error: {e}"),
            Style::default().fg(Color::Red),
        )],
        None => vec![Line::raw("loading...")],
    }
}

/// A text progress bar line with a label.
fn bar_line(label: &str, done: u64, total: u64, bar_w: usize) -> String {
    if total == 0 {
        return format!("{label}: ?");
    }
    let ratio = (done as f64 / total as f64).clamp(0.0, 1.0);
    let filled = (bar_w as f64 * ratio).round() as usize;
    format!(
        "[{}{}] {:5.1}%  {}/{}  {label}",
        "█".repeat(filled),
        "░".repeat(bar_w - filled),
        ratio * 100.0,
        fmt_bytes(done),
        fmt_bytes(total),
    )
}

/// Human size with adaptive units (KiB / MiB / GiB).
fn fmt_bytes(b: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = b as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else {
        format!("{:.0} KiB", b / KIB)
    }
}

/// Keep the tail of long file paths (the part that matters).
fn shorten(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        format!("…{}", chars[chars.len() - max + 1..].iter().collect::<String>())
    }
}

/// A centered rectangle for overlays.
fn centered_rect(percent_x: u16, percent_y: u16, area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let popup = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup[1])[1]
}

#[cfg(test)]
mod tests {
    use super::{
        body_choice, parse_quality_arg, split_play_args, ui, InstallDraft, QualityDraft, UiState,
        DEFAULT_GAME_DIR,
    };
    use kuro_core::{quality, BodyChoice, Game, GameStatus, Quality, Server};

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Render the whole UI into a headless buffer and return it as plain text,
    /// so a panel's layout can be eyeballed/asserted without a real terminal.
    pub(super) fn render_to_string(state: &UiState, w: u16, h: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| ui(f, state)).unwrap();
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// Synthetic game folder with one quality set present (HD) and the rest
    /// absent, mirroring a real WuWa install where only HD is downloaded.
    fn fake_wuwa(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kuro-tui-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hd = dir.join("Client/Content/HD");
        std::fs::create_dir_all(&hd).unwrap();
        std::fs::write(hd.join("pakchunk0.pak"), vec![0u8; 4096]).unwrap();
        std::fs::write(hd.join("pakchunk1.pak"), vec![0u8; 8192]).unwrap();
        dir
    }

    #[test]
    fn dump_panels_for_inspection() {
        let dir = fake_wuwa("dump");
        let info = quality::info(&dir);
        let mut state = UiState {
            paths: vec![dir.display().to_string()],
            logs: vec!["status: Wuthering Waves (cn) — up to date".into()],
            statuses: vec![None],
            quality_modal: Some(QualityDraft {
                game: Game::WuWa,
                choice: Quality::Hd,
                info,
            }),
            ..UiState::default()
        };
        println!("=== QUALITY PANEL ===\n{}", render_to_string(&state, 100, 34));

        state.quality_modal = None;
        state.show_help = true;
        println!("=== HELP PANEL ===\n{}", render_to_string(&state, 100, 40));

        state.show_help = false;
        state.install = Some(InstallDraft::new("/tmp/kuro-demo".into()));
        println!("=== INSTALL PANEL ===\n{}", render_to_string(&state, 100, 34));
    }

    /// The help overlay is the densest panel; assert it fits (nothing clipped)
    /// at a typical 100x40 terminal, and that the quality panel spells out the
    /// plain-language state rather than leaking the `-krqlv` flag.
    #[test]
    fn panels_fit_and_read_plainly() {
        let mut state = UiState {
            show_help: true,
            ..UiState::default()
        };
        // 100x34 is the tight case the help must still fit inside.
        let help = render_to_string(&state, 100, 34);
        assert!(help.contains("Esc to close"), "help footer clipped:\n{help}");
        assert!(!help.contains("-krqlv"), "help leaks the raw flag:\n{help}");

        let dir = fake_wuwa("clarity");
        let info = quality::info(&dir);
        state.show_help = false;
        state.quality_modal = Some(QualityDraft {
            game: Game::WuWa,
            choice: Quality::Hd,
            info,
        });
        let q = render_to_string(&state, 100, 34);
        assert!(q.contains("not installed"), "quality panel missing plain status:\n{q}");
        assert!(q.contains("Enter apply"), "quality panel missing action hint:\n{q}");
        assert!(!q.contains("-krqlv"), "quality panel leaks the raw flag:\n{q}");
    }

    /// `kuro play` with no folder used to panic on `&args[2..]`; the folder is
    /// optional and a leading flag is not a folder.
    #[test]
    fn play_args_split_folder_and_flags() {
        // Bare `kuro play`: default folder, nothing else — the panic case.
        let (folder, rest) = split_play_args(&args(&["play"]));
        assert_eq!(folder, DEFAULT_GAME_DIR);
        assert!(rest.is_empty(), "unexpected trailing args: {rest:?}");

        // Folder first, then the tier.
        let (folder, rest) = split_play_args(&args(&["play", "/games/wuwa", "hd"]));
        assert_eq!(folder, "/games/wuwa");
        assert_eq!(rest, vec!["hd"]);

        // A leading flag is not a folder.
        let (folder, rest) = split_play_args(&args(&["play", "--quality", "hd"]));
        assert_eq!(folder, DEFAULT_GAME_DIR);
        assert_eq!(rest, vec!["--quality", "hd"]);
    }

    #[test]
    fn quality_arg_accepts_flag_and_positional_forms() {
        assert_eq!(parse_quality_arg(&args(&[])), Ok(None));
        assert_eq!(parse_quality_arg(&args(&["hd"])), Ok(Some("hd")));
        assert_eq!(parse_quality_arg(&args(&["--quality", "uhd"])), Ok(Some("uhd")));
        assert_eq!(parse_quality_arg(&args(&["--quality=sd"])), Ok(Some("sd")));
        // flag form the old code silently mis-read as the literal tier string
        assert_eq!(parse_quality_arg(&args(&["--quality", "hd"])), Ok(Some("hd")));
    }

    #[test]
    fn quality_arg_rejects_dangling_flag() {
        assert_eq!(
            parse_quality_arg(&args(&["--quality"])),
            Err("--quality needs a value (sd|hd|uhd)")
        );
    }

    /// PGR takes no `-krqlv=` tier, so its box shows no quality line; WuWa's
    /// still does. (PGR used to render a meaningless "quality: not set".)
    #[test]
    fn status_quality_line_is_wuwa_only() {
        let state = |game, server| UiState {
            paths: vec!["/games/x".into()],
            statuses: vec![Some(Ok(GameStatus {
                game,
                server,
                local_version: Some("1.0.0".into()),
                remote_version: "1.0.0".into(),
                update_available: false,
            }))],
            ..UiState::default()
        };

        let pgr = render_to_string(&state(Game::Pgr, Server::Global), 60, 20);
        assert!(!pgr.contains("quality:"), "PGR box shows a quality line:\n{pgr}");

        let wuwa = render_to_string(&state(Game::WuWa, Server::Cn), 60, 20);
        assert!(wuwa.contains("quality: not set"), "WuWa box lost its quality line:\n{wuwa}");
    }

    #[test]
    fn body_choice_maps_the_flag_to_the_pack_an_install_fetches() {
        // No flag: everything the channel serves (the historical behaviour).
        assert_eq!(body_choice(Some(Game::WuWa), &args(&[])), Ok(BodyChoice::All));
        assert_eq!(
            body_choice(Some(Game::WuWa), &args(&["--quality", "hd"])),
            Ok(BodyChoice::Only(Quality::Hd))
        );
        let err = body_choice(Some(Game::WuWa), &args(&["--quality", "medium"])).unwrap_err();
        assert!(err.contains("unknown quality `medium`"), "{err}");
    }

    #[test]
    fn body_choice_refuses_quality_packs_for_pgr() {
        // PGR's manifest has no Client/Content/<TIER> bodies — asking for one
        // must not quietly install the whole client instead.
        let err = body_choice(Some(Game::Pgr), &args(&["--quality", "hd"])).unwrap_err();
        assert!(err.contains("no quality packs"), "{err}");
        assert!(err.contains("Punishing: Gray Raven"), "{err}");
        // Without the flag PGR installs normally.
        assert_eq!(body_choice(Some(Game::Pgr), &args(&[])), Ok(BodyChoice::All));
    }

    #[test]
    fn install_dialog_cycles_packs_and_pgr_has_none() {
        let mut draft = InstallDraft::new("/tmp/kuro-demo".into());
        assert_eq!(draft.body, BodyChoice::All);
        draft.cycle_body();
        assert_eq!(draft.body, BodyChoice::Only(Quality::Sd));
        draft.cycle_body();
        assert_eq!(draft.body, BodyChoice::Only(Quality::Hd));
        draft.cycle_body();
        assert_eq!(draft.body, BodyChoice::Only(Quality::Uhd));
        draft.cycle_body();
        assert_eq!(draft.body, BodyChoice::All);

        // Switching the dialog to PGR resets the pack: PGR can serve none.
        draft.cycle_body();
        draft.game = Game::Pgr;
        draft.cycle_body();
        assert_eq!(draft.body, BodyChoice::All);
    }
}
