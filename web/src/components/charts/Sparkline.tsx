import { useMemo } from 'react';

/** A null value is an hour with no figure to draw, which the line leaves as a gap rather than reading it as zero. */
export function Sparkline({ values, width = 160, height = 36, color, fill = true }: { color: string; fill?: boolean; height?: number; values: (number | null)[]; width?: number }) {
  const drawn = values.filter((v): v is number => v !== null);
  // The two floors at 1 stand together: the dot's keeps `last / max` finite on an all-zero series, and this one keeps
  // a series under 1 (the Dashboard's cost spark in dollars per hour) from rescaling to its own peak.
  const max = Math.max(...drawn, 1);
  const path = useMemo(() => {
    if (values.length < 2) return { area: '', line: '' };
    const min = 0;
    const stepX = width / (values.length - 1);
    const runs: (readonly [number, number])[][] = [];
    values.forEach((v, i) => {
      if (v === null) return;
      const point = [i * stepX, height - 2 - ((v - min) / (max - min || 1)) * (height - 4)] as const;
      if (i > 0 && values[i - 1] !== null) runs[runs.length - 1].push(point);
      else runs.push([point]);
    });
    const lineOf = (run: (readonly [number, number])[]) => run.map(([x, y], i) => `${i === 0 ? 'M' : 'L'}${x.toFixed(1)} ${y.toFixed(1)}`).join(' ');
    const line = runs.map(lineOf).join(' ');
    const area = runs.map((run) => `${lineOf(run)} L${run[run.length - 1][0].toFixed(1)} ${height} L${run[0][0].toFixed(1)} ${height} Z`).join(' ');
    return { area, line };
  }, [values, width, height, max]);
  const last = values[values.length - 1] ?? null;
  return (
    <svg className="sparkline" viewBox={`0 0 ${width} ${height}`} width="100%" height={height} preserveAspectRatio="none" aria-hidden>
      {fill && <path d={path.area} fill={color} opacity={0.1} />}
      <path d={path.line} fill="none" stroke={color} strokeWidth={1.75} strokeLinejoin="round" strokeLinecap="round" vectorEffect="non-scaling-stroke" />
      {last !== null && <circle cx={width} cy={height - 2 - (last / max) * (height - 4)} r={3} fill={color} />}
    </svg>
  );
}
