//! Interactive terminal UI.
//!
//! The scan runs on a background thread and streams events, so the list appears
//! immediately and sizes fill in as they are measured rather than the user
//! staring at a blank screen while a home directory is walked.

use std::collections::BTreeSet;
use std::io;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use ratatui::widgets::*;

use reclaim_core::exec::{self, CleanOptions};
use reclaim_core::format::bytes;
use reclaim_core::journal::Trigger;
use reclaim_core::model::{humanize_age, Candidate, CandidateId, Severity, Tier};
use reclaim_core::pipeline::{self, ScanEvent};
use reclaim_core::staleness::Filter;

use crate::app::App;

/// What the user is looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    List,
    Detail,
    ConfirmClean,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sort {
    Score,
    Size,
    Age,
    Name,
}

impl Sort {
    fn next(self) -> Self {
        match self {
            Sort::Score => Sort::Size,
            Sort::Size => Sort::Age,
            Sort::Age => Sort::Name,
            Sort::Name => Sort::Score,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Sort::Score => "score",
            Sort::Size => "size",
            Sort::Age => "age",
            Sort::Name => "name",
        }
    }
}

struct Ui {
    candidates: Vec<Candidate>,
    selected: BTreeSet<CandidateId>,
    cursor: usize,
    screen: Screen,
    sort: Sort,
    filter_text: String,
    editing_filter: bool,
    scanning: bool,
    scan_progress: (usize, usize),
    status: String,
    dry_run: bool,
    quit: bool,
}

impl Ui {
    fn new() -> Self {
        Self {
            candidates: Vec::new(),
            selected: BTreeSet::new(),
            cursor: 0,
            screen: Screen::List,
            sort: Sort::Score,
            filter_text: String::new(),
            editing_filter: false,
            scanning: true,
            scan_progress: (0, 0),
            status: "Scanning…".into(),
            dry_run: false,
            quit: false,
        }
    }

    /// Rows currently shown, after the text filter.
    fn visible(&self) -> Vec<&Candidate> {
        let needle = self.filter_text.to_lowercase();
        let mut rows: Vec<&Candidate> = self
            .candidates
            .iter()
            .filter(|c| {
                needle.is_empty()
                    || c.label.to_lowercase().contains(&needle)
                    || c.provider.to_lowercase().contains(&needle)
                    || c.group.title().to_lowercase().contains(&needle)
            })
            .collect();

        match self.sort {
            Sort::Score => {
                rows.sort_by(|a, b| b.score.unwrap_or(0.0).total_cmp(&a.score.unwrap_or(0.0)))
            }
            Sort::Size => rows.sort_by_key(|c| std::cmp::Reverse(c.reclaimable())),
            Sort::Age => rows.sort_by_key(|c| std::cmp::Reverse(c.last_used_days().unwrap_or(0))),
            Sort::Name => rows.sort_by(|a, b| a.label.cmp(&b.label)),
        }
        rows
    }

    fn current(&self) -> Option<&Candidate> {
        self.visible().get(self.cursor).copied()
    }

    fn chosen(&self) -> Vec<Candidate> {
        self.candidates
            .iter()
            .filter(|c| self.selected.contains(&c.id))
            .cloned()
            .collect()
    }

    fn chosen_bytes(&self) -> u64 {
        self.chosen().iter().map(Candidate::reclaimable).sum()
    }

    fn move_cursor(&mut self, delta: isize) {
        let len = self.visible().len();
        if len == 0 {
            self.cursor = 0;
            return;
        }
        let next = self.cursor as isize + delta;
        self.cursor = next.clamp(0, len as isize - 1) as usize;
    }

    fn toggle_current(&mut self) {
        if let Some(candidate) = self.current() {
            let id = candidate.id.clone();
            if !self.selected.remove(&id) {
                self.selected.insert(id);
            }
        }
    }

