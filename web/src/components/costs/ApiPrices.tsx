import type { FastPrice, ModelPrice } from '@/api/types';
import { familyOrder } from '@/lib/costs';
import { TOKEN_TYPES } from './PlanRates';

const CELL: Record<(typeof TOKEN_TYPES)[number]['key'], keyof FastPrice> = {
  cacheRead: 'cacheReadPerM',
  cacheWrite1h: 'cacheWrite1hPerM',
  cacheWrite5m: 'cacheWrite5mPerM',
  input: 'inputPerM',
  output: 'outputPerM',
};

/** Cents at least, and a third or fourth decimal where the price has one: $10.00, $0.25, $0.025. */
const fmtPrice = (usd: number) => `$${usd.toFixed(4).replace(/(\.\d\d[1-9]?)0+$/, '$1')}`;

const FAST_MODE = 'What an answer served in fast mode costs. A subscription account pays it from usage credits, not from its window.';

/** The dollar list prices per million tokens family by family, a fast-mode row under each model with one. */
export function ApiPrices({ prices }: { prices: ModelPrice[] }) {
  const rows = familyOrder(prices.map((p) => p.model)).flatMap((model) => prices.filter((p) => p.model === model));
  const cells = (row: FastPrice) =>
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
  return (
    <>
      <div className="cost-ref__caption">
        <b>USD per million tokens at list price</b>
        <span className="muted">An Anthropic-key account pays these. A subscription account pays them in overage and for fast-mode answers.</span>
      </div>
      <div className="table-wrap cost-ref__wrap">
        <table className="table cost-ref__table">
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
            {rows.flatMap((p, n) => {
              // A model and its fast-mode row share one stripe.
              const alt = n % 2 ? 'is-alt' : '';
              return [
                <tr key={p.model} className={`${alt}${p.fast ? ' has-sub' : ''}`}>
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
              ];
            })}
          </tbody>
        </table>
      </div>
    </>
  );
}
