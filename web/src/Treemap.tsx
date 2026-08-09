// Disk usage as a treemap: ecosystem -> candidate, area proportional to bytes.
//
// This is the one thing the terminal genuinely cannot do. A sorted table tells
// you the largest item; a treemap tells you at a glance how your disk is
// actually divided, and which ecosystem is quietly responsible for half of it.

import { useMemo } from "react";
import { hierarchy, treemap, treemapSquarify } from "d3-hierarchy";
import type { Candidate } from "./api";
import { formatBytes } from "./api";

interface Props {
  candidates: Candidate[];
  selected: Set<string>;
  onSelect: (id: string) => void;
  onInspect: (id: string) => void;
  width: number;
  height: number;
}

interface Node {
  name: string;
  id?: string;
  tier?: string;
  value?: number;
  children?: Node[];
}

const TIER_FILL: Record<string, string> = {
  safe: "var(--safe)",
  review: "var(--review)",
  caution: "var(--caution)",
};

export function Treemap({ candidates, selected, onSelect, onInspect, width, height }: Props) {
  const layout = useMemo(() => {
    const byGroup = new Map<string, Candidate[]>();
    for (const candidate of candidates) {
      if (candidate.on_disk <= 0) continue;
      const list = byGroup.get(candidate.group_title) ?? [];
      list.push(candidate);
      byGroup.set(candidate.group_title, list);
    }

    const root: Node = {
      name: "root",
      children: [...byGroup.entries()].map(([title, items]) => ({
        name: title,
        children: items.map((c) => ({
          name: c.label,
          id: c.id,
          tier: c.tier,
          value: c.on_disk,
        })),
      })),
    };

    if (!root.children?.length) return null;

    const nodes = hierarchy<Node>(root)
      .sum((d) => d.value ?? 0)
      .sort((a, b) => (b.value ?? 0) - (a.value ?? 0));

    return treemap<Node>()
      .tile(treemapSquarify)
      .size([width, height])
      .paddingInner(1)
      .paddingTop(18)
      .round(true)(nodes);
  }, [candidates, width, height]);

  if (!layout) {
    return <p className="empty">Nothing to show yet.</p>;
  }

  return (
    <svg width={width} height={height} role="img" aria-label="Disk usage by ecosystem">
      {layout.descendants().map((node, index) => {
        const w = node.x1 - node.x0;
        const h = node.y1 - node.y0;
        if (w <= 0 || h <= 0) return null;

        // Group headings.
        if (node.depth === 1) {
          return (
            <g key={`g${index}`} transform={`translate(${node.x0},${node.y0})`}>
              <rect width={w} height={h} className="treemap-group" />
              {w > 60 && (
                <text x={4} y={13} className="treemap-group-label">
                  {node.data.name} · {formatBytes(node.value ?? 0)}
                </text>
              )}
            </g>
          );
        }

        if (node.depth !== 2 || !node.data.id) return null;
        const id = node.data.id;
        const isSelected = selected.has(id);

        return (
          <g
            key={id}
            transform={`translate(${node.x0},${node.y0})`}
            onClick={() => onSelect(id)}
            onDoubleClick={() => onInspect(id)}
            className="treemap-cell"
          >
            <title>
              {`${node.data.name}\n${formatBytes(node.value ?? 0)} · ${node.data.tier}\n` +
                `click to select, double-click for detail`}
            </title>
            <rect
              width={w}
              height={h}
              fill={TIER_FILL[node.data.tier ?? "safe"]}
              className={isSelected ? "treemap-rect selected" : "treemap-rect"}
            />
            {w > 54 && h > 20 && (
              <>
                <text x={4} y={13} className="treemap-label">
                  {node.data.name}
                </text>
                {h > 32 && (
                  <text x={4} y={26} className="treemap-sub">
                    {formatBytes(node.value ?? 0)}
                  </text>
                )}
              </>
            )}
          </g>
        );
      })}
    </svg>
  );
}
