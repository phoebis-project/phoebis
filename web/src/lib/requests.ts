import { effectiveLabel } from '@/api/decode';
import type { CohortHold, RequestLog, Settings } from '@/api/types';
import { own } from '@/lib/analytics';
import { type Funding, fundingOf, nativeOf, revisionAt } from '@/lib/costs';
import { equivalentShort, fmtEquivalent, fmtUsd, shareLabel, shareShort, withEstimate } from '@/lib/format';
import { ageWord } from '@/lib/routingLog';
import type { UnitPosition } from '@/lib/unit';

/** The words under a dollar figure for who paid: the funding of a request billed in dollars. */
export const FUNDING_WORD: Record<Exclude<Funding, 'plan'>, string> = { api_key: 'API key', fast: 'fast mode', overage: 'overage' };
export const NO_SHARE_TITLE = 'The cost model prices no share for this request';
export const FREE_SHARE_TITLE = 'Free on the plan: a client classifier call takes no window share';

/** A wait's reason as a held row's hover names it: `cap` waited for a slot the in-flight cap freed. */
const WAIT_REASON_WORD: Record<string, string> = { cap: 'for a slot', reset: 'for the reset', rewrite: 'for the rewrite in flight', slow_down: 'for the slow-down' };
/** Where a hold waited: its cohort's account, else the last one it waited on. */
const heldOn = (held: CohortHold) => held.cohort ?? held.waitedOn[held.waitedOn.length - 1] ?? 'an account';
// Whole seconds round a wait under half a second to `0 s`, which would read as no wait at all.
const waitWord = (ms: number) => ageWord(Math.max(1_000, ms));

/**
 * A held row's badge, `waited 42 s`, and its hover, each wait with its reason in the row's order and the account it
 * waited on: `waited for the slow-down 42 s, for a slot 3 s on acct-a`. A row from before the waits names no reason, a
 * reason the console has no words for reads as the router sent it, and a wait under a second reads `1 s`.
 */
export function heldWords(held: CohortHold): { badge: string; title: string } {
  const waits = held.waits.map((w) => `${own(WAIT_REASON_WORD, w.reason) ? WAIT_REASON_WORD[w.reason] : `for ${w.reason}`} ${waitWord(w.heldMs)}`);
  return { badge: `waited ${waitWord(held.heldMs)}`, title: `waited ${waits.length > 0 ? waits.join(', ') : waitWord(held.heldMs)} on ${heldOn(held)}` };
}

/** The two windows every subscription shares, drawn first in this order, and a model's own week follows them. */
const SHARED_WINDOWS = ['five_hour', 'seven_day'];

/** A request's cost as a position reads it. A plan-funded request carries its 20x equivalents under native and the
 *  shares the route answered under the four other positions, each list the windows the route priced (a null one left
 *  out) in the order 5-hour, 7-day, then the model week. One billed in dollars carries the dollars with the funding
 *  word. One the route priced no window for carries the dollar equivalent alone, and a request no account served the
 *  zero dollars, with no funding to name. Every variant carries `estimated`, the route's flag, so the mark follows the
 *  request under every position. A plan-funded unit carries the route's `reason` where its figures read zero by
 *  rule. */
export type CostUnit =
  | { kind: 'dollars'; estimated: boolean; funding: Exclude<Funding, 'plan'>; usd: number }
  | { kind: 'equivalent'; equivalents: { key: string; share: number }[]; estimated: boolean; reason: 'client_classifier' | null; usd: number }
  | { kind: 'shares'; estimated: boolean; reason: 'client_classifier' | null; shares: { key: string; share: number }[]; usd: number }
  | { kind: 'unpriced'; estimated: boolean; usd: number }
  | { kind: 'unserved'; estimated: boolean; usd: number };

/** A window map's priced entries, the two shared windows first. */
function pricedWindows(map: Record<string, number | null> | null): { key: string; share: number }[] {
  // A null figure is a window the route could not price, which draws nothing rather than a wrong figure.
  const priced = Object.entries(map ?? {}).filter((entry): entry is [string, number] => entry[1] !== null);
  const rank = (key: string) => {
    const at = SHARED_WINDOWS.indexOf(key);
    return at < 0 ? SHARED_WINDOWS.length : at;
  };
  // A stable sort keeps the route's order among the keys past the two shared windows.
  return priced.sort((a, b) => rank(a[0]) - rank(b[0])).map(([key, share]) => ({ key, share }));
}

export function costUnitOf(r: Pick<RequestLog, 'attempts' | 'costEquivalent' | 'costShares' | 'costUsd' | 'estimated' | 'shareReason'>, unit: UnitPosition): CostUnit {
  const { costUsd: usd, estimated, shareReason: reason } = r;
  const last = r.attempts[r.attempts.length - 1];
  if (last === undefined) return { estimated, kind: 'unserved', usd };
  const funding = fundingOf(last);
  if (unit === 'native' && nativeOf(funding) === 'equivalent') {
    const equivalents = pricedWindows(r.costEquivalent);
    return equivalents.length === 0 ? { estimated, kind: 'unpriced', usd } : { equivalents, estimated, kind: 'equivalent', reason, usd };
  }
  // The four positions beside native are the Cost section's, so a subscription request keeps its account's own
  // shares under each of them.
  if (funding !== 'plan') return { estimated, funding, kind: 'dollars', usd };
  const shares = pricedWindows(r.costShares);
  return shares.length === 0 ? { estimated, kind: 'unpriced', usd } : { estimated, kind: 'shares', reason, shares, usd };
}

