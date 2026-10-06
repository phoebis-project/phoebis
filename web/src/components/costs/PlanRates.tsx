import { useState } from 'react';
import { planLabel } from '@/api/decode';
import type { ModelPrice, SubscriptionCosts } from '@/api/types';
import { Segmented } from '@/components/ui/primitives';
import { anchorTier, bandNow, shareOf, tierCapacity, weightOf, type CostWindow, type TokenType, type Usage } from '@/lib/costs';
import { fmtShare, undatedModel, upperFirst } from '@/lib/format';
import { nowLine } from './PeakBand';
import { BandSwitch } from './WindowsByTier';

export const TOKEN_TYPES: { key: TokenType; label: string }[] = [
  { key: 'input', label: 'Input' },
  { key: 'cacheRead', label: 'Cache read' },
  { key: 'cacheWrite5m', label: 'Write 5m' },
  { key: 'cacheWrite1h', label: 'Write 1h' },
  { key: 'output', label: 'Output' },
];
const WINDOWS: { key: CostWindow; label: string }[] = [
  { key: 'five_hour', label: '5-hour window' },
  { key: 'seven_day', label: '7-day window' },
];
/** In millions: one click per decade, where the spinner's arrows step 100,000 tokens. */
const PICKS = [0.1, 1, 10];
/** "1M", "2.5M", "100k". */
const fmtTokens = (millions: number) => (millions >= 1 ? `${Number(millions.toPrecision(4))}M` : `${Number((millions * 1000).toPrecision(3))}k`);
const ZERO: Usage = { cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input: 0, output: 0 };

const dash = <span className="muted">-</span>;

/** The model weeks a model carries: `seven_day_fable` for a Fable model, by the family the row's key names. */
const modelWeeks = (costs: SubscriptionCosts, model: string): string[] =>
  Object.keys(costs.modelWindows).filter((key) => costs.modelWindows[key].of === 'seven_day' && undatedModel(model).startsWith(`claude-${key.slice('seven_day_'.length)}`));
const weekLabel = (key: string) => `${upperFirst(key.slice('seven_day_'.length))} week`;

