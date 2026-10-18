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
export const NO_EQUIVALENT_TITLE = 'Billed in dollars, no subscription equivalent';
export const NOTHING_BILLED_TITLE = 'Nothing billed: a subscription request';

/** A wait's reason as a held row's hover names it: `cap` waited for a slot the in-flight cap freed. */
const WAIT_REASON_WORD: Record<string, string> = { cap: 'for a slot', reset: 'for the reset', rewrite: 'for the rewrite in flight', slow_down: 'for the slow-down' };
/** Where a hold waited: its cohort's account, else the last one it waited on. */
const heldOn = (held: CohortHold) => held.cohort ?? held.waitedOn[held.waitedOn.length - 1] ?? 'an account';
// Whole seconds round a wait under half a second to `0 s`, which would read as no wait at all.
const waitWord = (ms: number) => ageWord(Math.max(1_000, ms));

/**
 * A held row's badge, `waited 42 s`, and its hover, each wait with its reason in the row's order and the account it
 * waited on: `waited for the slow-down 42 s, for a slot 3 s on acct-a`, or where the waits span two accounts each with
 * its own, `waited for the slow-down 42 s on studio, for a slot 3 s on primary`. A row from before the waits names no
 * reason, a reason the console has no words for reads as the router sent it, and a wait under a second reads `1 s`.
 */
export function heldWords(held: CohortHold): { badge: string; title: string } {
  const waits = held.waits.map((w) => `${own(WAIT_REASON_WORD, w.reason) ? WAIT_REASON_WORD[w.reason] : `for ${w.reason}`} ${waitWord(w.heldMs)}`);
  const badge = `waited ${waitWord(held.heldMs)}`;
  // The router fills the accounts waited on from the same holds in the same order, so each wait names its own.
  if (waits.length > 0 && new Set(held.waitedOn).size > 1) return { badge, title: `waited ${waits.map((w, i) => `${w} on ${held.waitedOn[i] ?? heldOn(held)}`).join(', ')}` };
  return { badge, title: `waited ${waits.length > 0 ? waits.join(', ') : waitWord(held.heldMs)} on ${heldOn(held)}` };
}

/** The two windows every subscription shares, drawn first in this order, and a model's own week follows them. */
const SHARED_WINDOWS = ['five_hour', 'seven_day'];

/** A request's cost as a position reads it, each position reading what its line of the unit legend names. A plan-funded
 *  request carries its 20x equivalents under native, its dollar equivalent beside them under All in dollars and Dollar
 *  equivalent, the shares the route answered under Window shares and a dash under Billed dollars, each list the
 *  windows the route priced (a null one left out) in the order 5-hour, 7-day, then the model week. One billed in
 *  dollars carries the dollars with the funding word, and a dash under Dollar equivalent. One the route priced no
 *  window for carries the dollar equivalent alone, and a request no account served the zero dollars, with no funding
 *  to name. Every variant carries `estimated`, the route's flag, so the mark follows a figure under every position. A
 *  dash keeps the flag but draws no mark, since it shows no figure to estimate. */
export type CostUnit =
  | { kind: 'dash'; estimated: boolean; title: string; usd: number }
  | { kind: 'dollars'; estimated: boolean; funding: Exclude<Funding, 'plan'>; usd: number }
  | { kind: 'equivalent'; equivalents: { key: string; share: number }[]; estimated: boolean; usd: number }
  | { kind: 'equivalent_dollars'; equivalents: { key: string; share: number }[]; estimated: boolean; usd: number }
  | { kind: 'shares'; estimated: boolean; shares: { key: string; share: number }[]; usd: number }
  | { kind: 'unpriced'; estimated: boolean; usd: number }
  | { kind: 'unserved'; estimated: boolean; usd: number };

const COST_HEADER_TITLE: Record<UnitPosition, string> = {
  all: 'Dollar equivalent for a request a subscription window paid, dollars for an overage, fast mode or API key request',
  equivalent: 'Dollar equivalent for a request a subscription window paid, a dash for an overage, fast mode or API key request',
  native: '20x window equivalents for a request a subscription window paid, dollars for an overage, fast mode or API key request',
  share: 'Window shares for a request a subscription window paid, dollars for an overage, fast mode or API key request',
  usd: 'A dash for a request a subscription window paid, dollars for an overage, fast mode or API key request',
};

/** The Cost header's title per position, naming what a subscription and a billed request read under it. */
export function costHeaderTitle(unit: UnitPosition): string {
  return COST_HEADER_TITLE[unit];
}

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