/** An equivalent unit's figures in full, then the dollar equivalent. */
const equivalentReading = (unit: Extract<CostUnit, { kind: 'equivalent' }>): string =>
  [...unit.equivalents.map((e) => fmtEquivalent(e.key, e.share)), `${fmtUsd(unit.usd)} equivalent`].join(', ');

/** A plan unit's reading, led by the free sentence where the route gave a reason for its zero figures. */
const freeLed = (reason: 'client_classifier' | null, reading: string): string => (reason === null ? reading : `${FREE_SHARE_TITLE}. ${reading}`);

/** The table cell's two lines and title, before the estimated mark. */
function plainCell(unit: CostUnit): { main: string; sub: string; title: string } {
  if (unit.kind === 'dollars') return { main: fmtUsd(unit.usd), sub: FUNDING_WORD[unit.funding], title: `Billed in dollars: ${FUNDING_WORD[unit.funding]}` };
  if (unit.kind === 'unpriced') return { main: fmtUsd(unit.usd), sub: 'equivalent', title: NO_SHARE_TITLE };
  if (unit.kind === 'unserved') return { main: fmtUsd(unit.usd), sub: '', title: '' };
  if (unit.kind === 'equivalent') {
    // One request's cell leads with the 5-hour window. A request with no 5-hour figure leads with the next window it
    // priced, so the cell never draws an empty main line.
    const [main, sub] = unit.equivalents;
    return {
      main: equivalentShort(main.key, main.share),
      sub: sub ? equivalentShort(sub.key, sub.share) : '',
      title: freeLed(unit.reason, equivalentReading(unit)),
    };
  }
  const main = unit.shares.find((s) => !SHARED_WINDOWS.includes(s.key)) ?? unit.shares.find((s) => s.key === 'seven_day') ?? unit.shares[0];
  const fiveHour = unit.shares.find((s) => s.key === 'five_hour');
  return {
    main: shareShort(main.key, main.share),
    // A row priced on the 5-hour window alone reads it as its main line and not again under it.
    sub: fiveHour && fiveHour !== main ? shareShort(fiveHour.key, fiveHour.share) : '',
    title: freeLed(unit.reason, [...unit.shares.map((s) => shareLabel(s.key, s.share)), `${fmtUsd(unit.usd)} equivalent`].join(', ')),
  };
}

/** The table cell's two lines and title. An equivalent unit draws the 5-hour equivalent over the 7-day one, a share
 *  unit the model week's share where there is one, else the 7-day's, over the 5-hour share, each in its window's short
 *  word, with every figure in full and the dollar equivalent on the title, the free sentence ahead of them where the
 *  route gave a reason for zero figures. A dollar unit draws the dollars over the funding word or `equivalent`, and an
 *  unserved request's dollars alone, with no second line and no title. A figure the route marked estimated reads a
 *  tilde before its main line and the estimate after its title. */
export function costCellOf(unit: CostUnit): { main: string; sub: string; title: string } {
  const cell = plainCell(unit);
  return unit.estimated ? { main: `~${cell.main}`, sub: cell.sub, title: withEstimate(cell.title, true) } : cell;
}

/**
 * The price list and the cost model in force at a request's instant, as a sentence for a title, or null while the
 * settings are not at hand.
 */
export function revisionsTitle(settings: Pick<Settings, 'apiPrices' | 'subscriptionCosts'> | null, at: string): string | null {
  if (settings === null) return null;
  // A list with no entry has no revision to name, and `effectiveLabel` reads a date string.
  const dateOf = (entry: { effective: string } | null) => (entry === null ? 'no revision' : effectiveLabel(entry.effective));
  return `Priced at the API prices of ${dateOf(revisionAt(settings.apiPrices.revisions, at))} and the cost model of ${dateOf(revisionAt(settings.subscriptionCosts.revisions, at))}`;
}

/**
 * The Dashboard's recent Cost cell, a glance on one line. An equivalent unit reads its lead window's figure alone in
 * the 20x short form, and the title carries the full reading, the revisions in force and the estimate. Every other
 * unit reads as the Requests cell.
 */
export function glanceCellOf(unit: CostUnit, revisions: string | null): { main: string; sub: string; title: string } {
  if (unit.kind !== 'equivalent') return costCellOf(unit);
  const [lead] = unit.equivalents;
  const title = [equivalentReading(unit), revisions].filter((part) => part !== null).join('. ');
  return { main: `${unit.estimated ? '~' : ''}${equivalentShort(lead.key, lead.share)}`, sub: '', title: withEstimate(title, unit.estimated) };
}