    /// Select every visible row, but never a Caution-tier one: bulk selection
    /// must not sweep up something irreplaceable by accident.
    fn select_all_visible(&mut self) {
        let ids: Vec<CandidateId> = self
            .visible()
            .iter()
            .filter(|c| c.tier != Tier::Caution)
            .map(|c| c.id.clone())
            .collect();
        let skipped = self
            .visible()
            .iter()
            .filter(|c| c.tier == Tier::Caution)
            .count();
        self.selected.extend(ids);
        self.status = if skipped > 0 {
            format!("Selected all visible except {skipped} caution-tier item(s)")
        } else {
            "Selected all visible".into()
        };
    }
}

/// Run the interactive UI.
pub fn run(app: &App, filter: Filter) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let result = event_loop(app, filter, &mut terminal);
    restore_terminal(&mut terminal)?;
    result
}

fn event_loop<B: Backend + io::Write>(
    app: &App,
    filter: Filter,
    terminal: &mut Terminal<B>,
) -> Result<()> {
    let mut ui = Ui::new();

    // The scan streams into the UI thread so the list is usable immediately.
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|scope| -> Result<()> {
        scope.spawn(|| {
            pipeline::scan(&app.providers, &app.paths, &app.config, &filter, Some(&tx));
            drop(tx);
        });

        let tick = Duration::from_millis(60);
        let mut last_draw = Instant::now();

        while !ui.quit {
            drain_scan_events(&mut ui, &rx);

            if last_draw.elapsed() >= tick {
                terminal.draw(|frame| draw(frame, &ui, app))?;
                last_draw = Instant::now();
            }

            if event::poll(tick)? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == event::KeyEventKind::Press {
                        handle_key(&mut ui, app, key)?;
                    }
                }
            }
        }
        Ok(())
    })
}

fn drain_scan_events(ui: &mut Ui, rx: &mpsc::Receiver<ScanEvent>) {
    while let Ok(event) = rx.try_recv() {
        match event {
            ScanEvent::ProjectsFound { count, .. } => {
                ui.status = format!("Found {count} project(s), discovering caches…");
            }
            ScanEvent::Discovered(candidates) => {
                ui.candidates = candidates;
                ui.status = format!("Measuring {} item(s)…", ui.candidates.len());
            }
            ScanEvent::Measured {
                candidate,
                done,
                total,
            } => {
                ui.scan_progress = (done, total);
                if let Some(existing) = ui.candidates.iter_mut().find(|c| c.id == candidate.id) {
                    *existing = *candidate;
                }
            }
            ScanEvent::Complete(result) => {
                ui.candidates = result.candidates.clone();
                ui.scanning = false;
                ui.status = format!(
                    "{} item(s), {} reclaimable",
                    result.candidates.len(),
                    bytes(result.total_reclaimable())
                );
            }
        }
    }
}

/// Handle a key while the filter box has focus.
///
/// Returns true if the key was consumed. Split out because it must swallow keys
/// that are otherwise actions: typing "d" into the filter must not start a delete.
fn handle_filter_key(ui: &mut Ui, key: KeyEvent) -> bool {
    if !ui.editing_filter {
        return false;
    }
    match key.code {
        KeyCode::Esc => {
            ui.editing_filter = false;
            ui.filter_text.clear();
        }
        KeyCode::Enter => ui.editing_filter = false,
        KeyCode::Backspace => {
            ui.filter_text.pop();
        }
        KeyCode::Char(c) => ui.filter_text.push(c),
        _ => {}
    }
    ui.cursor = 0;
    true
}

