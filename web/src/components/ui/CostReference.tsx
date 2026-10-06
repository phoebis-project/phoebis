import { useState, type ReactNode } from 'react';
import { api } from '@/api';
import { planLabel } from '@/api/decode';
import type { FastPrice, ModelPrice, SubscriptionCosts } from '@/api/types';
import { anchorTier, ratesOf, type CostWindow, type TokenType } from '@/lib/costs';
import { fmtShare, undatedModel } from '@/lib/format';
import { useAsync } from '@/lib/hooks';
import { Button, Dialog, Skeleton } from './primitives';

const CAVEAT = 'API list prices. A subscription account on plan quota spends the window share columns instead of money.';
/** One term per column group, in table order, so a reader can find the one column they are looking at. */
const NOTES: { body: ReactNode; term: string }[] = [
  { term: 'Per prefix token', body: 'Every column is per prefix token, so the comparison leaves out the new tokens each turn writes.' },
  {
    term: 'Turns to pay off',
    body: 'The number of later turns that must read the prefix before writing it into the cache was cheaper than sending it uncached each turn.',
  },
  {
    term: 'Hold warm',
    body: 'The cost of keeping one prefix token warm across one idle hour (one write plus the probe reads), as a multiple of sending that token once as uncached input; 2.75x means holding it warm costs 2.75 times sending it cold once. A 5-minute entry needs a probe about every 4 minutes to survive an idle hour, 15 in all; a 1-hour entry needs one.',
  },
  {
    term: 'Payoff',
    body: 'The idle span after which the larger 1-hour write has been repaid by the probes a 5-minute entry would have spent by then.',
  },
  {
    term: 'Fable 5.1',
    body: 'On Fable 5.1, where a cache read costs 0.025x, a 5-minute entry kept warm by probes stays cheaper than a 1-hour entry for any idle span; a session with cache warming on may set CLAUDE_CODE_PROMPT_CACHE_TTL=5m on the client. Without warming, the 1-hour entry wins after a 5-minute pause.',
  },
  {
    term: 'Window share',
    body: (
      <>
        Each plan column is the share of a Max 20x subscription's 5-hour or 7-day window that one million tokens of the type consume, derived from the subscription cost model the
        ledger ships: the model's weight times the type's multiplier over the window's capacity. An account on another tier spends that share over the ratio of its tier's capacity to
        Max 20x's, so a Max 5x account spends four times as much of its own 5-hour window and a Pro account twenty times, by the multiples a group header's tooltip gives for each window.
        A dash is a model the plan meter has not weighed, or a tier it has not sized on the window, through which no share is priced. A 1-hour write spends the same share as a 5-minute
        one, so on plan-funded traffic the long TTL is free and the pool's plan rule set keeps <code>1h</code> for the main conversation and subagents alike; while a write costs money a
        1-hour write is 2x against 1.25x, so the metered rule set defaults to <code>5m</code> throughout.
      </>
    ),
  },
];
// An idle 5m entry is rewritten by a probe about every four minutes; a 1h entry once an hour.
const PROBES_PER_IDLE_HOUR = 15;

const usd = (n: number) => `$${n.toFixed(2)}`;

/**
 * Later turns that must read the prefix before the write has paid for itself:
 * the write's premium over plain input, divided by what each cached read saves
 * against sending the same token uncached.
 */
const payoffTurns = (p: FastPrice, write: number) => {
  const saved = p.inputPerM - p.cacheReadPerM;
  return write > 0 && saved > 0 ? ((write - p.inputPerM) / saved).toFixed(1) : null;
};
/** Two decimals at most, trailing zeros dropped: 2.75x, 2.1x, 1.63x. */
const multiple = (n: number) => `${Number(n.toFixed(2))}x`;

const idleHour = (p: FastPrice, write: number, probes: number) => (p.inputPerM > 0 && write > 0 ? multiple((write + probes * p.cacheReadPerM) / p.inputPerM) : null);

