//! Renders a `HistoryReport` as a single self-contained HTML file.
//!
//! No JS, no external assets, no charting library: every chart is hand-built
//! SVG computed at render time, matching how the web UI's treemap and scatter
//! views work. That keeps the report a plain file you can email or open years
//! from now with nothing to fetch and nothing to break.

use std::fmt::Write as _;

use reclaim_core::format::{bytes, relative_time};
use reclaim_core::journal::Trigger;
use reclaim_core::model::Tier;
use reclaim_core::report::{FailureEntry, GroupStats, HistoryReport, RunSummary, TopItem};

pub fn render(report: &HistoryReport, version: &str) -> String {
    let mut out = String::new();
    out.push_str("<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    out.push_str("<title>reclaim — history report</title>\n<style>");
    out.push_str(STYLE);
    out.push_str("</style></head><body>");

    write_header(&mut out, report, version);
    write_stats(&mut out, report);
    write_timeline(&mut out, report);
    write_by_group(&mut out, report);
    write_by_trigger(&mut out, report);
    write_top_items(&mut out, report);
    write_failures(&mut out, report);
    write_runs(&mut out, report);

    out.push_str("</body></html>");
    out
}

fn write_header(out: &mut String, report: &HistoryReport, version: &str) {
    let _ = writeln!(
        out,
        "<header><h1>reclaim history report</h1>\
         <p class=\"dim\">Generated {} · reclaim {} · {} run(s) in this journal ({} dry run{})</p></header>",
        esc(&relative_time(report.generated_at)),
        esc(version),
        report.runs,
        report.dry_runs,
        if report.dry_runs == 1 { "" } else { "s" },
    );
}

fn write_stats(out: &mut String, report: &HistoryReport) {
    out.push_str("<section class=\"stats\">");
    stat_card(out, "Lifetime freed", &bytes(report.lifetime_freed), "safe");
    stat_card(
        out,
        "In the Trash",
        &bytes(report.lifetime_trashed),
        "review",
    );
    stat_card(
        out,
        "Items found",
        &thousands(report.lifetime_candidates_found),
        "",
    );
    stat_card(out, "Real runs", &thousands(report.real_runs as u64), "");
    let failed_class = if report.failed_items > 0 { "caution" } else { "" };
    stat_card(
        out,
        "Failed items",
        &thousands(report.failed_items as u64),
        failed_class,
    );
    out.push_str("</section>");
}

fn stat_card(out: &mut String, label: &str, value: &str, accent: &str) {
    let _ = writeln!(
        out,
        "<div class=\"card {}\"><div class=\"card-value\">{}</div><div class=\"card-label\">{}</div></div>",
        accent,
        esc(value),
        esc(label)
    );
}

const CHART_W: f64 = 720.0;
const CHART_H: f64 = 180.0;
const CHART_PAD: f64 = 28.0;

fn write_timeline(out: &mut String, report: &HistoryReport) {
    out.push_str("<section><h2>Freed over time</h2>");
    if report.timeline.len() < 2 {
        out.push_str("<p class=\"dim\">Not enough real runs yet to plot a trend.</p></section>");
        return;
    }

    let points = &report.timeline;
    let t0 = epoch_secs(points.first().unwrap().started_at);
    let t1 = epoch_secs(points.last().unwrap().started_at);
    let span = (t1 - t0).max(1.0);
    let max_cumulative = points
        .iter()
        .map(|p| p.cumulative_freed)
        .max()
        .unwrap_or(1)
        .max(1) as f64;

    let x = |t: f64| CHART_PAD + (t - t0) / span * (CHART_W - 2.0 * CHART_PAD);
    let y = |v: u64| CHART_H - CHART_PAD - (v as f64 / max_cumulative) * (CHART_H - 2.0 * CHART_PAD);

    let mut path = String::new();
    let mut area = String::new();
    for (i, p) in points.iter().enumerate() {
        let px = x(epoch_secs(p.started_at));
        let py = y(p.cumulative_freed);
        if i == 0 {
            let _ = write!(path, "M{px:.1},{py:.1}");
            let _ = write!(area, "M{px:.1},{:.1} L{px:.1},{py:.1}", CHART_H - CHART_PAD);
        } else {
            let _ = write!(path, " L{px:.1},{py:.1}");
            let _ = write!(area, " L{px:.1},{py:.1}");
        }
    }
    let last_x = x(epoch_secs(points.last().unwrap().started_at));
    let _ = write!(area, " L{last_x:.1},{:.1} Z", CHART_H - CHART_PAD);

    let _ = writeln!(
        out,
        "<svg viewBox=\"0 0 {CHART_W} {CHART_H}\" class=\"chart\" role=\"img\" aria-label=\"Cumulative bytes freed over time\">\
         <line x1=\"{pad}\" y1=\"{base:.1}\" x2=\"{w:.1}\" y2=\"{base:.1}\" class=\"axis\" />\
         <path d=\"{area}\" class=\"chart-area\" />\
         <path d=\"{path}\" class=\"chart-line\" />",
        pad = CHART_PAD,
        base = CHART_H - CHART_PAD,
        w = CHART_W - CHART_PAD,
    );
    for p in points {
        let _ = writeln!(
            out,
            "<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"3\" class=\"chart-dot\"><title>{} · {} freed (running total {})</title></circle>",
            x(epoch_secs(p.started_at)),
            y(p.cumulative_freed),
            esc(&relative_time(p.started_at)),
            esc(&bytes(p.freed)),
            esc(&bytes(p.cumulative_freed)),
        );
    }
    out.push_str("</svg>");
    let _ = writeln!(
        out,
        "<p class=\"dim small\">{} total across {} real run(s), from {} to {}</p></section>",
        esc(&bytes(max_cumulative as u64)),
        points.len(),
        esc(&relative_time(points.first().unwrap().started_at)),
        esc(&relative_time(points.last().unwrap().started_at)),
    );
}

fn epoch_secs(t: std::time::SystemTime) -> f64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn write_by_group(out: &mut String, report: &HistoryReport) {
    out.push_str("<section><h2>By ecosystem</h2>");
    if report.by_group.is_empty() {
        out.push_str("<p class=\"dim\">Nothing reclaimed yet.</p></section>");
        return;
    }
    let max = report
        .by_group
        .iter()
        .map(|g| g.freed + g.trashed)
        .max()
        .unwrap_or(1)
        .max(1) as f64;

    out.push_str("<div class=\"bars\">");
    for g in &report.by_group {
        write_group_bar(out, g, max);
    }
    out.push_str("</div></section>");
}

fn write_group_bar(out: &mut String, g: &GroupStats, max: f64) {
    let freed_pct = g.freed as f64 / max * 100.0;
    let trashed_pct = g.trashed as f64 / max * 100.0;
    let _ = writeln!(
        out,
        "<div class=\"bar-row\"><div class=\"bar-label\">{} <span class=\"dim small\">({} item{})</span></div>\
         <div class=\"bar-track\"><div class=\"bar-fill freed\" style=\"width:{freed_pct:.2}%\"></div>\
         <div class=\"bar-fill trashed\" style=\"width:{trashed_pct:.2}%\"></div></div>\
         <div class=\"bar-value\">{}{}</div></div>",
        esc(&g.title),
        g.items,
        if g.items == 1 { "" } else { "s" },
        esc(&bytes(g.freed)),
        if g.trashed > 0 {
            format!(" <span class=\"dim small\">+{} trashed</span>", bytes(g.trashed))
        } else {
            String::new()
        },
    );
}

fn write_by_trigger(out: &mut String, report: &HistoryReport) {
    out.push_str("<section><h2>By trigger</h2>\n<table><thead><tr><th>Trigger</th><th>Runs</th><th>Freed</th></tr></thead><tbody>");
    for t in &report.by_trigger {
        let _ = writeln!(
            out,
            "<tr><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
            esc(&t.label),
            t.runs,
            esc(&bytes(t.freed))
        );
    }
    out.push_str("</tbody></table></section>");
}

fn write_top_items(out: &mut String, report: &HistoryReport) {
    out.push_str("<section><h2>Biggest reclaims ever</h2>");
    if report.top_items.is_empty() {
        out.push_str("<p class=\"dim\">Nothing reclaimed yet.</p></section>");
        return;
    }
    out.push_str("<table><thead><tr><th>What</th><th>Ecosystem</th><th>Tier</th><th>Size</th><th>When</th></tr></thead><tbody>");
    for item in &report.top_items {
        write_top_item_row(out, item);
    }
    out.push_str("</tbody></table></section>");
}

fn write_top_item_row(out: &mut String, item: &TopItem) {
    let _ = writeln!(
        out,
        "<tr><td>{}</td><td>{}</td><td><span class=\"pill {}\">{}</span></td><td class=\"num\">{}</td><td class=\"dim\">{}</td></tr>",
        esc(&item.label),
        esc(item.group.title()),
        tier_class(item.tier),
        tier_label(item.tier),
        esc(&bytes(item.bytes)),
        esc(&relative_time(item.started_at)),
    );
}

fn write_failures(out: &mut String, report: &HistoryReport) {
    out.push_str("<section><h2>Failures</h2>");
    if report.failures.is_empty() {
        out.push_str("<p class=\"dim\">No failures recorded. Nothing to investigate.</p></section>");
        return;
    }
    out.push_str("<table><thead><tr><th>When</th><th>What</th><th>Provider</th><th>Error</th></tr></thead><tbody>");
    for f in &report.failures {
        write_failure_row(out, f);
    }
    out.push_str("</tbody></table></section>");
}

fn write_failure_row(out: &mut String, f: &FailureEntry) {
    let _ = writeln!(
        out,
        "<tr class=\"failure\"><td class=\"dim\">{}</td><td>{}</td><td class=\"dim\">{}</td><td>{}</td></tr>",
        esc(&relative_time(f.started_at)),
        esc(&f.label),
        esc(&f.provider),
        esc(&f.error),
    );
}

fn write_runs(out: &mut String, report: &HistoryReport) {
    out.push_str("<section><h2>Every run</h2>\n<div class=\"scroll\">");
    out.push_str("<table><thead><tr><th>When</th><th>Trigger</th><th>Freed</th><th>Trashed</th><th>Items</th><th>Failures</th><th>Status</th></tr></thead><tbody>");
    for r in &report.runs_detail {
        write_run_row(out, r);
    }
    out.push_str("</tbody></table></div></section>");
}

fn write_run_row(out: &mut String, r: &RunSummary) {
    let status = if r.dry_run {
        "<span class=\"pill\">dry run</span>".to_string()
    } else if r.succeeded {
        "<span class=\"pill safe\">ok</span>".to_string()
    } else {
        "<span class=\"pill caution\">failed</span>".to_string()
    };
    let _ = writeln!(
        out,
        "<tr><td class=\"dim\">{}</td><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td>{}</td></tr>",
        esc(&relative_time(r.started_at)),
        esc(trigger_label(r.trigger)),
        esc(&bytes(r.freed)),
        esc(&bytes(r.trashed)),
        r.items,
        r.failures,
        status,
    );
}

fn trigger_label(t: Trigger) -> &'static str {
    t.label()
}

