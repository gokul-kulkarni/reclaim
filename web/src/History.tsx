// What has reclaim actually done for me, over the whole journal — not just
// the last scan. Lifetime totals, a trend, a breakdown by ecosystem and
// trigger, the biggest reclaims ever, and any failures worth investigating.

import { useMemo } from "react";
import type { HistoryReport, GroupStats } from "./api";
import { formatBytes } from "./api";

interface Props {
  report: HistoryReport | null;
  loading: boolean;
  error: string | null;
  onRefresh: () => void;
}

export function History({ report, loading, error, onRefresh }: Props) {
  if (error) {
    return (
      <div className="banner error">
        <strong>Error.</strong> {error}
      </div>
    );
  }

  if (loading && !report) {
    return (
      <div className="scan-loading">
        <span className="spinner" aria-hidden="true" />
        <p>Loading history…</p>
      </div>
    );
  }

  if (!report || report.runs === 0) {
    return (
      <p className="empty">
        No runs recorded yet. Clean something and this tab will start tracking it.
      </p>
    );
  }

  return (
    <div className="history">
      <div className="history-head">
        <p className="dim small">
          {report.runs} run{report.runs === 1 ? "" : "s"} recorded
          {report.dry_runs > 0 && ` (${report.dry_runs} dry run${report.dry_runs === 1 ? "" : "s"})`}
        </p>
        <button onClick={onRefresh} disabled={loading}>
          {loading ? "Refreshing…" : "Refresh"}
        </button>
      </div>

      <div className="stats">
        <StatCard label="Lifetime freed" value={formatBytes(report.lifetime_freed)} accent="safe" />
        <StatCard label="In the Trash" value={formatBytes(report.lifetime_trashed)} accent="review" />
        <StatCard label="Items found" value={report.lifetime_candidates_found.toLocaleString()} />
        <StatCard label="Real runs" value={report.real_runs.toLocaleString()} />
        <StatCard
          label="Failed items"
          value={report.failed_items.toLocaleString()}
          accent={report.failed_items > 0 ? "caution" : undefined}
        />
      </div>

      <section>
        <h3>Freed over time</h3>
        <Timeline points={report.timeline} />
      </section>

      <section>
        <h3>By ecosystem</h3>
        {report.by_group.length === 0 ? (
          <p className="dim small">Nothing reclaimed yet.</p>
        ) : (
          <div className="bars">
            {report.by_group.map((g) => (
              <GroupBar key={g.group} group={g} max={maxGroupTotal(report.by_group)} />
            ))}
          </div>
        )}
      </section>

      <div className="history-cols">
        <section>
          <h3>By trigger</h3>
          <table className="grid">
            <thead>
              <tr>
                <th>Trigger</th>
                <th>Runs</th>
                <th>Freed</th>
              </tr>
            </thead>
            <tbody>
              {report.by_trigger.map((t) => (
                <tr key={t.label}>
                  <td>{t.label}</td>
                  <td className="num">{t.runs}</td>
                  <td className="num">{formatBytes(t.freed)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </section>

        <section>
          <h3>Biggest reclaims ever</h3>
          {report.top_items.length === 0 ? (
            <p className="dim small">Nothing reclaimed yet.</p>
          ) : (
            <table className="grid">
              <thead>
                <tr>
                  <th>What</th>
                  <th>Size</th>
                  <th>When</th>
                </tr>
              </thead>
              <tbody>
                {report.top_items.slice(0, 8).map((item, i) => (
                  <tr key={i}>
                    <td>
                      {item.label} <span className="dim small">· {item.group_title}</span>
                    </td>
                    <td className="num">{formatBytes(item.bytes)}</td>
                    <td className="dim">{item.when}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </section>
      </div>

      <section>
        <h3>Failures</h3>
        {report.failures.length === 0 ? (
          <p className="dim small">No failures recorded. Nothing to investigate.</p>
        ) : (
          <table className="grid">
            <thead>
              <tr>
                <th>When</th>
                <th>What</th>
                <th>Provider</th>
                <th>Error</th>
              </tr>
            </thead>
            <tbody>
              {report.failures.map((f, i) => (
                <tr key={i} className="failure-row">
                  <td className="dim">{f.when}</td>
                  <td>{f.label}</td>
                  <td className="dim">{f.provider}</td>
                  <td>{f.error}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>

      <section>
        <h3>Every run</h3>
        <table className="grid">
          <thead>
            <tr>
              <th>When</th>
              <th>Trigger</th>
              <th>Freed</th>
              <th>Trashed</th>
              <th>Items</th>
              <th>Status</th>
            </tr>
          </thead>
          <tbody>
            {report.runs_detail.map((r) => (
              <tr key={r.id}>
                <td className="dim">{r.when}</td>
                <td>{r.trigger}</td>
                <td className="num">{formatBytes(r.freed)}</td>
                <td className="num">{formatBytes(r.trashed)}</td>
                <td className="num">{r.items}</td>
                <td>
                  {r.dry_run ? (
                    <span className="pill">dry run</span>
                  ) : r.succeeded ? (
                    <span className="pill safe">ok</span>
                  ) : (
                    <span className="pill caution">{r.failures} failed</span>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </section>
    </div>
  );
}

function StatCard({ label, value, accent }: { label: string; value: string; accent?: string }) {
  return (
    <div className={`card${accent ? ` ${accent}` : ""}`}>
      <div className="card-value">{value}</div>
      <div className="card-label">{label}</div>
    </div>
  );
}

function maxGroupTotal(groups: GroupStats[]): number {
  return Math.max(1, ...groups.map((g) => g.freed + g.trashed));
}

function GroupBar({ group, max }: { group: GroupStats; max: number }) {
  const freedPct = (group.freed / max) * 100;
  const trashedPct = (group.trashed / max) * 100;
  return (
    <div className="bar-row">
      <div className="bar-label">
        {group.title} <span className="dim small">({group.items})</span>
      </div>
      <div className="bar-track">
        <div className="bar-fill freed" style={{ width: `${freedPct}%` }} />
        <div className="bar-fill trashed" style={{ width: `${trashedPct}%` }} />
      </div>
      <div className="bar-value">{formatBytes(group.freed)}</div>
    </div>
  );
}

const CHART_W = 720;
const CHART_H = 160;
const CHART_PAD = 28;

function Timeline({ points }: { points: HistoryReport["timeline"] }) {
  const layout = useMemo(() => {
    if (points.length < 2) return null;

    const t0 = points[0].started_at_ms;
    const t1 = points[points.length - 1].started_at_ms;
    const span = Math.max(1, t1 - t0);
    const maxCumulative = Math.max(1, ...points.map((p) => p.cumulative_freed));

    const x = (t: number) => CHART_PAD + ((t - t0) / span) * (CHART_W - 2 * CHART_PAD);
    const y = (v: number) =>
      CHART_H - CHART_PAD - (v / maxCumulative) * (CHART_H - 2 * CHART_PAD);

    const coords = points.map((p) => ({ x: x(p.started_at_ms), y: y(p.cumulative_freed), p }));
    const line = coords.map((c, i) => `${i === 0 ? "M" : "L"}${c.x.toFixed(1)},${c.y.toFixed(1)}`).join(" ");
    const area =
      `M${coords[0].x.toFixed(1)},${(CHART_H - CHART_PAD).toFixed(1)} ` +
      coords.map((c) => `L${c.x.toFixed(1)},${c.y.toFixed(1)}`).join(" ") +
      ` L${coords[coords.length - 1].x.toFixed(1)},${(CHART_H - CHART_PAD).toFixed(1)} Z`;

    return { coords, line, area };
  }, [points]);

  if (!layout) {
    return <p className="dim small">Not enough real runs yet to plot a trend.</p>;
  }

  return (
    <svg
      viewBox={`0 0 ${CHART_W} ${CHART_H}`}
      className="chart"
      role="img"
      aria-label="Cumulative bytes freed over time"
    >
      <line x1={CHART_PAD} y1={CHART_H - CHART_PAD} x2={CHART_W - CHART_PAD} y2={CHART_H - CHART_PAD} className="axis" />
      <path d={layout.area} className="chart-area" />
      <path d={layout.line} className="chart-line" />
      {layout.coords.map((c, i) => (
        <circle key={i} cx={c.x} cy={c.y} r={3} className="chart-dot">
          <title>
            {c.p.when} · {formatBytes(c.p.freed)} freed (running total {formatBytes(c.p.cumulative_freed)})
          </title>
        </circle>
      ))}
    </svg>
  );
}