/** The share of a tier's windows a token count of each type spends, per model, outside or inside the band. */
export function PlanRates({ costs, now, prices }: { costs: SubscriptionCosts; now: number; prices: ModelPrice[] }) {
  const tiers = Object.keys(costs.capacity).filter((tier) => WINDOWS.some((w) => tierCapacity(costs, tier, w.key) !== null));
  const [tier, setTier] = useState(() => anchorTier(costs));
  const [inside, setInside] = useState(false);
  const [tokens, setTokens] = useState(1);
  const [typed, setTyped] = useState('1');
  const pick = (millions: number) => {
    setTokens(millions);
    setTyped(String(millions));
  };
  // The band's state now, or the instant it next flips, prices every cell as a request completing then.
  const next = bandNow(costs, now);
  const at = next.inside === inside ? now : (next.at ?? now);

  const weighted = Object.keys(costs.modelWeight).filter((model) => costs.modelWeight[model] !== null);
  const unweighted = [
    ...prices.map((p) => p.model).filter((model) => weightOf(costs, model, 'five_hour') === null),
    ...Object.keys(costs.modelWeight).filter((model) => costs.modelWeight[model] === null && !prices.some((p) => undatedModel(p.model) === model)),
  ];
  const fiveHourWeight = (model: string) => weightOf(costs, model, 'five_hour') ?? 0;
  const lightest = weighted.reduce<string | null>((low, model) => (low === null || fiveHourWeight(model) < fiveHourWeight(low) ? model : low), null);
  const heaviest = Math.max(0, ...weighted.map(fiveHourWeight));
  const tierName = planLabel(tier);
  const cols = 2 + WINDOWS.length * TOKEN_TYPES.length;

  /** `what` names the window in the cell's title: "5-hour window", "Fable week". */
  const cell = (model: string, window: string, what: string, type: (typeof TOKEN_TYPES)[number], i: number) => {
    const share = shareOf(costs, window, tier, model, { ...ZERO, [type.key]: tokens * 1_000_000 }, at);
    const sep = i === 0 ? 'cost-ref__sep' : '';
    if (share === null) {
      return (
        <td key={`${window}-${type.key}`} className={`right num ${sep}`}>
          {dash}
        </td>
      );
    }
    const full = share >= 1;
    return (
      <td
        key={`${window}-${type.key}`}
        className={`right num ${sep}${full ? ' is-full' : ''}`}
        title={`${model}, ${type.label.toLowerCase()}: ${fmtTokens(tokens)} tokens spend ${fmtShare(share)} of a ${tierName} ${what}${full ? ', more than a full one' : ''}`}
      >
        {fmtShare(share)}
      </td>
    );
  };
  const weightCell = (model: string) => {
    const weight = weightOf(costs, model, 'five_hour');
    const base = lightest === null ? null : weightOf(costs, lightest, 'five_hour');
    if (weight === null || base === null || base <= 0 || heaviest <= 0) {
      return (
        <td>
          <span className="cost-ref__unsized" title="No weight in the subscription cost model, so its traffic prices no window share">
            not weighed
          </span>
        </td>
      );
    }
    const multiple = `${Number((weight / base).toPrecision(3))}x`;
    return (
      <td>
        <div className="cost-ref__weight" title={`${model}: ${multiple} the share of ${lightest} per token of the same type on the 5-hour window`}>
          <span className="cost-ref__track">
            <span className="cost-ref__bar" style={{ width: `${(weight / heaviest) * 100}%` }} />
          </span>
          <span className="cost-ref__val">{multiple}</span>
        </div>
      </td>
    );
  };
  // A model and its sub-rows share one stripe, and the stripes restart under the divider.
  const rows = (models: string[]) =>
    models.flatMap((model, n) => {
      const alt = n % 2 ? 'is-alt' : '';
      const weeks = weightOf(costs, model, 'seven_day') === null ? [] : modelWeeks(costs, model);
      return [
        <tr key={model} className={`${alt}${weeks.length ? ' has-sub' : ''}`}>
          <td className="mono">{model}</td>
          {weightCell(model)}
          {WINDOWS.map((w) => TOKEN_TYPES.map((t, i) => cell(model, w.key, w.label, t, i)))}
        </tr>,
        ...weeks.map((week) => (
          <tr key={`${model} ${week}`} className={`cost-ref__sub ${alt}`}>
            <td title="A window only these models carry, sized against the 7-day window">{weekLabel(week)}</td>
            <td />
            {WINDOWS.map((w) =>
              TOKEN_TYPES.map((t, i) =>
                w.key === 'seven_day' ? cell(model, week, weekLabel(week), t, i) : <td key={`${w.key}-${t.key}`} className={i === 0 ? 'cost-ref__sep' : undefined} />,
              ),
            )}
          </tr>
        )),
      ];
    });

  return (
    <>
      <div className="cost-ref__controls">
        <span className="cost-ref__control">
          <span>Tier</span>
          <Segmented ariaLabel="Tier" value={tier} onChange={setTier} options={tiers.map((t) => ({ label: planLabel(t), value: t }))} />
        </span>
        <BandSwitch inside={inside} onChange={setInside} />
        <span className="cost-ref__tokens">
          <label>
            Tokens{' '}
            <input
              type="number"
              className="input"
              min="0.1"
              step="0.1"
              value={typed}
              aria-label="Tokens, in millions"
              onChange={(e) => {
                setTyped(e.target.value);
                const v = e.target.valueAsNumber;
                if (Number.isFinite(v) && v > 0) setTokens(v);
              }}
            />{' '}
            million
          </label>
          <span className="segmented" role="group" aria-label="Token count">
            {PICKS.map((v) => (
              <button key={v} type="button" className={tokens === v ? 'is-active' : ''} aria-pressed={tokens === v} onClick={() => pick(v)}>
                {fmtTokens(v)}
              </button>
            ))}
          </span>
        </span>
        <span className="cost-ref__now">{nowLine(costs, now)}</span>
      </div>
      <div className="cost-ref__caption">
        <b>
          Share of a {tierName} window per {fmtTokens(tokens)} tokens{inside ? ', a request completing inside the peak band' : ''}
        </b>
        <span className="muted">A shaded cell is more than one full window.</span>
      </div>
      <div className="table-wrap cost-ref__wrap">
        <table className="table cost-ref__table">
          <thead>
            <tr>
              <th rowSpan={2}>Model</th>
              <th rowSpan={2}>Weight, 5-hour window</th>
              {WINDOWS.map((w) => (
                <th key={w.key} colSpan={TOKEN_TYPES.length} className="cost-ref__grp cost-ref__sep">
                  {w.label}
                </th>
              ))}
            </tr>
            <tr>
              {WINDOWS.map((w) =>
                TOKEN_TYPES.map((t, i) => (
                  <th key={`${w.key}-${t.key}`} className={`right${i === 0 ? ' cost-ref__sep' : ''}`}>
                    {t.label}
                  </th>
                )),
              )}
            </tr>
          </thead>
          <tbody>
            {rows(weighted)}
            {unweighted.length > 0 && (
              <tr className="is-divider">
                <td colSpan={cols}>Not weighed by the subscription cost model: a share on any window reads a dash, and the analytics list the traffic under model unlisted</td>
              </tr>
            )}
            {rows(unweighted)}
          </tbody>
        </table>
      </div>
    </>
  );
}
