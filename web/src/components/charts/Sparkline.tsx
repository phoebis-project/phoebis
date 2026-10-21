import { useMemo } from 'react';

export function Sparkline({ values, width = 160, height = 36, color, fill = true }: { color: string; fill?: boolean; height?: number; values: number[]; width?: number }) {
  const path = useMemo(() => {
    if (values.length < 2) return { area: '', line: '' };
    // The two floors at 1 stand together: the dot's keeps `last / max` finite on an all-zero series, and this one keeps
    // a series under 1 (the Dashboard's cost spark in dollars per hour) from rescaling to its own peak.
    const max = Math.max(...values, 1);
    const min = 0;
    const stepX = width / (values.length - 1);
    const pts = values.map((v, i) => [i * stepX, height - 2 - ((v - min) / (max - min || 1)) * (height - 4)] as const);
    const line = pts.map(([x, y], i) => `${i === 0 ? 'M' : 'L'}${x.toFixed(1)} ${y.toFixed(1)}`).join(' ');
    const area = `${line} L${width} ${height} L0 ${height} Z`;
    return { area, line };
  }, [values, width, height]);
  const last = values[values.length - 1] ?? 0;
  const max = Math.max(...values, 1);
  const lastY = height - 2 - (last / max) * (height - 4);
  return (
    <svg className="sparkline" viewBox={`0 0 ${width} ${height}`} width="100%" height={height} preserveAspectRatio="none" aria-hidden>
      {fill && <path d={path.area} fill={color} opacity={0.1} />}
      <path d={path.line} fill="none" stroke={color} strokeWidth={1.75} strokeLinejoin="round" strokeLinecap="round" vectorEffect="non-scaling-stroke" />
      <circle cx={width} cy={lastY} r={3} fill={color} />
    </svg>
  );
}
