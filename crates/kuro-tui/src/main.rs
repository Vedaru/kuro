//! `kuro` — ratatui terminal UI.
//!
//! Keys: `r` refresh status · `d` predownload · `a` apply · `s` sync ·
//! `c` checkout CN<->bilibili · `q` quit

use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{init, restore, Frame, Terminal};

use kuro_core::{GameManager, GameStatus, ProgressEvent, Server};

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
}

fn push_log(state: &mut UiState, msg: impl Into<String>) {
    let msg = msg.into();
    state.logs.push(msg);
    if state.logs.len() > 200 {
        state.logs.drain(0..state.logs.len() - 200);
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| DEFAULT_GAME_DIR.to_string());

    let (tx, mut rx) = tokio::sync::mpsc::channel::<UiEvent>(64);

    // initial status
    {
        let tx = tx.clone();
        let path = path.clone();
        tokio::spawn(async move {
            let result = match GameManager::open(PathBuf::from(path)).await {
                Ok(m) => m.status().await.map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(UiEvent::Status(result)).await;
        });
    }

    let terminal = init();
    let res = run(terminal, &mut rx, tx, path).await;
    restore();
    res
}

async fn run(
    mut terminal: Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    rx: &mut tokio::sync::mpsc::Receiver<UiEvent>,
    tx: tokio::sync::mpsc::Sender<UiEvent>,
    path: String,
) -> std::io::Result<()> {
    let mut state = UiState::default();

    loop {
        terminal.draw(|f| ui(f, &state))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
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
                        Err(e) => push_log(&mut state, format!("task failed: {e}")),
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
            Ok::<_, String>(format!(
                "{} -> {} staged ({} groups, {} files, {:.1} GiB)",
                plan.from_version,
                plan.to_version,
                plan.patch_groups.len(),
                plan.full_files.len(),
                plan.total_bytes as f64 / (1 << 30) as f64
            ))
        }
        .await;
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
        Paragraph::new(status_lines).block(Block::default().borders(Borders::ALL).title("status")),
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
                        Err(e) => format!("✘ {e}"),
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
        Paragraph::new(task_lines).block(Block::default().borders(Borders::ALL).title("task")),
        cols[1],
    );

    // log panel
    let log_lines: Vec<Line> = state.logs.iter().rev().take(60).map(|l| Line::raw(l)).collect();
    f.render_widget(
        Paragraph::new(log_lines).block(Block::default().borders(Borders::ALL).title("log")),
        cols[2],
    );

    let footer = format!(
        "r: refresh  d: predownload  a: apply  s: sync  c: checkout  q: quit{}",
        if state.busy { "   [busy]" } else { "" }
    );
    f.render_widget(Paragraph::new(Line::raw(footer)), chunks[2]);
}
