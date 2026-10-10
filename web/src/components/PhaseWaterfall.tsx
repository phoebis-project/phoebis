import type { RequestPhases } from '@/api/types';
import { useChartInk } from '@/components/charts/useChartInk';
import { fmtMs } from '@/lib/format';

/** The seven phases in the order a request passes them, keyed by the summary's members. */
export const PHASES: { key: keyof RequestPhases; label: string }[] = [
  { key: 'receivedAt', label: 'Received' },
  { key: 'selectedAt', label: 'Selected' },
  { key: 'writtenAt', label: 'Written' },
  { key: 'headAt', label: 'Head' },
  { key: 'firstContentAt', label: 'First content' },
  { key: 'streamEndAt', label: 'Stream end' },
  { key: 'finishedAt', label: 'Finished' },
];

/**
 * The segmented bar from Received to the last measured phase, colored from the chart ramp, and the legend of offsets;
 * a phase with no instant reads "not measured" and draws no segment.
 */
export function PhaseWaterfall({ phases }: { phases: RequestPhases }) {
  const ink = useChartInk();
  const startMs = Date.parse(phases.receivedAt);
  const points = PHASES.map((p) => {
    const at = phases[p.key];
    return { ...p, at: at === null ? null : Date.parse(at) };
  });
  const last = [...points].reverse().find((p) => p.at !== null)?.at ?? startMs;
  const total = Math.max(1, last - startMs);
  // A segment spans from the previous measured phase to this one, and a phase with no timestamp draws nothing
  // rather than a zero-width sliver.
  const segments: { color: string; label: string; width: number }[] = [];
  let prev = startMs;
  points.slice(1).forEach((p, i) => {
    if (p.at === null) return;
    const width = Math.max(0.5, ((p.at - prev) / total) * 100);
    segments.push({ color: i < ink.ramp.length ? ink.ramp[i] : ink.muted, label: p.label, width });
    prev = p.at;
  });
  return (
    <div className="waterfall">
      <div className="waterfall__bar" role="img" aria-label={`Phase waterfall over ${fmtMs(total)}`}>
        {segments.map((s) => (
          <span key={s.label} className="waterfall__seg" style={{ width: `${s.width}%`, background: s.color }} title={s.label} />
        ))}
      </div>
      <ul className="waterfall__legend">
        {points.map((p, i) => (
          <li key={p.key}>
            <span className="waterfall__key" style={{ background: i === 0 ? ink.ink3 : p.at === null ? 'transparent' : i - 1 < ink.ramp.length ? ink.ramp[i - 1] : ink.muted, border: p.at === null ? `1px dashed ${ink.axis}` : undefined }} />
            <span>{p.label}</span>
            {p.at === null ? <span className="muted">not measured</span> : <span className="num">+{fmtMs(p.at - startMs)}</span>}
          </li>
        ))}
      </ul>
    </div>
  );
}
