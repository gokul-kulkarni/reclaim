//! The terminal UI while a clean runs, and the results screen afterwards.

use ratatui::prelude::*;
use ratatui::widgets::*;

use reclaim_core::exec::{self, BackgroundClean, CleanOptions};
use reclaim_core::format::{bytes, ellipsize};
use reclaim_core::journal::{ItemOutcome, RunRecord};
use reclaim_core::model::Candidate;
use reclaim_core::progress::{sections, shown_bytes, CleanProgress, OutcomeSection};
use reclaim_core::{PathGuard, Paths};

use crate::run_view::{totals_line, verb, where_it_was};

/// Longest item label shown on one line.
const LABEL_WIDTH: usize = 40;

/// A clean in flight: the background run and what it has reported so far.
pub(super) struct Cleaning {
    pub progress: CleanProgress,
    run: BackgroundClean,
}

/// What draining the run's events found.
pub(super) enum Poll {
    Running,
    Finished(RunRecord),
    /// The run's thread ended without reporting a result.
    Lost,
}

impl Cleaning {
    pub fn start(chosen: Vec<Candidate>, guard: PathGuard, options: CleanOptions) -> Self {
        let progress = CleanProgress::new(options.dry_run);
        Self {
            progress,
            run: exec::spawn_clean(chosen, guard, options),
        }
    }

    /// Fold in every event waiting, without blocking the UI thread.
    pub fn poll(&mut self) -> Poll {
        use std::sync::mpsc::TryRecvError;
        loop {
            match self.run.events.try_recv() {
                Ok(event) => self.progress.apply(event),
                Err(TryRecvError::Empty) => return Poll::Running,
                Err(TryRecvError::Disconnected) => {
                    return match self.progress.record.take() {
                        Some(record) => Poll::Finished(record),
                        None => Poll::Lost,
                    };
                }
            }
        }
    }
}

/// The overlay shown while items are removed.
pub(super) fn draw_cleaning(
    frame: &mut Frame,
    area: Rect,
    progress: &CleanProgress,
    spinner: char,
) {
    frame.render_widget(Clear, area);
    let title = if progress.dry_run {
        " dry run "
    } else {
        " reclaiming "
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(inner);

    let heading = format!(
        "{spinner} {} {} from {} item(s)",
        if progress.dry_run {
            "Checking"
        } else {
            "Reclaiming"
        },
        bytes(progress.bytes_total),
        progress.total
    );
    frame.render_widget(
        Paragraph::new(heading).style(Style::default().add_modifier(Modifier::BOLD)),
        rows[0],
    );
    frame.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(Color::Cyan))
            .ratio(progress.ratio())
            .label(format!("{} of {}", progress.done, progress.total)),
        rows[1],
    );
    frame.render_widget(Paragraph::new(progress.status_line()), rows[2]);
    let now = progress
        .current()
        .map(|label| format!("Now: {}", ellipsize(label, LABEL_WIDTH)))
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(now).style(Style::default().fg(Color::DarkGray)),
        rows[3],
    );

    // The most recent results that fit, newest at the bottom.
    let room = rows[5].height as usize;
    let skip = progress.finished.len().saturating_sub(room);
    let finished: Vec<Line> = progress.finished[skip..]
        .iter()
        .map(finished_line)
        .collect();
    frame.render_widget(Paragraph::new(finished), rows[5]);

    frame.render_widget(
        Paragraph::new("Results appear when the run ends · ctrl-c abandons it")
            .style(Style::default().fg(Color::DarkGray)),
        rows[6],
    );
}

fn finished_line(item: &ItemOutcome) -> Line<'static> {
    let section = OutcomeSection::of(item.disposition);
    Line::from(vec![
        Span::styled(format!("  {:<10}", verb(section)), section_style(section)),
        Span::raw(format!(
            " {:>9}  {}",
            bytes(shown_bytes(item)),
            ellipsize(&item.label, LABEL_WIDTH)
        )),
    ])
}

fn section_style(section: OutcomeSection) -> Style {
    Style::default().fg(match section {
        OutcomeSection::Failed => Color::Red,
        OutcomeSection::Trashed => Color::Yellow,
        OutcomeSection::DryRun => Color::Cyan,
        OutcomeSection::AlreadyGone => Color::DarkGray,
        OutcomeSection::Removed | OutcomeSection::Command => Color::Green,
    })
}

/// Every line of the results screen, so the caller can bound its scrolling.
pub(super) fn results_lines(record: &RunRecord, paths: &Paths) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (section, items) in sections(&record.items) {
        let total: u64 = items.iter().map(|i| shown_bytes(i)).sum();
        lines.push(Line::from(Span::styled(
            format!(
                "{} · {} item(s) · {}",
                section.heading(),
                items.len(),
                bytes(total)
            ),
            section_style(section).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            format!("  {}", section.explanation()),
            Style::default().fg(Color::DarkGray),
        )));
        for item in items {
            lines.push(Line::from(format!(
                "  {:>9}  {}",
                bytes(shown_bytes(item)),
                ellipsize(&item.label, LABEL_WIDTH)
            )));
            lines.push(Line::from(Span::styled(
                format!("             {}", where_it_was(item, paths)),
                Style::default().fg(Color::DarkGray),
            )));
            if let Some(error) = &item.error {
                lines.push(Line::from(Span::styled(
                    format!("             {error}"),
                    Style::default().fg(Color::Red),
                )));
            }
        }
        lines.push(Line::from(""));
    }
    if record.items.is_empty() {
        lines.push(Line::from("Nothing was processed."));
    }
    lines.push(Line::from(Span::styled(
        totals_line(record),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if record.bytes_trashed() > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "{} is in the Trash and still occupies disk until you empty it.",
                bytes(record.bytes_trashed())
            ),
            Style::default().fg(Color::Yellow),
        )));
    }
    lines
}

/// The results screen: grouped outcomes, scrollable.
pub(super) fn draw_results(
    frame: &mut Frame,
    area: Rect,
    record: &RunRecord,
    paths: &Paths,
    scroll: u16,
) {
    frame.render_widget(Clear, area);
    let title = if record.dry_run {
        " dry run results "
    } else {
        " results "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_bottom(Line::from(" ↑ ↓ scroll · any other key to close ").right_aligned());
    frame.render_widget(
        Paragraph::new(results_lines(record, paths))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0))
            .block(block),
        area,
    );
}
