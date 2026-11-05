import { type ReactNode, useState } from 'react';
import type { ModelPrice, SubscriptionCosts } from '@/api/types';
import { Input, Segmented } from '@/components/ui/primitives';
import { anchorTier, bandNow, COST_WINDOWS, MODEL_WEEK_TIERS, modelWeeks, regimeNames, regimeTitle, regimeWord, shareIn, newestAndOlder, sizedTiers, tierGroups, unreachableBand, weightedWindows, weightOf, withRegime, type TokenType, type Usage } from '@/lib/costs';
import { lookup } from '@/lib/counts';
import { figure, fmtShare, plural, undatedModel, upperFirst } from '@/lib/format';
import { planLabel } from '@/lib/quota';
import { nowLine } from './PeakBand';
import { BandSwitch } from './WindowsByTier';

export const TOKEN_TYPES: { key: TokenType; label: string }[] = [
  { key: 'input', label: 'Input' },
  { key: 'cacheRead', label: 'Cache read' },
  { key: 'cacheWrite5m', label: 'Write 5m' },
  { key: 'cacheWrite1h', label: 'Write 1h' },
  { key: 'output', label: 'Output' },
];
const WINDOWS = COST_WINDOWS.map((w) => ({ key: w.key, label: `${w.label} window` }));
/** In millions: one click per decade, where the spinner's arrows step 100,000 tokens. */
const PICKS = [0.1, 1, 10];
/** "1M", "2.5M", "100k". */
const fmtTokens = (millions: number) => (millions >= 1 ? `${Number(millions.toPrecision(4))}M` : `${Number((millions * 1000).toPrecision(3))}k`);
const ZERO: Usage = { cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input: 0, output: 0 };

const dash = <span className="muted">-</span>;

const weekLabel = (key: string) => `7-day ${upperFirst(key.slice('seven_day_'.length))}`;
const windowLabel = (window: string) => WINDOWS.find((w) => w.key === window)?.label ?? `${weekLabel(window)} window`;

/** The row at a model table's foot that folds its older models: their count, and the disclosure that shows them. */
export function FoldRow({ cols, count, onToggle, open }: { cols: number; count: number; onToggle: () => void; open: boolean }) {
  return (
    <tr className="cost-ref__fold">
      <td colSpan={cols}>
        <button type="button" className="link-btn" aria-expanded={open} onClick={onToggle}>
          {plural(count, 'older model')}
        </button>
      </td>
    </tr>
  );
}

/**
 * A model table's body: the newest models, the fold row and, while it is open, the older models. A model and its
 * sub-rows share one stripe, and the count runs on through the fold, so the stripes alternate whether the older models
 * show or not.
 */
export function FoldedRows<T>({ cols, older, onToggle, open, row, shown }: { cols: number; older: T[]; onToggle: () => void; open: boolean; row: (item: T, alt: string) => ReactNode[]; shown: T[] }) {
  const rows = (items: T[], from: number) => items.flatMap((item, i) => row(item, (from + i) % 2 ? 'is-alt' : ''));
  return (
    <>
      {rows(shown, 0)}
      {older.length > 0 && <FoldRow cols={cols} count={older.length} onToggle={onToggle} open={open} />}
      {open && rows(older, shown.length)}
    </>
  );
}

/**
 * The share of a tier's windows a token count of each type spends, per model, outside or inside the band: one row per
 * model tier with its newest model, the older models folded under a row of their own.
 */
