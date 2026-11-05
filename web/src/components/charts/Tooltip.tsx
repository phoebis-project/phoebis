import type { ReactNode } from 'react';
import { fmtAbsolute } from '@/lib/format';

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

function TimeTip<T extends { at: number }>({ payload, rows }: { payload?: readonly { payload?: unknown }[]; rows: (p: T) => TooltipRow[] }) {
  const p = payload?.[0]?.payload as T | undefined;
  if (!p) return null;
  return <ChartTooltip title={fmtAbsolute(p.at)} rows={rows(p)} />;
}

/**
 * A time chart's tooltip content: the hovered point's instant as the title over the rows `rows` builds from it. An
 * element, which recharts clones with the hovered payload, so the tooltip keeps one component type across renders and
 * a shown one updates in place.
 */
export function timeTip<T extends { at: number }>(rows: (p: T) => TooltipRow[]) {
  return <TimeTip<T> rows={rows} />;
}

/**
 * `hollow` draws the swatch as an outline in the color, as a chart's hollow dot marks a point: a line swatch grows to
 * 4 px, since under border-box a 1 px border on each side of a 2 px swatch would leave no interior.
 */
export function Legend({ items }: { items: { color: string; dashed?: boolean; hollow?: boolean; label: string; shape?: 'line' | 'rect'; title?: string; value?: ReactNode }[] }) {
  return (
    <ul className="legend">
      {items.map((i) => (
        <li key={i.label} title={i.title}>
          <span
            className={`legend__swatch legend__swatch--${i.shape ?? 'rect'}`}
            style={i.hollow ? { background: 'transparent', border: `1px solid ${i.color}`, height: i.shape === 'line' ? 4 : undefined } : { background: i.dashed ? `repeating-linear-gradient(90deg, ${i.color} 0 4px, transparent 4px 7px)` : i.color }}
          />
          {i.label}
          {i.value !== undefined && <span className="legend__value num">{i.value}</span>}
        </li>
      ))}
    </ul>
  );
}
