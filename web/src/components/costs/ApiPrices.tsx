import type { ModelPrice, ModelTiers, PriceCells } from '@/api/types';
import { tierGroups } from '@/lib/costs';
import { fmtCompact, fmtInt } from '@/lib/format';
import { FoldRow, TOKEN_TYPES } from './PlanRates';

const CELL: Record<(typeof TOKEN_TYPES)[number]['key'], keyof PriceCells> = {
  cacheRead: 'cacheReadPerM',
  cacheWrite1h: 'cacheWrite1hPerM',
  cacheWrite5m: 'cacheWrite5mPerM',
  input: 'inputPerM',
  output: 'outputPerM',
};

/** Cents at least, and a third or fourth decimal where the price has one: $10.00, $0.25, $0.025. */
const fmtPrice = (usd: number) => `$${usd.toFixed(4).replace(/(\.\d\d[1-9]?)0+$/, '$1')}`;

const FAST_MODE = 'What an answer served in fast mode costs. A subscription account pays it from usage credits, not from its window.';
const LONG_CONTEXT = (over: number) =>
  `What an attempt whose prompt is longer than ${fmtInt(over)} tokens costs. The prompt is its uncached input, its cache reads and its cache writes together.`;

/**
 * The dollar list prices per million tokens, one row per model tier with its newest model and the older models folded
 * under a row of their own, a fast-mode row under each model with one and a row under each model priced by prompt
 * length at its prices over the threshold.
 */
export function ApiPrices({ costs, olderOpen, onToggleOlder, prices }: { costs: ModelTiers; olderOpen: boolean; onToggleOlder: () => void; prices: ModelPrice[] }) {
  const groups = tierGroups(prices.map((p) => p.model), costs);
  const rowsOf = (models: string[]) => models.flatMap((model) => prices.filter((p) => p.model === model));
  const shown = rowsOf(groups.map((g) => g.models[0]));
  const older = rowsOf(groups.flatMap((g) => g.models.slice(1)));
  const cells = (row: PriceCells) =>
    TOKEN_TYPES.map((t, i) => {
      const usd = row[CELL[t.key]];
      // A model the file prices no cache write for reads a dash there rather than a free write.
      const none = (t.key === 'cacheWrite5m' || t.key === 'cacheWrite1h') && !usd;
      return (
        <td key={t.key} className={`right num${i === 0 ? ' cost-ref__sep' : ''}`}>
          {none ? <span className="muted">-</span> : fmtPrice(usd)}
        </td>
      );
    });
  // A model and its sub-rows share one stripe, and the count runs on through the fold, so the stripes alternate
  // whether the older models show or not.
  const body = (rows: ModelPrice[], from: number) =>
    rows.flatMap((p, i) => {
      const alt = (from + i) % 2 ? 'is-alt' : '';
      return [
        <tr key={p.model} className={`${alt}${p.fast || p.longContext ? ' has-sub' : ''}`}>
          <td className="mono">{p.model}</td>
          {cells(p)}
        </tr>,
        ...(p.fast
          ? [
              <tr key={`${p.model} fast`} className={`cost-ref__sub ${alt}`}>
                <td title={FAST_MODE}>fast mode</td>
                {cells(p.fast)}
              </tr>,
            ]
          : []),
        ...(p.longContext
          ? [
              <tr key={`${p.model} long-context`} className={`cost-ref__sub ${alt}`}>
                <td title={LONG_CONTEXT(p.longContext.over)}>prompt over {fmtCompact(p.longContext.over)} tokens</td>
                {cells(p.longContext)}
              </tr>,
            ]
          : []),
      ];
    });
  return (
    <>
      <div className="cost-ref__caption">
        <b>USD per million tokens at list price</b>
        <span className="muted">An API-key account pays these. A subscription account pays them in overage and for fast-mode answers.</span>
      </div>
      <div className="table-wrap cost-ref__wrap">
        <table className="table cost-ref__table table--plain">
          <thead>
            <tr>
              <th>Model</th>
              {TOKEN_TYPES.map((t, i) => (
                <th key={t.key} className={`right${i === 0 ? ' cost-ref__sep' : ''}`}>
                  {t.label}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {body(shown, 0)}
            {older.length > 0 && <FoldRow cols={1 + TOKEN_TYPES.length} count={older.length} onToggle={onToggleOlder} open={olderOpen} />}
            {olderOpen && body(older, shown.length)}
          </tbody>
        </table>
      </div>
    </>
  );
}