fn handle_key(ui: &mut Ui, app: &App, key: KeyEvent) -> Result<()> {
    if handle_filter_key(ui, key) {
        return Ok(());
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        ui.quit = true;
        return Ok(());
    }

    match ui.screen {
        Screen::Help | Screen::Detail => {
            // Any key returns to the list from a read-only screen.
            ui.screen = Screen::List;
            return Ok(());
        }
        Screen::ConfirmClean => {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    perform_clean(ui, app)?;
                    ui.screen = Screen::List;
                }
                _ => {
                    ui.screen = Screen::List;
                    ui.status = "Cancelled. Nothing was removed.".into();
                }
            }
            return Ok(());
        }
        Screen::List => {}
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => ui.quit = true,
        KeyCode::Up | KeyCode::Char('k') => ui.move_cursor(-1),
        KeyCode::Down | KeyCode::Char('j') => ui.move_cursor(1),
        KeyCode::PageUp => ui.move_cursor(-10),
        KeyCode::PageDown => ui.move_cursor(10),
        KeyCode::Home => ui.cursor = 0,
        KeyCode::End => ui.cursor = ui.visible().len().saturating_sub(1),
        KeyCode::Char(' ') => ui.toggle_current(),
        KeyCode::Char('a') => ui.select_all_visible(),
        KeyCode::Char('n') => {
            ui.selected.clear();
            ui.status = "Selection cleared".into();
        }
        KeyCode::Char('s') => {
            ui.sort = ui.sort.next();
            ui.cursor = 0;
        }
        KeyCode::Char('/') => {
            ui.editing_filter = true;
            ui.filter_text.clear();
        }
        KeyCode::Enter => {
            if ui.current().is_some() {
                ui.screen = Screen::Detail;
            }
        }
        KeyCode::Char('?') => ui.screen = Screen::Help,
        KeyCode::Char('p') => {
            ui.dry_run = !ui.dry_run;
            ui.status = if ui.dry_run {
                "Dry-run mode ON".into()
            } else {
                "Dry-run mode OFF".into()
            };
        }
        KeyCode::Char('d') => {
            if ui.selected.is_empty() {
                ui.status = "Nothing selected. Press space to select an item.".into();
            } else {
                ui.screen = Screen::ConfirmClean;
            }
        }
        _ => {}
    }

    Ok(())
}

fn perform_clean(ui: &mut Ui, app: &App) -> Result<()> {
    let chosen = ui.chosen();
    let options = CleanOptions {
        dry_run: ui.dry_run,
        mode: app.config.delete.mode,
        trigger: Trigger::Tui,
        concurrency: app.config.scan.threads(),
    };

    let record = exec::clean(&chosen, &app.guard, &options, None);
    let _ = app.journal.write(&record);

    ui.status = record.summary();

    // Drop what actually went away, keep what failed so the user can see it.
    let removed: BTreeSet<CandidateId> = record
        .items
        .iter()
        .filter(|i| i.disposition != reclaim_core::Disposition::Failed)
        .map(|i| i.id.clone())
        .collect();
    if !ui.dry_run {
        ui.candidates.retain(|c| !removed.contains(&c.id));
        ui.selected.retain(|id| !removed.contains(id));
    }
    ui.cursor = 0;

    Ok(())
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn draw(frame: &mut Frame, ui: &Ui, app: &App) {
    // The footer is 4 rows: two borders plus the status line *and* the key hints.
    // At 3 the hints were silently clipped, leaving the UI undiscoverable.
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(4),
    ])
    .split(frame.area());

    draw_header(frame, chunks[0], ui);
    draw_list(frame, chunks[1], ui, app);
    draw_footer(frame, chunks[2], ui);

    match ui.screen {
        Screen::Detail => draw_detail(frame, ui, app),
        Screen::Help => draw_help(frame),
        Screen::ConfirmClean => draw_confirm(frame, ui),
        Screen::List => {}
    }
}

