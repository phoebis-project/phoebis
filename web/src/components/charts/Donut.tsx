import { useState } from 'react';

export interface DonutSlice {
  color: string;
  id: string;
  label: string;
  /** Hover text for the slice; defaults to the slice's share of the ring. */
  title?: string;
  value: number;
}

/**
 * Part-to-whole ring. Each slice is drawn as `value / total` of the ring and
 * the gray remainder is `total - sum(values)`. For quota, a slice is an
 * account's remaining share weighed by its tier's capacity, the total the
 * pooled capacity, and the gray arc what has been spent.
 */
export function Donut({ slices, total, centerLabel, centerValue, size = 148, onHover }: { centerLabel: string; centerValue: string; onHover?: (id: string | null) => void; size?: number; slices: DonutSlice[]; total: number }) {
  const [hover, setHover] = useState<string | null>(null);
  const r = size / 2 - 10;
  const c = 2 * Math.PI * r;
  const gap = 2;
  let offset = 0;
  const sum = slices.reduce((s, x) => s + x.value, 0);
  const spent = Math.max(0, total - sum);
  const all: DonutSlice[] = [...slices, { id: '__spent', label: 'Spent', value: spent, color: 'var(--series-muted)', title: `Spent: ${Math.round((total > 0 ? spent / total : 0) * 100)}% of pooled capacity` }];
  return (
    <svg className="donut" width={size} height={size} viewBox={`0 0 ${size} ${size}`} role="img" aria-label={`${centerLabel}: ${centerValue}`}>
      <g transform={`rotate(-90 ${size / 2} ${size / 2})`}>
        {all.map((s) => {
          const share = total > 0 ? s.value / total : 0;
          const len = Math.max(0, share * c - gap);
          const dash = `${len} ${c - len}`;
          const el = (
            <circle
              key={s.id}
              cx={size / 2}
              cy={size / 2}
              r={r}
              fill="none"
              stroke={s.color}
              strokeWidth={hover === s.id ? 16 : 12}
              strokeDasharray={dash}
              strokeDashoffset={-offset}
              opacity={hover && hover !== s.id ? 0.45 : 1}
              style={{ transition: 'stroke-width 120ms, opacity 120ms' }}
              onMouseEnter={() => {
                setHover(s.id);
                onHover?.(s.id);
              }}
              onMouseLeave={() => {
                setHover(null);
                onHover?.(null);
              }}
            >
              <title>{s.title ?? `${s.label}: ${Math.round(share * 100)}%`}</title>
            </circle>
          );
          offset += share * c;
          return el;
        })}
      </g>
      <text x="50%" y="46%" textAnchor="middle" className="donut__label">
        {centerLabel}
      </text>
      <text x="50%" y="62%" textAnchor="middle" className="donut__value">
        {centerValue}
      </text>
    </svg>
  );
}