export function PlanRates({ costs, now, olderOpen, onToggleOlder, prices }: { costs: SubscriptionCosts; now: number; olderOpen: boolean; onToggleOlder: () => void; prices: ModelPrice[] }) {
  // The anchor's tier first, since every figure the console prints is a share of its week, then the file's order.
  const sized = sizedTiers(costs);
  const tiers = [...sized.filter((tier) => tier === anchorTier(costs)), ...sized.filter((tier) => tier !== anchorTier(costs))];
  const [tier, setTier] = useState(() => anchorTier(costs));
  const regimes = regimeNames(costs);
  // The interactive regime first where the model names it, since the interactive client is the traffic an operator
  // runs. '' is the base, drawn as Headless.
  const [regime, setRegime] = useState(() => (regimes.includes('interactive') ? 'interactive' : ''));
  // A revision without the chosen regime reads the base, so the control and the caption follow it.
  const shownRegime = regimes.includes(regime) ? regime : '';
  const priced = withRegime(costs, shownRegime || null);
  const next = bandNow(priced, now);
  const unresolved = 'unresolved' in next ? next.unresolved : null;
  // With no state to read the viewer picks one, Inside first, so a share never reads below what the band may charge.
  const [inside, setInside] = useState(() => unresolved !== null);
  const [tokens, setTokens] = useState(1);
  const [typed, setTyped] = useState('1');
  const pick = (millions: number) => {
    setTokens(millions);
    setTyped(String(millions));
  };
  const unreachable = unreachableBand(next);
  // A band that never flips offers one state alone, so the choice follows it.
  const chosen = unreachable === null ? inside : !unreachable.inside;

  // The cost model's models and the price file's, a dated price row folded into the weight row of its undated id.
  const seen = new Set<string>();
  const models = [...Object.keys(costs.modelWeight), ...prices.map((p) => p.model)].filter((model) => {
    const undated = undatedModel(model);
    if (seen.has(undated)) return false;
    seen.add(undated);
    return true;
  });
  const { newest: shown, older } = newestAndOlder(tierGroups(models, costs));
  // The lightest is a model weighed on the 5-hour window itself, since a model weighed on the week alone has no 5-hour
  // weight there. The heaviest reads every model's own 5-hour weight, an unweighed model counting nothing, so no bar
  // runs past its track.
  const fiveHourWeighted = models.filter((model) => weightOf(costs, model, 'five_hour') !== null);
  const fiveHourWeight = (model: string) => weightOf(costs, model, 'five_hour') ?? 0;
  const lightest = fiveHourWeighted.reduce<string | null>((low, model) => (low === null || fiveHourWeight(model) < fiveHourWeight(low) ? model : low), null);
  const heaviest = Math.max(0, ...models.map(fiveHourWeight));
  const tierName = planLabel(tier);
  const cols = 2 + WINDOWS.length * TOKEN_TYPES.length;

  /** `what` names the window in the cell's title: "5-hour window", "7-day Fable". */
  const cell = (model: string, window: string, what: string, type: (typeof TOKEN_TYPES)[number], i: number) => {
    const share = shareIn(priced, window, tier, model, { ...ZERO, [type.key]: tokens * 1_000_000 }, chosen);
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
    const windows = weightedWindows(costs, model);
    // A null shared weight under window weights has no one figure to scale, so the cell names each window's own.
    if (lookup(costs.modelWeight, model) === null && windows.length > 0) {
      const named = windows.map((w) => `${weightOf(costs, model, w)} on the ${windowLabel(w)}`).join(', ');
      return (
        <td>
          <span className="cost-ref__unsized" title={`${model} carries no shared weight, only a weight per window: ${named}`}>
            by window
          </span>
        </td>
      );
    }
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
    const multiple = `${figure(weight / base)}x`;
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
  const row = (model: string, alt: string) => {
    // A model week draws only under a tier whose plan carries it, so no row names a Fable week against Pro.
    const weeks = modelWeeks(costs, model).filter((week) => weightOf(costs, model, week) !== null && (MODEL_WEEK_TIERS[week]?.includes(tier) ?? false));
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
  };

  return (
    <>
      <div className="cost-ref__controls">
        <span className="cost-ref__control">
          <span>Tier</span>
          <Segmented ariaLabel="Tier" value={tier} onChange={setTier} options={tiers.map((t) => ({ label: planLabel(t), value: t }))} />
        </span>
        {regimes.length > 0 && (
          <span className="cost-ref__control">
            <span>Regime</span>
            <Segmented ariaLabel="Regime" value={shownRegime} onChange={setRegime} options={[...regimes.map((r) => ({ label: regimeWord(r), value: r })), { label: regimeWord(null), value: '' }]} />
          </span>
        )}
        <BandSwitch inside={chosen} onChange={setInside} unreachable={unreachable} />
        <span className="cost-ref__tokens">
          <label>
            Tokens{' '}
            <Input
              type="number"
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
        <span className="cost-ref__now">{unresolved === null ? nowLine(priced, now) : `This browser cannot read the zone ${unresolved}, so pick the band state yourself.`}</span>
      </div>
      <div className="cost-ref__caption">
        <b>
          Share of a {tierName} window per {fmtTokens(tokens)} tokens under {regimeTitle(costs, shownRegime || null)}
          {chosen ? ', a request completing inside the peak band' : ''}
        </b>
        <span className="muted">A shaded cell is more than one full window.</span>
      </div>
      <div className="table-wrap cost-ref__wrap">
        <table className="table cost-ref__table table--plain">
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
            <FoldedRows cols={cols} older={older} onToggle={onToggleOlder} open={olderOpen} row={row} shown={shown} />
          </tbody>
        </table>
      </div>
    </>
  );
}