fn draw_header(frame: &mut Frame, area: Rect, ui: &Ui) {
    let total: u64 = ui.candidates.iter().map(Candidate::reclaimable).sum();
    let progress = if ui.scanning && ui.scan_progress.1 > 0 {
        format!(" · measuring {}/{}", ui.scan_progress.0, ui.scan_progress.1)
    } else {
        String::new()
    };

    let line = Line::from(vec![
        Span::styled("reclaim", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(
            format!("{} reclaimable", bytes(total)),
            Style::default().fg(Color::Green),
        ),
        Span::raw("  ·  "),
        Span::styled(
            format!(
                "{} selected ({})",
                ui.selected.len(),
                bytes(ui.chosen_bytes())
            ),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw(progress),
        if ui.dry_run {
            Span::styled("  [DRY RUN]", Style::default().fg(Color::Yellow))
        } else {
            Span::raw("")
        },
    ]);

    frame.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn draw_list(frame: &mut Frame, area: Rect, ui: &Ui, app: &App) {
    let rows: Vec<Row> = ui
        .visible()
        .iter()
        .map(|candidate| {
            let mark = if ui.selected.contains(&candidate.id) {
                "●"
            } else {
                " "
            };
            let age = candidate
                .last_used_days()
                .map(humanize_age)
                .unwrap_or_else(|| "—".into());
            let warn = match candidate.max_severity() {
                Some(Severity::Danger) => "!!",
                Some(Severity::Caution) => "!",
                _ => "",
            };

            Row::new(vec![
                Cell::from(mark),
                Cell::from(bytes(candidate.reclaimable()))
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                Cell::from(candidate.tier.as_str()).style(tier_style(candidate.tier)),
                Cell::from(age),
                Cell::from(candidate.label.clone()),
                Cell::from(candidate.group.title()),
                Cell::from(warn).style(Style::default().fg(Color::Red)),
            ])
        })
        .collect();

    let mut state = TableState::default();
    state.select(Some(ui.cursor));

    let title = if ui.editing_filter {
        format!(" filter: {}▌ ", ui.filter_text)
    } else if !ui.filter_text.is_empty() {
        format!(" filter: {} ", ui.filter_text)
    } else {
        format!(" sorted by {} ", ui.sort.label())
    };

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(14),
            Constraint::Min(20),
            Constraint::Length(14),
            Constraint::Length(3),
        ],
    )
    .header(
        Row::new(vec!["", "SIZE", "TIER", "LAST USED", "WHAT", "GROUP", ""])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::ALL).title(title))
    .row_highlight_style(Style::default().bg(Color::DarkGray))
    .highlight_symbol("▸");

    frame.render_stateful_widget(table, area, &mut state);
    let _ = app;
}