fn tier_class(t: Tier) -> &'static str {
    t.as_str()
}

fn tier_label(t: Tier) -> &'static str {
    t.as_str()
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

/// Escapes text pulled from candidate labels, provider ids and error strings
/// before it goes into the HTML: all three ultimately derive from filesystem
/// paths or command output, which the report does not otherwise sanitise.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

const STYLE: &str = r#"
:root {
  --bg: #f6f7f9; --surface: #fff; --border: #dfe3e8; --text: #12161c; --dim: #5f6b7a;
  --safe: #2f9e5e; --review: #c98a12; --caution: #cf3f3f; --accent: #2f6fdb;
}
@media (prefers-color-scheme: dark) {
  :root { --bg: #14171c; --surface: #1c2027; --border: #2c323b; --text: #e6e9ee; --dim: #99a3b1;
    --safe: #4cc37e; --review: #e0a838; --caution: #f2645f; --accent: #6aa1ff; }
}
* { box-sizing: border-box; }
body { margin: 0; padding: 2rem; background: var(--bg); color: var(--text);
  font: 14px/1.5 -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
header { margin-bottom: 1.5rem; }
h1 { margin: 0 0 0.2rem; font-size: 1.4rem; }
h2 { font-size: 1rem; margin: 0 0 0.6rem; }
.dim { color: var(--dim); }
.small { font-size: 0.8em; }
section { max-width: 900px; margin: 0 auto 1.8rem; background: var(--surface);
  border: 1px solid var(--border); border-radius: 10px; padding: 1rem 1.2rem; }
header { max-width: 900px; margin-left: auto; margin-right: auto; }
.stats { max-width: 900px; margin: 0 auto 1.8rem; display: grid;
  grid-template-columns: repeat(auto-fit, minmax(140px, 1fr)); gap: 0.75rem; background: none;
  border: none; padding: 0; }
.card { background: var(--surface); border: 1px solid var(--border); border-radius: 10px;
  padding: 0.8rem; text-align: center; }
.card-value { font-size: 1.3rem; font-weight: 700; }
.card.safe .card-value { color: var(--safe); }
.card.review .card-value { color: var(--review); }
.card.caution .card-value { color: var(--caution); }
.card-label { color: var(--dim); font-size: 0.78rem; margin-top: 0.15rem; }
table { width: 100%; border-collapse: collapse; font-size: 0.86rem; }
th, td { text-align: left; padding: 0.35rem 0.5rem; border-bottom: 1px solid var(--border); }
th { color: var(--dim); font-weight: 600; }
td.num { font-variant-numeric: tabular-nums; white-space: nowrap; }
tr.failure td { color: var(--caution); }
.scroll { overflow-x: auto; }
.pill { display: inline-block; padding: 0.05rem 0.45rem; border-radius: 999px;
  font-size: 0.75rem; border: 1px solid currentColor; color: var(--dim); }
.pill.safe { color: var(--safe); }
.pill.review { color: var(--review); }
.pill.caution { color: var(--caution); }
.chart { width: 100%; height: auto; }
.axis { stroke: var(--border); }
.chart-area { fill: color-mix(in srgb, var(--accent) 18%, transparent); stroke: none; }
.chart-line { fill: none; stroke: var(--accent); stroke-width: 2; }
.chart-dot { fill: var(--accent); }
.bars { display: flex; flex-direction: column; gap: 0.55rem; }
.bar-row { display: grid; grid-template-columns: 11rem 1fr 8rem; align-items: center; gap: 0.6rem; }
.bar-track { display: flex; height: 12px; background: var(--bg); border-radius: 6px; overflow: hidden; }
.bar-fill.freed { background: var(--safe); }
.bar-fill.trashed { background: var(--review); opacity: 0.7; }
.bar-value { text-align: right; font-variant-numeric: tabular-nums; white-space: nowrap; }
"#;
