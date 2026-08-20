//! `kuro` — ratatui terminal UI.
//!
//! Keys: `r` refresh status · `d` predownload · `a` apply · `s` sync ·
//! `c` checkout CN<->bilibili · `q` quit

use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{init, restore, Frame, Terminal};

use kuro_core::{default_game_dir, detect_steam, Game, GameManager, GameStatus, ProgressEvent, Server, SteamInfo};

/// Default game folder (the user's known install).
const DEFAULT_GAME_DIR: &str = "/home/vedaru/.local/share/Steam/steamapps/common/Wuthering Waves";

enum UiEvent {
    /// Status result for one game (index into `UiState::statuses`).
    Status(usize, Result<GameStatus, String>),
    Progress(ProgressEvent),
    TaskDone(Result<String, String>),
}

#[derive(Default)]
struct TaskUi {
    kind: String,
    total: usize,
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
    status: Option<Result<GameStatus, String>>,
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
}

impl InstallDraft {
    fn new(target: String) -> Self {
        Self {
            game: Game::WuWa,
            server: Server::Cn,
            target,
            edit_target: false,
        }
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
        Some("install") => {
            return cli_install(&args).await;
        }
        Some("status") => {
            let folder = args.get(1).cloned().unwrap_or_else(|| DEFAULT_GAME_DIR.to_string());
            return cli_status(&folder).await;
        }
        Some("sync") => {
            let folder = args.get(1).cloned().unwrap_or_else(|| DEFAULT_GAME_DIR.to_string());
            return cli_sync(&folder).await;
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

    match GameManager::install(game, server, folder.into()).await {
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

async fn cli_sync(folder: &str) -> std::io::Result<()> {
    match GameManager::open(PathBuf::from(folder)).await {
        Ok(m) => match m.sync().await {
            Ok(r) => println!(
                "checked={} ok={} repaired={} failed={}",
                r.checked,
                r.ok,
                r.repaired,
                r.failed.len()
            ),
            Err(e) => println!("sync error: {e}"),
        },
        Err(e) => println!("open error: {e}"),
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
                let mut start_install: Option<(Game, Server, String)> = None;
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
                        KeyCode::Char('p') => draft.game = Game::Pgr,
                        KeyCode::Char('c') => draft.server = Server::Cn,
                        KeyCode::Char('b') => draft.server = Server::Bilibili,
                        KeyCode::Char('g') => draft.server = Server::Global,
                        KeyCode::Char('s') => {
                            if let Some(steam) = &state.steam {
                                draft.target =
                                    default_game_dir(steam, draft.game).to_string_lossy().into_owned();
                            }
                        }
                        KeyCode::Char('t') => draft.edit_target = true,
                        KeyCode::Enter => {
                            start_install =
                                Some((draft.game, draft.server, draft.target.clone()));
                        }
                        KeyCode::Esc => state.install = None,
                        _ => {}
                    }
                    // stay in the modal unless Enter was pressed
                    if start_install.is_none() {
                        continue;
                    }
                }
                if let Some((game, server, target)) = start_install {
                    state.install = None;
                    if !state.busy {
                        state.busy = true;
                        state.task = Some(TaskUi {
                            kind: "install".into(),
                            ..Default::default()
                        });
                        push_log(
                            &mut state,
                            format!("installing {} ({}) into {target}", pretty_game(game), server),
                        );
                        spawn_install(&tx, &target, game, server);
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

/// Find installed Kuro games: the standard Steam folders (any library) whose
/// launcher config marks them as real installs.
fn auto_detect_games() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(steam) = detect_steam() else {
        return out;
    };
    for game in [Game::WuWa, Game::Pgr] {
        let name = match game {
            Game::WuWa => "Wuthering Waves",
            Game::Pgr => "Punishing Gray Raven",
        };
        for lib in &steam.libraries {
            let dir = lib.join("common").join(name);
            if dir.join("launcherDownloadConfig.json").is_file() {
                out.push(dir.to_string_lossy().into_owned());
                break; // one entry per game
            }
        }
    }
    out
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

fn spawn_install(tx: &tokio::sync::mpsc::Sender<UiEvent>, path: &str, game: Game, server: Server) {
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
        let result = match GameManager::install_with_progress(game, server, PathBuf::from(path), Some(ptx)).await {
            Ok(r) => {
                let exe = r
                    .game_exe
                    .map(|e| format!(" — exe: {e}"))
                    .unwrap_or_default();
                Ok(format!(
                    "install complete — {} v{} (game files){exe}",
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
                    let report = mgr.apply().await.map_err(|e| e.to_string())?;
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
                        "sync complete — {} repaired ({:.1} GiB), {} ok{failed}",
                        report.repaired,
                        report.repaired_bytes as f64 / (1 << 30) as f64,
                        report.ok
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
    // stacked sections: header / status / task / log / footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),      // header
            Constraint::Length(9),      // status: one box per game
            Constraint::Percentage(45), // task: overall + per-file bars
            Constraint::Min(4),         // log
            Constraint::Length(1),      // footer
        ])
        .split(f.area());

    let title = Line::from(vec![
        Span::styled("kuro", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw(" — Kuro Games launcher (native, no wine)"),
    ]);
    f.render_widget(Paragraph::new(title).block(Block::default().borders(Borders::ALL)), chunks[0]);

    // ---- status section: separate box per game ----
    let n = state.paths.len().max(1);
    if n > 1 {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(
                std::iter::repeat(Constraint::Ratio(1, n as u32))
                    .take(n)
                    .collect::<Vec<_>>(),
            )
            .split(chunks[1]);
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
                Paragraph::new(status_box_lines(s, active && state.busy))
                    .wrap(Wrap { trim: true })
                    .block(border),
                *col,
            );
        }
    } else {
        let s = state.statuses.get(0).and_then(|x| x.as_ref());
        f.render_widget(
            Paragraph::new(status_box_lines(s, state.busy))
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
            chunks[1],
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
                v.push(Line::raw(bar_line("overall", t.done_bytes, t.total_bytes, 34)));
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
        chunks[2],
    );

    // ---- log panel: wrap + scroll window (PgUp/PgDn) ----
    let all_logs: Vec<Line> = state.logs.iter().rev().take(200).map(|l| Line::raw(l)).collect();
    let max_scroll = all_logs.len().saturating_sub(1);
    let scroll = state.log_scroll.min(max_scroll);
    let log_lines: Vec<Line> = all_logs.iter().skip(scroll).take(60).cloned().collect();
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
        chunks[3],
    );

    let footer = format!(
        "{} | r: refresh  s: sync  h: help  q: quit{}",
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
    f.render_widget(Paragraph::new(Line::raw(footer)), chunks[4]);

    // overlays: help / install modal
    if state.show_help {
        let help_lines: Vec<Line> = vec![
            Line::raw("kuro — Kuro Games launcher (native, no wine)"),
            Line::raw(""),
            Line::raw("  keys:"),
            Line::raw("    r        refresh status            d   predownload update"),
            Line::raw("    a        apply update              s   sync / repair files"),
            Line::raw("    c        checkout server (CN<->B)  i   install a new game"),
            Line::raw("    s        (in install) Steam default target"),
            Line::raw("    t        (in install) edit target path"),
            Line::raw("    Tab      cycle focus (status/task/log)"),
            Line::raw("    ←/→      switch game (multi-folder)"),
            Line::raw("    ↑/↓ PgUp/PgDn scroll log (log focused) h/?  this help"),
            Line::raw("    q        quit"),
            Line::raw(""),
            Line::raw("  install a new game (also available via 'i'):"),
            Line::raw("    kuro install wuwa cn ~/Games/WutheringWaves"),
            Line::raw("    kuro install pgr global ~/PGR"),
            Line::raw("  installs the GAME files only (no launcher — on Linux you"),
            Line::raw("  launch the game .exe via Steam + GE-Proton afterwards)"),
            Line::raw("  installed games are auto-detected in Steam libraries;"),
            Line::raw("  or pass folders: kuro <folder1> <folder2> ...  (Tab switches)"),
            Line::raw(""),
            Line::raw("  press h / ? / Esc to close"),
        ];
        let area = centered_rect(70, 60, f.area());
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(help_lines).block(Block::default().borders(Borders::ALL).title("help")),
            area,
        );
    } else if let Some(draft) = state.install.as_ref() {
        let mut modal_lines: Vec<Line> = vec![
            Line::raw("Install a new game"),
            Line::raw(""),
            Line::raw(if draft.edit_target {
                "  target:  [EDITING — type, Backspace, Enter/Esc when done]"
            } else {
                "  target:"
            }),
            Line::raw(format!("           {}", draft.target)),
            Line::raw(format!(
                "  game:    [w]uwa / [p]gr            -> {}",
                if matches!(draft.game, Game::WuWa) {
                    "wuwa"
                } else {
                    "pgr"
                }
            )),
            Line::raw(format!(
                "  server:  [c]n / [b]ilibili / [g]lobal  -> {}",
                draft.server
            )),
            Line::raw(""),
        ];
        match &state.steam {
            Some(steam) => {
                let proton = steam
                    .proton()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "none".to_string());
                modal_lines.push(Line::raw(format!("  [s]: Steam default ({})", steam.steam_root.display())));
                modal_lines.push(Line::raw(format!("  Steam: {}  Proton: {proton}", steam.steam_root.display())));
            }
            None => {
                modal_lines.push(Line::raw("  Steam: not detected — type the target with [t]"));
            }
        }
        modal_lines.push(Line::raw("  [t]: edit target    Esc: cancel"));
        modal_lines.push(Line::styled(
            "  Enter: start install",
            Style::default().fg(Color::Cyan),
        ));
        modal_lines.push(Line::raw(""));
        modal_lines.push(Line::raw("  installs the GAME files (launcher NOT included)"));
        modal_lines.push(Line::raw("  wuwa ~85 GB / pgr ~64 GB; resumable, md5-verified"));
        modal_lines.push(Line::raw("  afterwards: launch the game .exe via Steam + GE-Proton"));
        let area = centered_rect(75, 55, f.area());
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(modal_lines)
                .wrap(Wrap { trim: true })
                .block(Block::default().borders(Borders::ALL).title("install")),
            area,
        );
    }
}

/// Lines for a per-game status box. `busy` marks the active game's box while
/// a task is running, so "up to date" never lies about in-flight work.
fn status_box_lines(s: Option<&Result<GameStatus, String>>, busy: bool) -> Vec<Line> {
    match s {
        Some(Ok(s)) => vec![
            Line::raw(format!("game:    {}", s.game)),
            Line::raw(format!("server:  {}", s.server)),
            Line::raw(format!("local:   {}", s.local_version.as_deref().unwrap_or("none"))),
            Line::raw(format!("remote:  {}", s.remote_version)),
            if busy {
                Line::styled(
                    "updating…",
                    Style::default().fg(Color::Yellow),
                )
            } else if s.update_available {
                Line::styled("UPDATE AVAILABLE", Style::default().fg(Color::Yellow))
            } else {
                Line::styled("up to date", Style::default().fg(Color::Green))
            },
        ],
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
