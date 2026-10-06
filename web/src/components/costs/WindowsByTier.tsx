import { useState } from 'react';
import { planLabel } from '@/api/decode';
import type { SubscriptionCosts } from '@/api/types';
import { Card, Segmented } from '@/components/ui/primitives';
import { fullWindowsAWeek, smallestTier, tierCapacity, windowMultiple, type CostWindow } from '@/lib/costs';
import { fmtShare } from '@/lib/format';

const WINDOWS: { factor: 'fiveHour' | 'sevenDay'; key: CostWindow; short: string }[] = [
  { factor: 'fiveHour', key: 'five_hour', short: '5-hour' },
  { factor: 'sevenDay', key: 'seven_day', short: '7-day' },
];

/** Three figures at most, trailing zeros dropped: 12, 17.1, 8.57. */
const figure = (n: number) => String(Number(n.toPrecision(3)));

/** The band control both tabs share: the peak band's state a figure is read in, outside the band until switched. */
export function BandSwitch({ inside, onChange }: { inside: boolean; onChange: (inside: boolean) => void }) {
  return (
    <span className="cost-ref__control">
      <span>Peak band</span>
      <Segmented<'in' | 'out'>
        ariaLabel="Peak band"
        value={inside ? 'in' : 'out'}
        onChange={(v) => onChange(v === 'in')}
        options={[
          { label: 'Outside', value: 'out' },
          { label: 'Inside', value: 'in' },
        ]}
      />
    </span>
  );
}

/** Each sized tier's windows against the smallest tier's, and its week counted in full 5-hour windows. */
export function WindowsByTier({ costs }: { costs: SubscriptionCosts }) {
  const [inside, setInside] = useState(false);
  const tiers = Object.keys(costs.capacity).filter((tier) => WINDOWS.some((w) => tierCapacity(costs, tier, w.key) !== null));
  const largest = (window: CostWindow) => Math.max(0, ...tiers.map((tier) => tierCapacity(costs, tier, window) ?? 0));
  const smallest = smallestTier(costs, 'five_hour');
  const barCell = (tier: string, w: (typeof WINDOWS)[number]) => {
    const label = planLabel(tier);
    const capacity = tierCapacity(costs, tier, w.key);
    const multiple = windowMultiple(costs, tier, w.key);
    const base = smallestTier(costs, w.key);
    const max = largest(w.key);
    if (capacity === null || multiple === null || base === null || max <= 0) {
      return (
        <td key={w.key}>
          <span className="cost-ref__unsized" title={`The cost model sizes no ${label} ${w.short} window, so ${label} traffic prices no share there`}>
            not sized
          </span>
        </td>
      );
    }
    const factor = inside ? costs.peak.capacityFactor[w.factor] : 1;
    const held = capacity * factor;
    const title = `${label} ${w.short} window${inside ? ' inside the peak band' : ''}: ${figure(multiple)}x a ${planLabel(base)} one, ${fmtShare(held / max)} of the largest`;
    return (
      <td key={w.key}>
        <div className="cost-ref__barcell" title={title}>
          <span className="cost-ref__track">
            {factor < 1 && <span className="cost-ref__bar cost-ref__bar--light" style={{ width: `${(capacity / max) * 100}%` }} />}
            <span className="cost-ref__bar" style={{ width: `${(held / max) * 100}%` }} />
          </span>
          <span className="cost-ref__val">{figure(multiple)}x</span>
        </div>
      </td>
    );
  };
  const weekCell = (tier: string) => {
    const full = fullWindowsAWeek(costs, tier, inside);
    if (full === null) {
      return (
        <td>
          <span className="cost-ref__unsized">-</span>
        </td>
      );
    }
    const each = fmtShare(1 / full);
    const segment = `One full 5-hour window${inside ? ' inside the peak band' : ''}, ${each} of the ${planLabel(tier)} week`;
    return (
      <td>
        <div className="cost-ref__barcell">
          <span className="cost-ref__segs">
            {Array.from({ length: Math.round(full) }, (_, i) => (
              <span key={i} title={segment} />
            ))}
          </span>
          <span className="cost-ref__val">
            {figure(full)} <small>({each} each)</small>
          </span>
        </div>
      </td>
    );
  };
  return (
    <Card title="Windows by tier" subtitle={`How much each tier's windows hold, against ${smallest === null ? 'the smallest' : planLabel(smallest)}`} actions={<BandSwitch inside={inside} onChange={setInside} />}>
      <table className="cost-ref__tiers">
        <thead>
          <tr>
            <th>Tier</th>
            <th>5-hour window</th>
            <th>7-day window</th>
            <th>Full 5-hour windows a week</th>
          </tr>
        </thead>
        <tbody>
          {tiers.map((tier, n) => (
            <tr key={tier} className={n % 2 ? 'is-alt' : undefined}>
              <td>
                <span className="cost-ref__tier-name">{planLabel(tier)}</span>
                <span className="cost-ref__tier-key">{tier}</span>
              </td>
              {WINDOWS.map((w) => barCell(tier, w))}
              {weekCell(tier)}
            </tr>
          ))}
        </tbody>
      </table>
    </Card>
  );
}
