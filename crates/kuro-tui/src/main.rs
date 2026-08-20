//! `kuro` — ratatui terminal UI.

use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{init, restore, Frame, Terminal};

use kuro_core::{GameManager, GameStatus};

/// Default game folder (the user's known install).
const DEFAULT_GAME_DIR: &str = "/home/vedaru/.local/share/Steam/steamapps/common/Wuthering Waves";

#[derive(Default)]
struct UiState {
    status: Option<Result<GameStatus, String>>,
    logs: Vec<String>,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| DEFAULT_GAME_DIR.to_string());

    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    tokio::spawn(async move {
        let result = match GameManager::open(PathBuf::from(path)).await {
            Ok(m) => m.status().await.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        let _ = tx.send(result).await;
    });

    let terminal = init();
    let res = run(terminal, &mut rx).await;
    restore();
    res
}

async fn run(
    mut terminal: Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    rx: &mut tokio::sync::mpsc::Receiver<Result<GameStatus, String>>,
) -> std::io::Result<()> {
    let mut state = UiState::default();
    loop {
        terminal.draw(|f| ui(f, &state))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press
                    && (key.code == KeyCode::Char('q') || key.code == KeyCode::Esc)
                {
                    break;
                }
            }
        }

        if let Ok(status) = rx.try_recv() {
            state.logs.push(format!("status: {status:?}"));
            state.status = Some(status);
        }
    }
    Ok(())
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
    f.render_widget(
        Paragraph::new(title).block(Block::default().borders(Borders::ALL)),
        chunks[0],
    );

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(chunks[1]);

    let status_lines: Vec<Line> = match &state.status {
        Some(Ok(s)) => vec![
            Line::raw(format!("game:    {}", s.game)),
            Line::raw(format!("server:  {}", s.server)),
            Line::raw(format!("local:   {}", s.local_version.as_deref().unwrap_or("none"))),
            Line::raw(format!("remote:  {}", s.remote_version)),
            Line::styled(
                if s.update_available {
                    "UPDATE AVAILABLE"
                } else {
                    "up to date"
                },
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

    let log_lines: Vec<Line> = state.logs.iter().rev().take(50).map(|l| Line::raw(l)).collect();
    f.render_widget(
        Paragraph::new(log_lines).block(Block::default().borders(Borders::ALL).title("log")),
        cols[1],
    );

    f.render_widget(
        Paragraph::new(Line::raw("q: quit")),
        chunks[2],
    );
}
