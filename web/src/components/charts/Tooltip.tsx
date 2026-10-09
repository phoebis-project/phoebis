import type { ReactNode } from 'react';

export interface TooltipRow {
  color?: string;
  label: string;
  /** Hover hint on the label, e.g. the precision of a percentile. */
  title?: string;
  value: ReactNode;
}

export function ChartTooltip({ title, rows }: { rows: TooltipRow[]; title: ReactNode }) {
  return (
    <div className="chart-tip">
      <div className="chart-tip__title">{title}</div>
      {rows.map((r) => (
        <div key={r.label} className="chart-tip__row">
          {/* Drawn on every row so an uncolored row keeps its value and label in the colored rows' columns. */}
          <span className="chart-tip__key" style={r.color ? { background: r.color } : undefined} />
          <span className="chart-tip__value num">{r.value}</span>
          <span className="chart-tip__label" title={r.title}>
            {r.label}
          </span>
        </div>
      ))}
    </div>
  );
}

export function Legend({ items }: { items: { color: string; dashed?: boolean; label: string; shape?: 'line' | 'rect'; title?: string; value?: ReactNode }[] }) {
  return (
    <ul className="legend">
      {items.map((i) => (
        <li key={i.label} title={i.title}>
          <span className={`legend__swatch legend__swatch--${i.shape ?? 'rect'}`} style={{ background: i.dashed ? `repeating-linear-gradient(90deg, ${i.color} 0 4px, transparent 4px 7px)` : i.color }} />
          {i.label}
          {i.value !== undefined && <span className="legend__value num">{i.value}</span>}
        </li>
      ))}
    </ul>
  );
}