export function costUnitOf(r: Pick<RequestLog, 'attempts' | 'costEquivalent' | 'costShares' | 'costUsd' | 'estimated'>, unit: UnitPosition): CostUnit {
  const { costUsd: usd, estimated } = r;
  const last = r.attempts[r.attempts.length - 1];
  if (last === undefined) return { estimated, kind: 'unserved', usd };
  const funding = fundingOf(last);
  if (unit === 'native' && nativeOf(funding) === 'equivalent') {
    const equivalents = pricedWindows(r.costEquivalent);
    return equivalents.length === 0 ? { estimated, kind: 'unpriced', usd } : { equivalents, estimated, kind: 'equivalent', usd };
  }
  // A billed request has no subscription equivalent to read, and a subscription request bills no dollars.
  if (funding !== 'plan') return unit === 'equivalent' ? { estimated, kind: 'dash', title: NO_EQUIVALENT_TITLE, usd } : { estimated, funding, kind: 'dollars', usd };
  if (unit === 'usd') return { estimated, kind: 'dash', title: NOTHING_BILLED_TITLE, usd };
  if (unit === 'all' || unit === 'equivalent') return { equivalents: pricedWindows(r.costEquivalent), estimated, kind: 'equivalent_dollars', usd };
  const shares = pricedWindows(r.costShares);
  return shares.length === 0 ? { estimated, kind: 'unpriced', usd } : { estimated, kind: 'shares', shares, usd };
}

/** An equivalent unit's figures in full, then the dollar equivalent. */
const equivalentReading = (unit: Extract<CostUnit, { kind: 'equivalent' | 'equivalent_dollars' }>): string =>
  [...unit.equivalents.map((e) => fmtEquivalent(e.key, e.share)), `${fmtUsd(unit.usd)} equivalent`].join(', ');

/** The table cell's two lines and title, before the estimated mark. */
function plainCell(unit: CostUnit): { main: string; sub: string; title: string } {
  if (unit.kind === 'dash') return { main: '-', sub: '', title: unit.title };
  if (unit.kind === 'dollars') return { main: fmtUsd(unit.usd), sub: FUNDING_WORD[unit.funding], title: `Billed in dollars: ${FUNDING_WORD[unit.funding]}` };
  if (unit.kind === 'equivalent_dollars') return { main: fmtUsd(unit.usd), sub: 'equivalent', title: equivalentReading(unit) };
  if (unit.kind === 'unpriced') return { main: fmtUsd(unit.usd), sub: 'equivalent', title: NO_SHARE_TITLE };
  if (unit.kind === 'unserved') return { main: fmtUsd(unit.usd), sub: '', title: '' };
  if (unit.kind === 'equivalent') {
    // One request's cell leads with the 5-hour window. A request with no 5-hour figure leads with the next window it
    // priced, so the cell never draws an empty main line.
    const [main, sub] = unit.equivalents;
    return {
      main: equivalentShort(main.key, main.share),
      sub: sub ? equivalentShort(sub.key, sub.share) : '',
      title: equivalentReading(unit),
    };
  }
  const main = unit.shares.find((s) => !SHARED_WINDOWS.includes(s.key)) ?? unit.shares.find((s) => s.key === 'seven_day') ?? unit.shares[0];
  const fiveHour = unit.shares.find((s) => s.key === 'five_hour');
  return {
    main: shareShort(main.key, main.share),
    // A row priced on the 5-hour window alone reads it as its main line and not again under it.
    sub: fiveHour && fiveHour !== main ? shareShort(fiveHour.key, fiveHour.share) : '',
    title: [...unit.shares.map((s) => shareLabel(s.key, s.share)), `${fmtUsd(unit.usd)} equivalent`].join(', '),
  };
}

/** The table cell's two lines and title. An equivalent unit draws the 5-hour equivalent over the 7-day one, a share
 *  unit the model week's share where there is one, else the 7-day's, over the 5-hour share, each in its window's short
 *  word, with every figure in full and the dollar equivalent on the title. A dollar equivalent draws the dollars over
 *  `equivalent` with every equivalent in full and the dollars on the title, a dollar unit the dollars over the funding
 *  word or `equivalent`, a dash its title alone, and an unserved request's dollars alone, with no second line and no
 *  title. A figure the route marked estimated reads a tilde before its main line and the estimate after its title. */
export function costCellOf(unit: CostUnit): { main: string; sub: string; title: string } {
  const cell = plainCell(unit);
  return unit.estimated && unit.kind !== 'dash' ? { main: `~${cell.main}`, sub: cell.sub, title: withEstimate(cell.title, true) } : cell;
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
