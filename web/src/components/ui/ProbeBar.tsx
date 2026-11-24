import type { KeepaliveOverview } from '@/api/types';
import { PROBE_SERIES } from '@/components/charts/theme';

/** The day's probes as one bar, each outcome's segment as wide as its share, its counts on the bar's label. */
export function ProbeBar({ probes }: { probes: KeepaliveOverview['probes24h'] }) {
  return (
    <div className="probe-bar" role="img" aria-label={`Probes in the last 24h: ${PROBE_SERIES.map((s) => `${probes[s.key]} ${s.key}`).join(', ')}`}>
      {PROBE_SERIES.map((s) => (
        <span key={s.key} className={`probe-bar__seg probe-bar__seg--${s.key}`} style={{ flex: probes[s.key] }} title={s.hint} />
      ))}
    </div>
  );
}
