import { useState } from 'react';
import type { SubscriptionCosts } from '@/api/types';
import { Card, Segmented } from '@/components/ui/primitives';
import { anchorTier, fullWindowsAWeek, regimeNames, regimeTitle, regimeWord, smallestTier, tierCapacity, withRegime, type CostWindow } from '@/lib/costs';
import { fmtInt, fmtShare } from '@/lib/format';
import { planLabel } from '@/lib/quota';

const WINDOWS: { factor: 'fiveHour' | 'sevenDay'; key: CostWindow; short: string }[] = [
  { factor: 'fiveHour', key: 'five_hour', short: '5-hour' },
  { factor: 'sevenDay', key: 'seven_day', short: '7-day' },
];

/** Three figures at most, trailing zeros dropped: 12, 17.1, 8.57. */
const figure = (n: number) => String(Number(n.toPrecision(3)));

/** The band control both tabs share: the peak band's state a figure is read in, outside the band until switched.
 *  `unreachable` disables the state no instant reaches, under its title. */
export function BandSwitch({ inside, onChange, unreachable = null }: { inside: boolean; onChange: (inside: boolean) => void; unreachable?: { inside: boolean; title: string } | null }) {
  const reach = (state: boolean) => (unreachable !== null && unreachable.inside === state ? { disabled: true, title: unreachable.title } : {});
  return (
    <span className="cost-ref__control">
      <span>Peak band</span>
      <Segmented<'in' | 'out'>
        ariaLabel="Peak band"
        value={inside ? 'in' : 'out'}
        onChange={(v) => onChange(v === 'in')}
        options={[
          { label: 'Outside', value: 'out', ...reach(false) },
          { label: 'Inside', value: 'in', ...reach(true) },
        ]}
      />
    </span>
  );
}

/** Each regime's tiers first under a row naming it and the clients it prices, then every sized tier's windows against
 *  the smallest tier's under the Headless row, each tier's week counted in full 5-hour windows. */
export function WindowsByTier({ costs }: { costs: SubscriptionCosts }) {
  const [inside, setInside] = useState(false);
  const tiers = Object.keys(costs.capacity).filter((tier) => WINDOWS.some((w) => tierCapacity(costs, tier, w.key) !== null));
  // A tier the regime names with neither window sized still gets its row, since the regime prices it unsized.
  const regimes = regimeNames(costs).map((name) => ({ model: withRegime(costs, name), name, tiers: Object.keys(costs.regimes[name].capacity) }));
  // One scale for every row the card draws, so a regime's window reads against the base's in the bar's length.
  const largest = (window: CostWindow) => Math.max(0, ...tiers.map((tier) => tierCapacity(costs, tier, window) ?? 0), ...regimes.flatMap((r) => r.tiers.map((tier) => tierCapacity(r.model, tier, window) ?? 0)));
  const smallest = smallestTier(costs, 'five_hour');
  const anchor = anchorTier(costs);
  /** `regime` is null on the base's rows. A regime's multiples read against the base's smallest tier, never the
   *  regime's own, so its rows compare with the base's. */
  const barCell = (model: SubscriptionCosts, regime: string | null, tier: string, w: (typeof WINDOWS)[number]) => {
    const label = planLabel(tier);
    const capacity = tierCapacity(model, tier, w.key);
    const base = smallestTier(costs, w.key);
    const baseCapacity = base === null ? null : tierCapacity(costs, base, w.key);
    const multiple = capacity === null || baseCapacity === null ? null : capacity / baseCapacity;
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
    const factor = inside ? model.peak.capacityFactor[w.factor] : 1;
    const held = capacity * factor;
    const read = `${label} ${w.short} window${regime === null ? '' : ` under ${regimeWord(regime)}`}${inside ? ' inside the peak band' : ''}: ${figure(multiple)}x a ${planLabel(base)} one, ${fmtShare(held / max)} of the largest`;
    const regimeAnchor = tierCapacity(model, anchor, w.key);
    const baseAnchor = tierCapacity(costs, anchor, w.key);
    const title =
      regime === null || regimeAnchor === null || baseAnchor === null
        ? read
        : `${read}. One percent of the ${planLabel(anchor)} ${w.short} window is ${fmtInt(regimeAnchor / 100)} units for a request under ${regimeWord(regime)} and ${fmtInt(baseAnchor / 100)} under ${regimeWord(null)}.`;
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
  const weekCell = (model: SubscriptionCosts, tier: string) => {
    const full = fullWindowsAWeek(model, tier, inside);
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
  const tierRow = (model: SubscriptionCosts, regime: string | null, tier: string) => (alt: string | undefined) => (
    <tr key={regime === null ? tier : `${regime} ${tier}`} className={alt}>
      <td>
        <span className="cost-ref__tier-name">{planLabel(tier)}</span>
        <span className="cost-ref__tier-key">{tier}</span>
      </td>
      {WINDOWS.map((w) => barCell(model, regime, tier, w))}
      {weekCell(model, tier)}
    </tr>
  );
  const groupRow = (name: string | null) => (alt: string | undefined) => (
    <tr key={`group ${name ?? 'base'}`} className={['cost-ref__group', alt].filter(Boolean).join(' ')}>
      <td colSpan={WINDOWS.length + 2}>{regimeTitle(costs, name)}</td>
    </tr>
  );
  // The stripes run on through the group rows, which count as rows of their own.
  const rows = [
    ...regimes.flatMap(({ model, name, tiers: named }) => [groupRow(name), ...named.map((tier) => tierRow(model, name, tier))]),
    groupRow(null),
    ...tiers.map((tier) => tierRow(costs, null, tier)),
  ];
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
        <tbody>{rows.map((row, n) => row(n % 2 ? 'is-alt' : undefined))}</tbody>
      </table>
    </Card>
  );
}
