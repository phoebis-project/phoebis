import type { KeepaliveOverview } from '@/api/types';
import { HELD_PROBE_HINT, MIGRATED_PROBE_HINT } from '@/components/charts/theme';

/** The day's probes as one bar, each outcome's segment as wide as its share, its counts on the bar's label. */
export function ProbeBar({ probes }: { probes: KeepaliveOverview['probes24h'] }) {
  return (
    <div className="probe-bar" role="img" aria-label={`Probes in the last 24h: ${probes.ok} ok, ${probes.migrated} migrated, ${probes.held} held, ${probes.skipped} skipped, ${probes.failed} failed`}>
      <span className="probe-bar__seg probe-bar__seg--ok" style={{ flex: probes.ok }} />
      <span className="probe-bar__seg probe-bar__seg--migrated" style={{ flex: probes.migrated }} title={MIGRATED_PROBE_HINT} />
      <span className="probe-bar__seg probe-bar__seg--held" style={{ flex: probes.held }} title={HELD_PROBE_HINT} />
      <span className="probe-bar__seg probe-bar__seg--skipped" style={{ flex: probes.skipped }} />
      <span className="probe-bar__seg probe-bar__seg--failed" style={{ flex: probes.failed }} />
    </div>
  );
}
