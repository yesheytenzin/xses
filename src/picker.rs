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
pub fn picker(sessions: &[Session], query: &str) -> Result<Option<Session>> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let res = run_ui(&mut term, sessions, query);

    disable_raw_mode()?;
    crossterm::execute!(term.backend_mut(), LeaveAlternateScreen)?;
    res
}

fn run_ui(
    term: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    sessions: &[Session],
    initial_query: &str,
) -> Result<Option<Session>> {
    let matcher = SkimMatcherV2::default();
    let mut query = initial_query.to_string();
    let mut selected = 0usize;
    let mut list_state = ListState::default();

    loop {
        // Filter
        let mut filtered: Vec<usize> = (0..sessions.len()).collect();
        if !query.is_empty() {
            filtered = sessions
                .iter()
                .enumerate()
                .filter_map(|(i, s)| {
                    let hay = format!("{} {} {}", s.summary, s.cwd.display(), s.tool.as_str());
                    matcher.fuzzy_match(&hay, &query).map(|_| i)
                })
                .collect();
        }
        if selected >= filtered.len() {
            selected = filtered.len().saturating_sub(1);
        }

        term.draw(|f| ui(f, sessions, &filtered, &query, selected, &mut list_state))?;

        if let event::Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None);
                }
                KeyCode::Esc => return Ok(None),
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(filtered.len().saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                KeyCode::Backspace => {
                    query.pop();
                }
                KeyCode::Char(c) => query.push(c),
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

fn ui(
    f: &mut Frame,
    sessions: &[Session],
    filtered: &[usize],
    query: &str,
    selected: usize,
    _list_state: &mut ListState,
) {
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

    let items: Vec<ListItem> = filtered
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
    st.select(Some(selected));
    f.render_stateful_widget(list, chunks[1], &mut st);

    let help = Paragraph::new(format!(
        "{} sessions · ↑/↓ or j/k move · type to filter · Enter resume · Esc quit",
        filtered.len()
    ))
    .style(Style::default().fg(Color::DarkGray));
    f.render_widget(help, chunks[2]);
}
