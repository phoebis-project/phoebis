import { useMemo } from 'react';

/** A null value is an hour with no figure to draw, which the line leaves as a gap rather than reading it as zero. */
export function Sparkline({ values, width = 160, height = 36, color, fill = true }: { color: string; fill?: boolean; height?: number; values: (number | null)[]; width?: number }) {
  const drawn = values.filter((v): v is number => v !== null);
  // The floor at 1 keeps `v / max` finite on an all-zero series, and keeps a series under 1 (the Dashboard's cost spark
  // in dollars per hour) from rescaling to its own peak.
  const max = Math.max(...drawn, 1);
  const path = useMemo(() => {
    // One y for the line and the dot, so the dot sits on the line, read ahead of the early return since a single value
    // still draws its dot.
    const yOf = (v: number) => height - 2 - (v / max) * (height - 4);
    const last = values[values.length - 1] ?? null;
    const dot = last === null ? null : yOf(last);
    if (values.length < 2) return { area: '', dot, line: '' };
    const stepX = width / (values.length - 1);
    const runs: (readonly [number, number])[][] = [];
    values.forEach((v, i) => {
      if (v === null) return;
      const point = [i * stepX, yOf(v)] as const;
      if (i > 0 && values[i - 1] !== null) runs[runs.length - 1].push(point);
      else runs.push([point]);
    });
    const lineOf = (run: (readonly [number, number])[]) => run.map(([x, y], i) => `${i === 0 ? 'M' : 'L'}${x.toFixed(1)} ${y.toFixed(1)}`).join(' ');
    const line = runs.map(lineOf).join(' ');
    const area = runs.map((run) => `${lineOf(run)} L${run[run.length - 1][0].toFixed(1)} ${height} L${run[0][0].toFixed(1)} ${height} Z`).join(' ');
    return { area, dot, line };
  }, [values, width, height, max]);
  return (
    <svg className="sparkline" viewBox={`0 0 ${width} ${height}`} width="100%" height={height} preserveAspectRatio="none" aria-hidden>
      {fill && <path d={path.area} fill={color} opacity={0.1} />}
      <path d={path.line} fill="none" stroke={color} strokeWidth={1.75} strokeLinejoin="round" strokeLinecap="round" vectorEffect="non-scaling-stroke" />
      {path.dot !== null && <circle cx={width} cy={path.dot} r={3} fill={color} />}
    </svg>
  );
}
