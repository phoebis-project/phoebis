import { useState, type ReactNode } from 'react';
import { api } from '@/api';
import type { CostTable, ModelPrice, PlanModelCells, TierFactors } from '@/api/types';
import { own } from '@/lib/analytics';
import { fmtShare } from '@/lib/format';
import { useAsync } from '@/lib/hooks';
import { Button, Dialog, Skeleton } from './primitives';

const CAVEAT = 'API list prices. A subscription upstream on plan quota spends the window share columns instead of money.';
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
        Each plan column is the share of a Max 5x subscription's 5-hour or 7-day window that one million tokens of the type consume, from the cost table the ledger ships. An account on
        another tier spends that share over its tier's factor from the tier factor table, so a Pro account spends more of its own window and a Max 20x account less, by the multiples a group
        header's tooltip gives. A share marked * is not yet measured on its window and carries a stand-in derived from earlier meter readings. A 1-hour write spends the same share as a 5-minute
        one, so on plan-funded traffic the long TTL is free and the pool's plan rule set keeps <code>1h</code> for the main conversation and subagents alike; while a write costs money a
        1-hour write is 2x against 1.25x, so the metered rule set's <code>keepalive</code> TTL defaults to <code>5m</code>.
      </>
    ),
  },
];
const UNMEASURED = 'not measured on this window; derived from earlier meter readings';
// An idle 5m entry is rewritten by a probe about every four minutes; a 1h entry once an hour.
const PROBES_PER_IDLE_HOUR = 15;

const usd = (n: number) => `$${n.toFixed(2)}`;

/**
 * Later turns that must read the prefix before the write has paid for itself:
 * the write's premium over plain input, divided by what each cached read saves
 * against sending the same token uncached.
 */
const payoffTurns = (p: ModelPrice, write: number) => {
  const saved = p.inputPerM - p.cacheReadPerM;
  return write > 0 && saved > 0 ? ((write - p.inputPerM) / saved).toFixed(1) : null;
};
/** Two decimals at most, trailing zeros dropped: 2.75x, 2.1x, 1.63x. */
const multiple = (n: number) => `${Number(n.toFixed(2))}x`;

const idleHour = (p: ModelPrice, write: number, probes: number) => (p.inputPerM > 0 && write > 0 ? multiple((write + probes * p.cacheReadPerM) / p.inputPerM) : null);

/** Idle minutes at which the probes a 5m entry needs have cost as much as one 1h write. */
const payoffMinutes = (p: ModelPrice) => {
  if (!p.cacheWrite5mPerM || !p.cacheWrite1hPerM || p.cacheReadPerM <= 0) return null;
  const minutes = Math.round((4 * (p.cacheWrite1hPerM - p.cacheWrite5mPerM)) / p.cacheReadPerM);
  return minutes >= 60 ? 'never within 1h' : `${minutes} min`;
};

const dash = <span className="muted">-</span>;

/** The model's cells on a window, exact first and then undated, since the price table lists dated ids and the cost table ships undated. */
const cellsOf = (table: CostTable, window: string, model: string): PlanModelCells | null => {
  const models = own(table.windows, window) ? table.windows[window] : {};
  const undated = model.replace(/-\d{8}$/, '');
  return own(models, model) ? models[model] : own(models, undated) ? models[undated] : null;
};
const PLAN_COLUMNS: { key: keyof PlanModelCells; label: string }[] = [
  { key: 'input', label: 'Input' },
  { key: 'cacheRead', label: 'Cache read' },
  { key: 'cacheWrite5m', label: '5m write' },
  { key: 'cacheWrite1h', label: '1h write' },
  { key: 'output', label: 'Output' },
];
const WINDOWS = [
  { factor: 'fiveHour', key: 'five_hour', label: '5-hour window' },
  { factor: 'sevenDay', key: 'seven_day', label: '7-day window' },
] as const;

/** What a Pro and a Max 20x account spend of their own window against the Max 5x share the group shows. */
const groupTitle = (tiers: TierFactors, window: (typeof WINDOWS)[number]['factor']) => {
  const spends = (tier: string, words: (times: number | string) => string) => {
    const factor = own(tiers.tiers, tier) ? tiers.tiers[tier][window] : null;
    const times = factor && factor.factor > 0 ? Number((1 / factor.factor).toPrecision(3)) : '-';
    return `${words(times)}${factor?.unmeasured ? ' (not measured)' : ''}`;
  };
  const pro = spends('default_claude_pro', (times) => `a Pro account spends ${times} times as much of its own`);
  const max20x = spends('default_claude_max_20x', (times) => `a Max 20x ${times} times`);
  return `Share of a Max 5x window per million tokens; ${pro}, ${max20x}`;
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
  const prices = settings.data?.pricing.models ?? null;
  const tables = settings.data?.planTables ?? null;
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
              prices.map((p) => {
                const idle5m = idleHour(p, p.cacheWrite5mPerM, PROBES_PER_IDLE_HOUR);
                const idle1h = idleHour(p, p.cacheWrite1hPerM, 1);
                const payoff = payoffMinutes(p);
                return (
                  <tr key={p.model}>
                    <td className="mono">{p.model}</td>
                    <td className="right num">{usd(p.inputPerM)}</td>
                    <td className="right num">{p.cacheWrite5mPerM ? usd(p.cacheWrite5mPerM) : dash}</td>
                    <td className="right num">{p.cacheWrite1hPerM ? usd(p.cacheWrite1hPerM) : dash}</td>
                    <td className="right num">{usd(p.cacheReadPerM)}</td>
                    <td className="right num">{usd(p.outputPerM)}</td>
                    <td className="right num cost-ref__sep">{payoffTurns(p, p.cacheWrite5mPerM) ?? dash}</td>
                    <td className="right num">{payoffTurns(p, p.cacheWrite1hPerM) ?? dash}</td>
                    <td className="right num cost-ref__sep">{idle5m ?? dash}</td>
                    <td className="right num">{idle1h ?? dash}</td>
                    <td className="right num">{payoff ?? dash}</td>
                  </tr>
                );
              })
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
                <th key={w.key} colSpan={PLAN_COLUMNS.length} className="cost-ref__sep" title={tables ? groupTitle(tables.tiers, w.factor) : undefined}>
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
            {prices ? (
              prices.map((p) => (
                <tr key={p.model}>
                  <td className="mono">{p.model}</td>
                  {WINDOWS.map((w) => {
                    const cells = tables ? cellsOf(tables.cost, w.key, p.model) : null;
                    return PLAN_COLUMNS.map((c, i) => (
                      <td key={`${w.key}-${c.key}`} className={`right num ${i === 0 ? 'cost-ref__sep' : ''}`}>
                        {cells ? (
                          <>
                            {fmtShare(cells[c.key].sharePerMillion)}
                            {cells[c.key].unmeasured && (
                              <span className="muted cost-ref__flag" title={UNMEASURED}>
                                *
                              </span>
                            )}
                          </>
                        ) : (
                          dash
                        )}
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
      <p className="secondary cost-ref__legend">
        <span className="muted">*</span> {UNMEASURED}
      </p>
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
