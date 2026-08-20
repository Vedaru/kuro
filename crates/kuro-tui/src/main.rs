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
    Status(Result<GameStatus, String>),
    Progress(ProgressEvent),
    TaskDone(Result<String, String>),
}

#[derive(Default)]
struct TaskUi {
    kind: String,
    total: usize,
    done: usize,
    current: Option<String>,
    finished: Option<Result<String, String>>,
}

#[derive(Default)]
struct UiState {
    status: Option<Result<GameStatus, String>>,
    logs: Vec<String>,
    task: Option<TaskUi>,
    busy: bool,
    /// Game folders in the manager; Tab cycles between them.
    paths: Vec<String>,
    active: usize,
    /// Log panel scroll offset (lines from the newest).
    log_scroll: usize,
    /// Open install modal (game + server selection).
    install: Option<InstallDraft>,
    /// Help overlay open.
    show_help: bool,
    /// Detected Steam + Proton (for install targets).
    steam: Option<SteamInfo>,
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

/// Turn known raw errors into friendly, human-readable messages.
fn friendly_error(raw: &str) -> String {
    if raw.contains("no channel files could be swapped") || raw.contains("checkout:") {
        "checkout isn't possible for this server — its channel files aren't in the manifest; your install is unchanged".to_string()
    } else if raw.contains("patchConfig entry") {
        "already on the latest version — nothing to do".to_string()
    } else if raw.contains("run predownload first") || raw.contains("incremental_download") {
        "no downloaded update found — press 'd' first to download it".to_string()
    } else if raw.contains("NoLocalConfig") || raw.contains("launcher config not found") {
        "no game detected in this folder (launcherDownloadConfig.json missing)".to_string()
    } else if raw.contains("global fast-switch") {
        "the global server can't be fast-switched — use 's' to sync instead".to_string()
    } else if raw.contains("already up to date") {
        "already up to date".to_string()
    } else {
        format!("oops: {raw}")
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

    // TUI: one or more game folders (default: WuWa)
    let mut paths = args;
    if paths.is_empty() {
        paths.push(DEFAULT_GAME_DIR.to_string());
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<UiEvent>(64);

    // initial status for the first game
    {
        let tx = tx.clone();
        let path = paths[0].clone();
        tokio::spawn(async move {
            let result = match GameManager::open(PathBuf::from(path)).await {
                Ok(m) => m.status().await.map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(UiEvent::Status(result)).await;
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
        ..Default::default()
    };

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
                    continue;
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
                            format!("installing {game}/{server} into {target}"),
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
                        if state.paths.len() > 1 && !state.busy {
                            state.active = (state.active + 1) % state.paths.len();
                            state.status = None;
                            let msg = format!(
                                "switched to game {} ({})",
                                state.active + 1,
                                state.paths[state.active]
                            );
                            push_log(&mut state, msg);
                            spawn_status(&tx, &state.paths[state.active]);
                        }
                    }
                    KeyCode::Char('r') => {
                        if !state.busy {
                            spawn_status(&tx, &path);
                        }
                    }
                    KeyCode::Char('d') => {
                        if !state.busy {
                            state.busy = true;
                            state.task = Some(TaskUi {
                                kind: "predownload".into(),
                                ..Default::default()
                            });
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
                            spawn_simple(&tx, &path, TaskKind::Checkout);
                        }
                    }
                    KeyCode::PageUp => state.log_scroll += 10,
                    KeyCode::PageDown => state.log_scroll = state.log_scroll.saturating_sub(10),
                    _ => {}
                }
            }
        }

        while let Ok(ev) = rx.try_recv() {
            match ev {
                UiEvent::Status(s) => {
                    push_log(&mut state, format!("status refreshed: {s:?}"));
                    state.status = Some(s);
                }
                UiEvent::Progress(p) => match p {
                    ProgressEvent::Log(m) => push_log(&mut state, m),
                    ProgressEvent::GroupStart { name } => {
                        if let Some(t) = state.task.as_mut() {
                            t.current = Some(name);
                        }
                    }
                    ProgressEvent::GroupDone { name, bytes } => {
                        if let Some(t) = state.task.as_mut() {
                            t.done += 1;
                            push_log(&mut state, format!("done {name} ({bytes} bytes)"));
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
                        Ok(msg) => push_log(&mut state, format!("task ok: {msg}")),
                        Err(e) => push_log(&mut state, format!("task failed: {}", friendly_error(e))),
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

fn spawn_status(tx: &tokio::sync::mpsc::Sender<UiEvent>, path: &str) {
    let tx = tx.clone();
    let path = path.to_string();
    tokio::spawn(async move {
        let result = match GameManager::open(PathBuf::from(path)).await {
            Ok(m) => m.status().await.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        let _ = tx.send(UiEvent::Status(result)).await;
    });
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
            mgr.predownload(&plan, ptx).await.map_err(|e| e.to_string())?;
            if plan.total_bytes == 0 {
                Ok::<_, String>("already up to date — nothing to download".to_string())
            } else {
                Ok::<_, String>(format!(
                    "{} -> {} staged ({} groups, {} files, {:.1} GiB)",
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
        let result = match GameManager::install(game, server, PathBuf::from(path)).await {
            Ok(r) => Ok(format!(
                "installed {} v{} (checked={} ok={} repaired={} failed={})",
                game,
                r.version,
                r.sync.checked,
                r.sync.ok,
                r.sync.repaired,
                r.sync.failed.len()
            )),
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
        let result = async {
            let mgr = GameManager::open(PathBuf::from(path)).await.map_err(|e| e.to_string())?;
            match kind {
                TaskKind::Apply => {
                    let report = mgr.apply().await.map_err(|e| e.to_string())?;
                    Ok(format!(
                        "apply: merged={} skipped={} fallback={} swapped={} deleted={}",
                        report.merged,
                        report.skipped,
                        report.fallback,
                        report.swapped,
                        report.deleted.len()
                    ))
                }
                TaskKind::Sync => {
                    let report = mgr.sync().await.map_err(|e| e.to_string())?;
                    Ok(format!(
                        "sync: checked={} ok={} repaired={} ({:.1} GiB) failed={}",
                        report.checked,
                        report.ok,
                        report.repaired,
                        report.repaired_bytes as f64 / (1 << 30) as f64,
                        report.failed.len()
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
                        "checkout: {} -> {} ({} files, v{})",
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3), Constraint::Length(1)])
        .split(f.area());

    let title = Line::from(vec![
        Span::styled("kuro", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw(" — Kuro Games launcher (native, no wine)"),
    ]);
    f.render_widget(Paragraph::new(title).block(Block::default().borders(Borders::ALL)), chunks[0]);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(30), Constraint::Percentage(40)])
        .split(chunks[1]);

    // status panel
    let status_lines: Vec<Line> = match &state.status {
        Some(Ok(s)) => vec![
            Line::raw(format!("game:    {}", s.game)),
            Line::raw(format!("server:  {}", s.server)),
            Line::raw(format!("local:   {}", s.local_version.as_deref().unwrap_or("none"))),
            Line::raw(format!("remote:  {}", s.remote_version)),
            Line::styled(
                if s.update_available { "UPDATE AVAILABLE" } else { "up to date" },
                Style::default().fg(if s.update_available { Color::Yellow } else { Color::Green }),
            ),
        ],
        Some(Err(e)) => vec![Line::styled(format!("error: {e}"), Style::default().fg(Color::Red))],
        None => vec![Line::raw("loading...")],
    };
    f.render_widget(
        Paragraph::new(status_lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("status")),
        cols[0],
    );

    // task panel
    let task_lines: Vec<Line> = match &state.task {
        None => vec![Line::raw("idle")],
        Some(t) => {
            let mut v = vec![
                Line::raw(format!("task: {}", t.kind)),
                Line::raw(format!("groups: {}/{}", t.done, t.total)),
                Line::raw(format!("current: {}", t.current.as_deref().unwrap_or("-"))),
            ];
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
            .block(Block::default().borders(Borders::ALL).title("task")),
        cols[1],
    );

    // log panel: wrap long lines + scroll window (PgUp/PgDn)
    let all_logs: Vec<Line> = state.logs.iter().rev().take(200).map(|l| Line::raw(l)).collect();
    let max_scroll = all_logs.len().saturating_sub(1);
    let scroll = state.log_scroll.min(max_scroll);
    let log_lines: Vec<Line> = all_logs.iter().skip(scroll).take(60).cloned().collect();
    f.render_widget(
        Paragraph::new(log_lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("log (PgUp/PgDn)")),
        cols[2],
    );

    let footer = format!(
        "{} | r: refresh  d: predownload  a: apply  s: sync  c: checkout  i: install  h: help  q: quit{}",
        if state.paths.len() > 1 {
            format!(
                "Tab: switch game ({}/{})",
                state.active + 1,
                state.paths.len()
            )
        } else {
            "single game".to_string()
        },
        if state.busy { "   [busy]" } else { "" }
    );
    f.render_widget(Paragraph::new(Line::raw(footer)), chunks[2]);

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
            Line::raw("    Tab      switch game (multi-folder)"),
            Line::raw("    PgUp/PgDn scroll log               h/?  this help"),
            Line::raw("    q        quit"),
            Line::raw(""),
            Line::raw("  install a new game (also available via 'i'):"),
            Line::raw("    kuro install wuwa cn ~/Games/WutheringWaves"),
            Line::raw("    kuro install pgr global ~/PGR"),
            Line::raw("  then run: kuro <folder1> <folder2> ...   (Tab switches)"),
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
        modal_lines.push(Line::raw("  downloads the full client from the official CDN"));
        modal_lines.push(Line::raw("  (wuwa ~85 GB, pgr ~7 GB; resumable, verified by md5)"));
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