/** Idle minutes at which the probes a 5m entry needs have cost as much as one 1h write. */
const payoffMinutes = (p: FastPrice) => {
  if (!p.cacheWrite5mPerM || !p.cacheWrite1hPerM || p.cacheReadPerM <= 0) return null;
  const minutes = Math.round((4 * (p.cacheWrite1hPerM - p.cacheWrite5mPerM)) / p.cacheReadPerM);
  return minutes >= 60 ? 'never within 1h' : `${minutes} min`;
};

const dash = <span className="muted">-</span>;

const FAST_MODE = 'What an answer served in fast mode costs, in dollars; a subscription account pays it from usage credits, not its window';

/** One row of the dollar table: the five cells and what they make a kept-warm prefix cost. */
function PriceRow({ cells, className, label }: { cells: FastPrice; className?: string; label: ReactNode }) {
  const idle5m = idleHour(cells, cells.cacheWrite5mPerM, PROBES_PER_IDLE_HOUR);
  const idle1h = idleHour(cells, cells.cacheWrite1hPerM, 1);
  const payoff = payoffMinutes(cells);
  return (
    <tr className={className}>
      {label}
      <td className="right num">{usd(cells.inputPerM)}</td>
      <td className="right num">{cells.cacheWrite5mPerM ? usd(cells.cacheWrite5mPerM) : dash}</td>
      <td className="right num">{cells.cacheWrite1hPerM ? usd(cells.cacheWrite1hPerM) : dash}</td>
      <td className="right num">{usd(cells.cacheReadPerM)}</td>
      <td className="right num">{usd(cells.outputPerM)}</td>
      <td className="right num cost-ref__sep">{payoffTurns(cells, cells.cacheWrite5mPerM) ?? dash}</td>
      <td className="right num">{payoffTurns(cells, cells.cacheWrite1hPerM) ?? dash}</td>
      <td className="right num cost-ref__sep">{idle5m ?? dash}</td>
      <td className="right num">{idle1h ?? dash}</td>
      <td className="right num">{payoff ?? dash}</td>
    </tr>
  );
}

const PLAN_COLUMNS: { key: TokenType; label: string }[] = [
  { key: 'input', label: 'Input' },
  { key: 'cacheRead', label: 'Cache read' },
  { key: 'cacheWrite5m', label: '5m write' },
  { key: 'cacheWrite1h', label: '1h write' },
  { key: 'output', label: 'Output' },
];
const WINDOWS: { capacity: 'fiveHour' | 'sevenDay'; key: CostWindow; label: string }[] = [
  { capacity: 'fiveHour', key: 'five_hour', label: '5-hour window' },
  { capacity: 'sevenDay', key: 'seven_day', label: '7-day window' },
];

/** What each other tier spends of its own window against the anchor tier's share the group shows: the anchor's
 *  capacity over the tier's, in the file's order. */
const groupTitle = (costs: SubscriptionCosts, window: (typeof WINDOWS)[number]['capacity']) => {
  const anchor = anchorTier(costs);
  const anchored = costs.capacity[anchor]?.[window] ?? null;
  const spends = Object.keys(costs.capacity)
    .filter((tier) => tier !== anchor)
    .map((tier) => {
      const capacity = costs.capacity[tier][window];
      return anchored !== null && capacity !== null && capacity > 0
        ? `a ${planLabel(tier)} account spends ${Number((anchored / capacity).toPrecision(3))} times as much of its own`
        : `a ${planLabel(tier)} account's capacity is not measured on this window`;
    });
  return `Share of a ${planLabel(anchor)} window per million tokens; ${spends.join(', ')}`;
};
/** The share table's rows: every price table model, then every weighted model no price id reads (a dated price id
 *  reads the weight of its undated name), in the files' order. */
const shareModels = (prices: ModelPrice[], costs: SubscriptionCosts): string[] => {
  const priced = prices.map((p) => p.model);
  const undated = new Set(priced.map(undatedModel));
  return [...priced, ...Object.keys(costs.modelWeight).filter((m) => !undated.has(m))];
};

