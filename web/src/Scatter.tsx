// Size against staleness.
//
// The point of this chart is the top-right quadrant: big *and* untouched for a
// long time is the definition of an easy win, and it is much faster to see than
// to read off a sorted list. Colour carries risk, so a red dot far right is the
// thing to think twice about.

import { useMemo } from "react";
import type { Candidate } from "./api";
import { formatBytes } from "./api";

interface Props {
  candidates: Candidate[];
  selected: Set<string>;
  onSelect: (id: string) => void;
  width: number;
  height: number;
}

const MARGIN = { top: 12, right: 16, bottom: 34, left: 58 };

export function Scatter({ candidates, selected, onSelect, width, height }: Props) {
  const points = useMemo(() => {
    const usable = candidates.filter((c) => c.on_disk > 0);
    if (!usable.length) return null;

    const innerW = width - MARGIN.left - MARGIN.right;
    const innerH = height - MARGIN.top - MARGIN.bottom;

    const maxBytes = Math.max(...usable.map((c) => c.on_disk));
    const maxRadius = 18;

    // Pad the domains so the largest and stalest points are not drawn half
    // outside the plot area — those are exactly the ones the user cares about.
    const dataMaxDays = Math.max(30, ...usable.map((c) => c.last_used_days ?? 0));
    const maxDays = dataMaxDays * 1.08;

    // Log scale on size: caches span kilobytes to tens of gigabytes, and a
    // linear axis would collapse everything except the single largest item.
    const logMax = Math.log10(maxBytes + 1);
    const plotW = innerW - maxRadius;
    const plotH = innerH - maxRadius;

    return {
      innerW,
      innerH,
      maxDays,
      maxBytes,
      items: usable.map((c) => ({
        candidate: c,
        x: ((c.last_used_days ?? 0) / maxDays) * plotW,
        y: innerH - (Math.log10(c.on_disk + 1) / logMax) * plotH,
        r: Math.max(4, Math.min(maxRadius, Math.sqrt(c.on_disk / maxBytes) * 22)),
      })),
    };
  }, [candidates, width, height]);

  if (!points) return <p className="empty">Nothing to plot yet.</p>;

  const xTicks = [0, 0.25, 0.5, 0.75, 1].map((f) => ({
    at: f * points.innerW,
    label: `${Math.round(f * points.maxDays)}d`,
  }));

  return (
    <svg width={width} height={height} role="img" aria-label="Size against time since last use">
      <g transform={`translate(${MARGIN.left},${MARGIN.top})`}>
        <rect
          x={points.innerW * 0.5}
          y={0}
          width={points.innerW * 0.5}
          height={points.innerH * 0.5}
          className="scatter-quadrant"
        />
        <text x={points.innerW * 0.5 + 8} y={points.innerH * 0.5 - 8} className="scatter-quadrant-label">
          big and unused — the easy wins
        </text>

        <line x1={0} y1={points.innerH} x2={points.innerW} y2={points.innerH} className="axis" />
        <line x1={0} y1={0} x2={0} y2={points.innerH} className="axis" />

        {xTicks.map((tick) => (
          <text key={tick.at} x={tick.at} y={points.innerH + 18} className="tick">
            {tick.label}
          </text>
        ))}
        <text
          x={points.innerW / 2}
          y={points.innerH + 32}
          className="axis-title"
          textAnchor="middle"
        >
          days since last used
        </text>
        <text transform={`translate(-42,${points.innerH / 2}) rotate(-90)`} className="axis-title">
          size (log)
        </text>

        {points.items.map(({ candidate, x, y, r }) => (
          <circle
            key={candidate.id}
            cx={x}
            cy={y}
            r={r}
            className={`dot ${candidate.tier} ${selected.has(candidate.id) ? "selected" : ""}`}
            onClick={() => onSelect(candidate.id)}
          >
            <title>
              {`${candidate.label}\n${formatBytes(candidate.on_disk)} · ${candidate.last_used_human} · ${candidate.tier}`}
            </title>
          </circle>
        ))}
      </g>
    </svg>
  );
}