fn draw_footer(frame: &mut Frame, area: Rect, ui: &Ui) {
    let keys = "space select · a all · n none · d clean · / filter · s sort · enter detail · p dry-run · ? help · q quit";
    let text = vec![
        Line::from(ui.status.clone()),
        Line::from(Span::styled(keys, Style::default().fg(Color::DarkGray))),
    ];
    frame.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn draw_detail(frame: &mut Frame, ui: &Ui, app: &App) {
    let Some(candidate) = ui.current() else {
        return;
    };
    let area = centered(frame.area(), 78, 70);
    frame.render_widget(Clear, area);

    let mut lines = vec![
        Line::from(Span::styled(
            candidate.label.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(candidate.detail.clone()),
        Line::from(""),
        Line::from(format!("Size on disk:  {}", bytes(candidate.reclaimable()))),
    ];

    if let Some(size) = candidate.size {
        if size.shared > 0 {
            lines.push(Line::from(Span::styled(
                format!(
                    "Shared:        {} is hardlinked elsewhere and will not be freed",
                    bytes(size.shared)
                ),
                Style::default().fg(Color::Yellow),
            )));
        }
        lines.push(Line::from(format!("Files:         {}", size.files)));
        if size.partial {
            lines.push(Line::from(Span::styled(
                "Note:          some subdirectories were unreadable, so this is a lower bound",
                Style::default().fg(Color::Yellow),
            )));
        }
    }

    if let Some(signals) = &candidate.signals {
        lines.push(Line::from(format!(
            "Last used:     {} ({} days)",
            humanize_age(signals.last_used_days),
            signals.last_used_days
        )));
        if signals.source_mtime.is_some() {
            lines.push(Line::from(
                "               (derived from the owning project's source files, not the artifact)",
            ));
        }
        if signals.active_now {
            lines.push(Line::from(Span::styled(
                "Active:        touched in the last 24h — a build may be using it right now",
                Style::default().fg(Color::Yellow),
            )));
        }
    }

    lines.push(Line::from(format!(
        "Risk:          {}",
        candidate.tier.as_str()
    )));
    lines.push(Line::from(format!(
        "Comes back:    {}",
        candidate.regen.summary()
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Paths",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    for path in &candidate.paths {
        lines.push(Line::from(format!("  {}", app.paths.contract(path))));
    }

    if !candidate.warnings.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Before you delete this",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for warning in &candidate.warnings {
            lines.push(Line::from(Span::styled(
                format!("  • {}", warning.message),
                Style::default().fg(severity_colour(warning.severity)),
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "any key to go back",
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" detail ")),
        area,
    );
}

fn draw_confirm(frame: &mut Frame, ui: &Ui) {
    let area = centered(frame.area(), 64, 50);
    frame.render_widget(Clear, area);

    let chosen = ui.chosen();
    let caution: Vec<&Candidate> = chosen.iter().filter(|c| c.tier == Tier::Caution).collect();

    let mut lines = vec![
        Line::from(Span::styled(
            format!(
                "Reclaim {} from {} item(s)?",
                bytes(ui.chosen_bytes()),
                chosen.len()
            ),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    for candidate in chosen.iter().take(10) {
        lines.push(Line::from(format!(
            "  {:>9}  {:<8} {}",
            bytes(candidate.reclaimable()),
            candidate.tier.as_str(),
            candidate.label
        )));
    }
    if chosen.len() > 10 {
        lines.push(Line::from(format!("  …and {} more", chosen.len() - 10)));
    }

    if !caution.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(
                "{} caution-tier item(s) may be irreplaceable:",
                caution.len()
            ),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
        for candidate in &caution {
            for warning in &candidate.warnings {
                lines.push(Line::from(Span::styled(
                    format!("  • {}", warning.message),
                    Style::default().fg(Color::Red),
                )));
            }
        }
    }

    lines.push(Line::from(""));
    if ui.dry_run {
        lines.push(Line::from(Span::styled(
            "Dry-run mode is on: nothing will actually be removed.",
            Style::default().fg(Color::Yellow),
        )));
    }
    lines.push(Line::from(Span::styled(
        "y to confirm · any other key to cancel",
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" confirm ")),
        area,
    );
}

fn draw_help(frame: &mut Frame) {
    let area = centered(frame.area(), 60, 60);
    frame.render_widget(Clear, area);

    let lines: Vec<Line> = [
        ("↑ ↓ / j k", "move"),
        ("space", "select or deselect"),
        ("a", "select all visible (never caution-tier)"),
        ("n", "clear selection"),
        ("enter", "show the full evidence for an item"),
        ("d", "reclaim the selected items"),
        ("p", "toggle dry-run mode"),
        ("/", "filter by text"),
        ("s", "cycle sort: score, size, age, name"),
        ("q / esc", "quit"),
        ("", ""),
        ("safe", "regenerates automatically"),
        ("review", "costs a re-download or a rebuild"),
        ("caution", "may be irreplaceable — read the warnings"),
    ]
    .iter()
    .map(|(key, description)| {
        Line::from(vec![
            Span::styled(
                format!("  {key:<12}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(*description),
        ])
    })
    .collect();

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" keys ")),
        area,
    );
}

fn tier_style(tier: Tier) -> Style {
    Style::default().fg(match tier {
        Tier::Safe => Color::Green,
        Tier::Review => Color::Yellow,
        Tier::Caution => Color::Red,
    })
}

fn severity_colour(severity: Severity) -> Color {
    match severity {
        Severity::Info => Color::Gray,
        Severity::Caution => Color::Yellow,
        Severity::Danger => Color::Red,
    }
}

fn centered(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
}

// ---------------------------------------------------------------------------
// Terminal lifecycle
// ---------------------------------------------------------------------------

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal<B: Backend + io::Write>(terminal: &mut Terminal<B>) -> Result<()> {
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reclaim_core::model::{CandidateBuilder, Group, Signals, Size};
    use std::time::SystemTime;

    fn candidate(label: &str, tier: Tier, on_disk: u64, days: u32) -> Candidate {
        CandidateBuilder::new("test.thing", Group::Node, label)
            .path(format!("/home/tester/.cache/{label}"))
            .tier(tier)
            .build()
            .with_measurement(
                Size {
                    on_disk,
                    logical: on_disk,
                    ..Size::default()
                },
                Signals {
                    artifact_mtime: SystemTime::UNIX_EPOCH,
                    artifact_atime: None,
                    source_mtime: None,
                    vcs_activity: None,
                    last_used_days: days,
                    active_now: false,
                },
            )
    }

    fn ui_with(candidates: Vec<Candidate>) -> Ui {
        let mut ui = Ui::new();
        ui.candidates = candidates;
        ui.scanning = false;
        ui
    }

    #[test]
    fn selecting_all_visible_never_includes_caution_items() {
        // Bulk selection must not be able to sweep up something irreplaceable.
        let mut ui = ui_with(vec![
            candidate("npm", Tier::Safe, 1000, 100),
            candidate("archives", Tier::Caution, 5000, 100),
            candidate("m2", Tier::Review, 2000, 100),
        ]);

        ui.select_all_visible();

        assert_eq!(ui.selected.len(), 2);
        let chosen = ui.chosen();
        let chosen_labels: Vec<&str> = chosen.iter().map(|c| c.label.as_str()).collect();
        assert!(!chosen_labels.contains(&"archives"));
        assert!(
            ui.status.contains("caution"),
            "the user must be told why: {}",
            ui.status
        );
    }

    #[test]
    fn toggling_selection_adds_then_removes() {
        let mut ui = ui_with(vec![candidate("npm", Tier::Safe, 1000, 100)]);
        ui.toggle_current();
        assert_eq!(ui.selected.len(), 1);
        ui.toggle_current();
        assert!(ui.selected.is_empty());
    }

    #[test]
    fn the_cursor_cannot_leave_the_list() {
        let mut ui = ui_with(vec![
            candidate("a", Tier::Safe, 1, 1),
            candidate("b", Tier::Safe, 1, 1),
        ]);
        ui.move_cursor(-5);
        assert_eq!(ui.cursor, 0);
        ui.move_cursor(50);
        assert_eq!(ui.cursor, 1);
    }

    #[test]
    fn the_cursor_stays_valid_on_an_empty_list() {
        let mut ui = ui_with(vec![]);
        ui.move_cursor(3);
        assert_eq!(ui.cursor, 0);
        assert!(ui.current().is_none());
    }

    #[test]
    fn sorting_cycles_and_actually_reorders() {
        let mut ui = ui_with(vec![
            candidate("small-old", Tier::Safe, 100, 900),
            candidate("big-new", Tier::Safe, 100_000, 1),
        ]);

        ui.sort = Sort::Size;
        assert_eq!(ui.visible()[0].label, "big-new");

        ui.sort = Sort::Age;
        assert_eq!(ui.visible()[0].label, "small-old");

        ui.sort = Sort::Name;
        assert_eq!(ui.visible()[0].label, "big-new");

        assert_eq!(Sort::Score.next(), Sort::Size);
        assert_eq!(Sort::Name.next(), Sort::Score);
    }

    #[test]
    fn the_text_filter_matches_label_provider_and_group() {
        let mut ui = ui_with(vec![
            candidate("npm cache", Tier::Safe, 1, 1),
            candidate("gradle caches", Tier::Safe, 1, 1),
        ]);

        ui.filter_text = "npm".into();
        assert_eq!(ui.visible().len(), 1);

        ui.filter_text = "node".into();
        assert_eq!(ui.visible().len(), 2, "group title should match both");

        ui.filter_text = "nothing-matches".into();
        assert!(ui.visible().is_empty());
    }

    #[test]
    fn chosen_bytes_only_counts_selected_rows() {
        let mut ui = ui_with(vec![
            candidate("a", Tier::Safe, 1000, 1),
            candidate("b", Tier::Safe, 5000, 1),
        ]);
        assert_eq!(ui.chosen_bytes(), 0);
        ui.toggle_current();
        assert_eq!(ui.chosen_bytes(), 1000);
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn filter_editing_swallows_keys_that_are_otherwise_actions() {
        // Typing "d" or "q" into the filter must not delete or quit.
        let mut ui = ui_with(vec![candidate("a", Tier::Safe, 1, 1)]);
        ui.editing_filter = true;

        for c in ['d', 'q', 'a'] {
            assert!(handle_filter_key(&mut ui, press(KeyCode::Char(c))));
        }

        assert_eq!(ui.filter_text, "dqa");
        assert!(!ui.quit, "keys typed into the filter must not quit");
        assert!(
            ui.selected.is_empty(),
            "keys typed into the filter must not select"
        );
    }

    #[test]
    fn escape_abandons_the_filter_and_enter_keeps_it() {
        let mut ui = ui_with(vec![candidate("a", Tier::Safe, 1, 1)]);

        ui.editing_filter = true;
        handle_filter_key(&mut ui, press(KeyCode::Char('x')));
        handle_filter_key(&mut ui, press(KeyCode::Enter));
        assert!(!ui.editing_filter);
        assert_eq!(ui.filter_text, "x", "enter commits the filter");

        ui.editing_filter = true;
        handle_filter_key(&mut ui, press(KeyCode::Esc));
        assert!(!ui.editing_filter);
        assert!(ui.filter_text.is_empty(), "escape clears the filter");
    }

    #[test]
    fn filter_keys_are_ignored_when_the_box_is_not_focused() {
        let mut ui = ui_with(vec![candidate("a", Tier::Safe, 1, 1)]);
        assert!(!handle_filter_key(&mut ui, press(KeyCode::Char('d'))));
        assert!(ui.filter_text.is_empty());
    }

    #[test]
    fn dry_run_starts_off_so_the_tui_does_real_work_by_default() {
        assert!(!Ui::new().dry_run);
    }

    // ---- rendering -------------------------------------------------------
    //
    // Drawn into an in-memory buffer rather than a terminal, so the frames are
    // actually asserted rather than merely assumed to compile.

    fn test_app() -> (tempfile::TempDir, App) {
        let tmp = tempfile::TempDir::new().unwrap();
        let app = App::new(&crate::cli::GlobalArgs {
            config: None,
            root: Some(tmp.path().to_path_buf()),
            concurrency: Some(1),
            verbose: 0,
            no_color: true,
        })
        .unwrap();
        (tmp, app)
    }

    /// Render one frame and return it as plain text.
    fn render(ui: &Ui, app: &App, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, ui, app)).unwrap();

        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_list_frame_shows_size_tier_age_and_label() {
        let (_tmp, app) = test_app();
        let mut ui = ui_with(vec![
            candidate("npm cache", Tier::Safe, 2 * 1024 * 1024 * 1024, 120),
            candidate("Xcode Archives", Tier::Caution, 5 * 1024 * 1024 * 1024, 400),
        ]);
        ui.status = "2 items".into();

        let frame = render(&ui, &app, 110, 24);

        assert!(frame.contains("reclaim"), "{frame}");
        assert!(frame.contains("2.00 GB"), "{frame}");
        assert!(frame.contains("npm cache"), "{frame}");
        assert!(frame.contains("Xcode Archives"), "{frame}");
        assert!(frame.contains("safe"), "{frame}");
        assert!(frame.contains("caution"), "{frame}");
        assert!(frame.contains("4 months ago"), "{frame}");
        // The key hints must be visible; the UI is unusable without them.
        assert!(frame.contains("space select"), "{frame}");
    }

    #[test]
    fn the_header_reports_the_selection_total() {
        let (_tmp, app) = test_app();
        let mut ui = ui_with(vec![candidate(
            "npm cache",
            Tier::Safe,
            1024 * 1024 * 1024,
            100,
        )]);
        ui.toggle_current();

        let frame = render(&ui, &app, 110, 24);
        assert!(frame.contains("1 selected"), "{frame}");
        assert!(frame.contains("1.00 GB"), "{frame}");
    }

    #[test]
    fn dry_run_mode_is_visible_in_the_frame() {
        // The user must never be unsure whether a delete will really happen.
        let (_tmp, app) = test_app();
        let mut ui = ui_with(vec![candidate("npm cache", Tier::Safe, 1024, 100)]);
        ui.dry_run = true;

        assert!(render(&ui, &app, 110, 24).contains("DRY RUN"));
    }

    #[test]
    fn the_detail_frame_shows_the_warnings_and_regeneration_cost() {
        let (_tmp, app) = test_app();
        let mut c = candidate("Xcode Archives", Tier::Caution, 5 * 1024 * 1024 * 1024, 400);
        c.warnings = vec![reclaim_core::Warning::danger(
            "Contains dSYMs for shipped builds.",
        )];
        c.regen = reclaim_core::Regen::Never;
        c.detail = "Archived builds.".into();

        let mut ui = ui_with(vec![c]);
        ui.screen = Screen::Detail;

        let frame = render(&ui, &app, 110, 30);
        assert!(frame.contains("Before you delete this"), "{frame}");
        assert!(frame.contains("dSYMs"), "{frame}");
        assert!(frame.contains("gone forever"), "{frame}");
    }

    #[test]
    fn the_confirm_frame_restates_the_caution_warnings() {
        let (_tmp, app) = test_app();
        let mut c = candidate("Xcode Archives", Tier::Caution, 5 * 1024 * 1024 * 1024, 400);
        c.warnings = vec![reclaim_core::Warning::danger(
            "These cannot be regenerated.",
        )];

        let mut ui = ui_with(vec![c]);
        ui.toggle_current();
        ui.screen = Screen::ConfirmClean;

        let frame = render(&ui, &app, 110, 30);
        assert!(frame.contains("Reclaim"), "{frame}");
        assert!(frame.contains("irreplaceable"), "{frame}");
        assert!(frame.contains("cannot be regenerated"), "{frame}");
        assert!(frame.contains("y to confirm"), "{frame}");
    }

    #[test]
    fn the_help_frame_explains_the_tiers() {
        let (_tmp, app) = test_app();
        let mut ui = ui_with(vec![]);
        ui.screen = Screen::Help;

        let frame = render(&ui, &app, 110, 30);
        assert!(frame.contains("regenerates automatically"), "{frame}");
        assert!(frame.contains("may be irreplaceable"), "{frame}");
    }

    #[test]
    fn an_empty_list_still_renders_without_panicking() {
        let (_tmp, app) = test_app();
        let ui = ui_with(vec![]);
        let frame = render(&ui, &app, 80, 20);
        assert!(frame.contains("reclaim"));
    }

    #[test]
    fn a_very_narrow_terminal_does_not_panic() {
        // ratatui panics on impossible layouts if constraints are wrong.
        let (_tmp, app) = test_app();
        let ui = ui_with(vec![candidate("npm cache", Tier::Safe, 1024, 100)]);
        for (w, h) in [(20u16, 10u16), (40, 12), (200, 60)] {
            let _ = render(&ui, &app, w, h);
        }
    }
}