/** Opens the shipped price table next to anything that spends tokens on the user's behalf. */
export function CostReferenceLink() {
  const [open, setOpen] = useState(false);
  return (
    <>
      <button type="button" className="link-btn" onClick={() => setOpen(true)}>
        Cost reference
      </button>
      <Dialog
        open={open}
        onClose={() => setOpen(false)}
        title="Cost reference"
        subtitle="USD per million tokens, from the shipped price table, what they make a kept-warm prefix cost, and the share of a subscription window each million spends."
        width={1600}
        footer={
          <Button variant="primary" onClick={() => setOpen(false)}>
            Close
          </Button>
        }
      >
        {open && <PriceTable />}
      </Dialog>
    </>
  );
}

function PriceTable() {
  const settings = useAsync(() => api.getSettings(), []);
  const prices = settings.data?.apiPrices.models ?? null;
  const costs = settings.data?.subscriptionCosts ?? null;
  return (
    <div className="cost-ref">
      <p className="secondary">{CAVEAT}</p>
      <div className="table-wrap">
        <table className="table cost-ref__table cost-ref__prices">
          <thead>
            <tr>
              <th>Model</th>
              <th className="right">Uncached input</th>
              <th className="right">Cache write 5m</th>
              <th className="right">Cache write 1h</th>
              <th className="right">Cache read</th>
              <th className="right">Output</th>
              <th className="right cost-ref__sep">Turns to pay off, 5m</th>
              <th className="right">Turns to pay off, 1h</th>
              <th className="right cost-ref__sep">Hold warm 1h, 5m + probes</th>
              <th className="right">Hold warm 1h, 1h + probe</th>
              <th className="right">1h pays off after</th>
            </tr>
          </thead>
          <tbody>
            {prices ? (
              prices.flatMap((p) => [
                <PriceRow key={p.model} cells={p} label={<td className="mono">{p.model}</td>} />,
                ...(p.fast
                  ? [
                      <PriceRow
                        key={`${p.model} fast`}
                        cells={p.fast}
                        className="cost-ref__fast"
                        label={
                          <td className="muted" style={{ paddingLeft: 28 }} title={FAST_MODE}>
                            fast mode
                          </td>
                        }
                      />,
                    ]
                  : []),
              ])
            ) : (
              <tr>
                <td colSpan={11}>
                  <Skeleton />
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
      <div className="table-wrap">
        <table className="table cost-ref__table cost-ref__shares">
          <thead>
            <tr>
              <th rowSpan={2}>Model</th>
              {WINDOWS.map((w) => (
                <th key={w.key} colSpan={PLAN_COLUMNS.length} className="cost-ref__sep" title={costs ? groupTitle(costs, w.capacity) : undefined}>
                  {w.label}
                </th>
              ))}
            </tr>
            <tr>
              {WINDOWS.map((w) =>
                PLAN_COLUMNS.map((c, i) => (
                  <th key={`${w.key}-${c.key}`} className={`right ${i === 0 ? 'cost-ref__sep' : ''}`}>
                    {c.label}
                  </th>
                )),
              )}
            </tr>
          </thead>
          <tbody>
            {prices && costs ? (
              shareModels(prices, costs).map((model) => (
                <tr key={model}>
                  <td className="mono">{model}</td>
                  {WINDOWS.map((w) => {
                    // A rate is per million tokens, the unit the subtitle names, so a cell draws it as it is.
                    const rates = ratesOf(costs, w.key, anchorTier(costs), model);
                    return PLAN_COLUMNS.map((c, i) => (
                      <td key={`${w.key}-${c.key}`} className={`right num ${i === 0 ? 'cost-ref__sep' : ''}`}>
                        {rates === null ? dash : fmtShare(rates[c.key])}
                      </td>
                    ));
                  })}
                </tr>
              ))
            ) : (
              <tr>
                <td colSpan={1 + WINDOWS.length * PLAN_COLUMNS.length}>
                  <Skeleton />
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
      <dl className="cost-ref__notes">
        {NOTES.map((n) => (
          <div className="cost-ref__notes-row" key={n.term}>
            <dt>{n.term}</dt>
            <dd>{n.body}</dd>
          </div>
        ))}
      </dl>
    </div>
  );
}
