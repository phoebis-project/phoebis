import type { KeepaliveOverview } from '@/api/types';
import { PROBE_SERIES } from '@/components/charts/theme';
import { probeParts } from '@/lib/analytics';

/** The day's probes as one bar, each outcome's segment as wide as its share, its counts on the bar's label. */
export function ProbeBar({ probes }: { probes: KeepaliveOverview['probes24h'] }) {
  const parts = probeParts(probes);
  return (
    <div className="probe-bar" role="img" aria-label={`Probes in the last 24h: ${PROBE_SERIES.map((s) => `${parts[s.key]} ${s.key}`).join(', ')}`}>
      {PROBE_SERIES.map((s) => (
        <span key={s.key} className={`probe-bar__seg probe-bar__seg--${s.key}`} style={{ flex: parts[s.key] }} title={s.hint} />
      ))}
    </div>
  );
}
