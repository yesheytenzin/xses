//! Unified resume picker: scans all tools, fuzzy search, Enter → exec resume.
use crate::model::{Session, Tool};
use anyhow::Result;
use crossterm::event::{self, KeyCode, KeyEventKind, KeyModifiers};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use ratatui::{
    crossterm::terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    },
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame, Terminal,
};
use std::io::stdout;

/// Resume command argv per tool, using each CLI's real flags.
pub fn resume_argv(s: &Session) -> Vec<String> {
    let id = s.id.clone();
    match s.tool {
        Tool::Claude => vec!["claude".into(), "--resume".into(), id],
        Tool::Codex => vec!["codex".into(), "resume".into(), id],
        Tool::Pi => vec![
            "pi".into(),
            "--session".into(),
            id,
            "--cwd".into(),
            s.cwd.to_string_lossy().to_string(),
        ],
        Tool::Omp => vec![
            "omp".into(),
            "-r".into(),
            id,
            "--cwd".into(),
            s.cwd.to_string_lossy().to_string(),
        ],
        Tool::Opencode => vec![
            "opencode".into(),
            "-s".into(),
            id,
            s.cwd.to_string_lossy().to_string(),
        ],
    }
}

/// TUI picker. Returns the session to resume, or None on quit.
struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
        let _ = disable_raw_mode();
    }
}

pub fn picker(sessions: &[Session], query: &str, limit: Option<usize>) -> Result<Option<Session>> {
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    let mut stdout = stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;
    run_ui(&mut term, sessions, query, limit)
}

fn run_ui(
    term: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    sessions: &[Session],
    initial_query: &str,
    limit: Option<usize>,
) -> Result<Option<Session>> {
    let matcher = SkimMatcherV2::default();
    let mut query = initial_query.to_string();
    let mut selected = 0usize;
    let search: Vec<String> = sessions
        .iter()
        .map(|s| format!("{} {} {}", s.summary, s.cwd.display(), s.tool.as_str()))
        .collect();
    let mut filtered = Vec::new();
    let mut dirty = true;
    loop {
        if dirty {
            filtered = search
                .iter()
                .enumerate()
                .filter_map(|(i, hay)| {
                    (query.is_empty() || matcher.fuzzy_match(hay, &query).is_some()).then_some(i)
                })
                .collect();
            if let Some(limit) = limit {
                filtered.truncate(limit);
            }
            selected = 0;
            dirty = false;
        }
        if selected >= filtered.len() {
            selected = filtered.len().saturating_sub(1);
        }

        term.draw(|f| ui(f, sessions, &filtered, &query, selected))?;

        if let event::Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None);
                }
                KeyCode::Esc => return Ok(None),
                KeyCode::Down => selected = (selected + 1).min(filtered.len().saturating_sub(1)),
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Backspace => {
                    query.pop();
                    dirty = true;
                }
                KeyCode::Char(c) => {
                    query.push(c);
                    dirty = true;
                }
                KeyCode::Enter => {
                    if let Some(&idx) = filtered.get(selected) {
                        return Ok(Some(sessions[idx].clone()));
                    }
                }
                _ => {}
            }
        }
    }
}

fn ui(f: &mut Frame, sessions: &[Session], filtered: &[usize], query: &str, selected: usize) {
    let area = f.area();
    let chunks = ratatui::layout::Layout::vertical([
        ratatui::layout::Constraint::Length(1),
        ratatui::layout::Constraint::Min(3),
        ratatui::layout::Constraint::Length(1),
    ])
    .split(area);

    let input =
        Paragraph::new(format!("❯ {query}")).style(Style::default().add_modifier(Modifier::BOLD));
    f.render_widget(input, chunks[0]);

    let height = chunks[1].height as usize;
    let start = selected.saturating_sub(height.saturating_sub(1));
    let end = (start + height).min(filtered.len());
    let items: Vec<ListItem> = filtered[start..end]
        .iter()
        .map(|&i| {
            let s = &sessions[i];
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {:<7} ", s.tool.as_str()),
                    Style::default().fg(match s.tool {
                        Tool::Claude => Color::Cyan,
                        Tool::Codex => Color::Green,
                        Tool::Pi => Color::Yellow,
                        Tool::Omp => Color::Magenta,
                        _ => Color::Gray,
                    }),
                ),
                Span::styled(
                    format!("{:>5}m ", s.mtime.format("%d %H:%M")),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(format!("{:<40.40}", s.summary)),
                Span::styled(
                    format!(" {}", s.cwd.display()),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::NONE))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    // manual selection via offset; render_stateful needs state
    let mut st = ListState::default();
    st.select((!filtered.is_empty()).then_some(selected - start));
    f.render_stateful_widget(list, chunks[1], &mut st);

    let help = Paragraph::new(format!(
        "{} sessions · ↑/↓ move · type to filter · Enter resume · Esc quit",
        filtered.len()
    ))
    .style(Style::default().fg(Color::DarkGray));
    f.render_widget(help, chunks[2]);
}
