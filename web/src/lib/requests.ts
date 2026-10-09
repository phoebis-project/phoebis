import type { RequestLog } from '@/api/types';
import { type Funding, fundingOf } from '@/lib/costs';
import { fmtUsd, shareLabel } from '@/lib/format';

/** The words under a dollar figure for who paid: the funding of a request billed in dollars. */
export const FUNDING_WORD: Record<Exclude<Funding, 'plan'>, string> = { api_key: 'API key', fast: 'fast mode', overage: 'overage' };
export const NO_SHARE_TITLE = 'The cost model prices no share for this request';

/** The two windows every subscription shares, drawn first in this order; a model's own week follows them. */
const SHARED_WINDOWS = ['five_hour', 'seven_day'];

/** A request's cost in its native unit: the shares the route answered for a plan-funded request (the windows it
 *  priced, a null one left out), in the order 5-hour, 7-day, then the model week; the dollars with the funding word
 *  for one billed in dollars; the dollar equivalent alone where the route priced no share. */
export type CostUnit =
  | { kind: 'dollars'; funding: Exclude<Funding, 'plan'>; usd: number }
  | { kind: 'shares'; shares: { key: string; share: number }[]; usd: number }
  | { kind: 'unpriced'; usd: number };

export function costUnitOf(r: Pick<RequestLog, 'attempts' | 'costShares' | 'costUsd'>): CostUnit {
  const funding = fundingOf(r.attempts[r.attempts.length - 1] ?? null);
  if (funding !== 'plan') return { kind: 'dollars', funding, usd: r.costUsd };
  // A null share is a window the route could not price, which draws nothing rather than a wrong figure.
  const priced = Object.entries(r.costShares ?? {}).filter((entry): entry is [string, number] => entry[1] !== null);
  if (priced.length === 0) return { kind: 'unpriced', usd: r.costUsd };
  const rank = (key: string) => {
    const at = SHARED_WINDOWS.indexOf(key);
    return at < 0 ? SHARED_WINDOWS.length : at;
  };
  // A stable sort keeps the route's order among the keys past the two shared windows.
  const shares = priced.sort((a, b) => rank(a[0]) - rank(b[0])).map(([key, share]) => ({ key, share }));
  return { kind: 'shares', shares, usd: r.costUsd };
}

/** The table cell's two lines and title: the model week's share where there is one, else the 7-day's, over the
 *  5-hour share; the dollars over the funding word or `equivalent`; every share and the equivalent on the title. */
export function costCellOf(unit: CostUnit): { main: string; sub: string; title: string } {
  if (unit.kind === 'dollars') return { main: fmtUsd(unit.usd), sub: FUNDING_WORD[unit.funding], title: `Billed in dollars: ${FUNDING_WORD[unit.funding]}` };
  if (unit.kind === 'unpriced') return { main: fmtUsd(unit.usd), sub: 'equivalent', title: NO_SHARE_TITLE };
  const main = unit.shares.find((s) => !SHARED_WINDOWS.includes(s.key)) ?? unit.shares.find((s) => s.key === 'seven_day') ?? unit.shares[0];
  const fiveHour = unit.shares.find((s) => s.key === 'five_hour');
  return {
    main: shareLabel(main.key, main.share),
    sub: fiveHour ? shareLabel(fiveHour.key, fiveHour.share) : '',
    title: [...unit.shares.map((s) => shareLabel(s.key, s.share)), `${fmtUsd(unit.usd)} equivalent`].join(', '),
  };
}
