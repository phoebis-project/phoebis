import { effectiveLabel } from '@/api/decode';
import type { Account, CohortHold, RequestLog, Settings, SubscriptionCosts } from '@/api/types';
import { hintPlan } from '@/lib/analytics';
import { type Funding, fundingOf, inBand, nativeOf, revisionAt } from '@/lib/costs';
import { lookup } from '@/lib/counts';
import { fmtUsd, fmtWeek, weekShort } from '@/lib/format';
import { ageWord } from '@/lib/routingLog';
import type { UnitPosition, WeekPosition } from '@/lib/unit';

/** The route's order, newest first and the higher id first at one instant, which a poll's merge and the mock keep. */
export const compareRequests = (a: RequestLog, b: RequestLog) => Date.parse(b.at) - Date.parse(a.at) || (a.id < b.id ? 1 : a.id > b.id ? -1 : 0);

/** The words under a dollar figure for who paid: the funding of a request billed in dollars. */
export const FUNDING_WORD: Record<Exclude<Funding, 'plan'>, string> = { api_key: 'API key', fast: 'fast mode', overage: 'overage', usage_based: 'usage-based' };
export const NO_SHARE_TITLE = 'The cost model prices no share for this request';
export const NO_EQUIVALENT_TITLE = 'Billed in dollars, no subscription equivalent';
export const NOTHING_BILLED_TITLE = 'Nothing billed: a subscription request';
export const FREE_SHARE_TITLE = 'Free on the plan: a client classifier call takes no window share';
/**
 * The refusals the request figures leave out, named on the Refused tile's hover and the Traffic table's Refused column.
 */
export const REFUSED_TITLE =
  "Requests refused for the client's or a policy's cause before any account was asked. The router's own refusals, account_canceled, accounts_unavailable, body_budget_exhausted, key_body_reads, ledger_behind and router_unavailable, count as requests instead.";

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
  const waits = held.waits.map((w) => `${lookup(WAIT_REASON_WORD, w.reason) ?? `for ${w.reason}`} ${waitWord(w.heldMs)}`);
  const badge = `waited ${waitWord(held.heldMs)}`;
  // The router fills the accounts waited on from the same holds in the same order, so each wait names its own.
  if (waits.length > 0 && new Set(held.waitedOn).size > 1) return { badge, title: `waited ${waits.map((w, i) => `${w} on ${held.waitedOn[i] ?? heldOn(held)}`).join(', ')}` };
  return { badge, title: `waited ${waits.length > 0 ? waits.join(', ') : waitWord(held.heldMs)} on ${heldOn(held)}` };
}

/**
 * A request's cost as a position reads it, each position reading what its line of the unit legend names. A plan-funded
 * request carries its Pro-week figure under native, its dollar equivalent beside it under All in dollars and Dollar
 * equivalent, and a dash under Billed dollars, with the hint where the surface composed one. One billed in dollars
 * carries the dollars with the funding word, and a dash under Dollar equivalent. One the route priced nothing for
 * carries the dollar equivalent alone, and a request no account served the zero dollars, with no funding to name. A
 * unit of the figure carries the route's `reason` where the figure reads zero by rule.
 */
export type CostUnit =
  | { kind: 'dash'; title: string; usd: number }
  | { kind: 'dollars'; funding: Exclude<Funding, 'plan'>; usd: number }
  | { kind: 'equivalent'; figure: number; hint: string | null; reason: 'client_classifier' | null; usd: number }
  | { kind: 'equivalent_dollars'; figure: number; hint: string | null; reason: 'client_classifier' | null; usd: number }
  | { kind: 'unpriced'; usd: number }
  | { kind: 'unserved'; usd: number };

const COST_HEADER_TITLE: Record<UnitPosition, string> = {
  all: 'Dollar equivalent for a request a subscription window paid, dollars for an API key, fast mode, overage or usage-based request',
  equivalent: 'Dollar equivalent for a request a subscription window paid, a dash for an API key, fast mode, overage or usage-based request',
  native: 'A share of the chosen week for a request a subscription window paid, dollars for an API key, fast mode, overage or usage-based request',
  usd: 'A dash for a request a subscription window paid, dollars for an API key, fast mode, overage or usage-based request',
};

/** The Cost header's title per position, naming what a subscription and a billed request read under it. */
export function costHeaderTitle(unit: UnitPosition): string {
  return COST_HEADER_TITLE[unit];
}

/**
 * The hint's arguments for a request: the serving account's plan, its display name where the accounts read holds it,
 * the shares the route answered, the tier the attempt stored and the band at the request's completion where the cost
 * model is at hand. Null while the accounts read is out, and where the row names no plan to label.
 */
export function requestHintArgs(r: Pick<RequestLog, 'accountId' | 'at' | 'costShares' | 'durationMs' | 'tier'>, accounts: Account[] | null, costs: SubscriptionCosts | null): [string, Record<string, number | null> | null, string | null, boolean | null] | null {
  if (accounts === null) return null;
  const plan = hintPlan(accounts.find((a) => a.id === r.accountId), r.tier);
  return plan === null ? null : [plan, r.costShares, r.tier, costs === null ? null : inBand(costs, Date.parse(r.at) + (r.durationMs ?? 0))];
}

/** `hint` is `shareHint` over the row, composed by the surface that holds the account and the cost model. */
export function costUnitOf(r: Pick<RequestLog, 'attempts' | 'costEquivalent' | 'costShares' | 'costUsd' | 'shareReason' | 'tier'>, unit: UnitPosition, hint: string | null): CostUnit {
  const { costUsd: usd, shareReason: reason } = r;
  const last = r.attempts[r.attempts.length - 1];
  if (last === undefined) return { kind: 'unserved', usd };
  const funding = fundingOf(last);
  if (unit === 'native' && nativeOf(funding) === 'equivalent') return r.costEquivalent === null ? { kind: 'unpriced', usd } : { figure: r.costEquivalent, hint, kind: 'equivalent', reason, usd };
  // A billed request has no subscription equivalent to read, and a subscription request bills no dollars.
  if (funding !== 'plan') return unit === 'equivalent' ? { kind: 'dash', title: NO_EQUIVALENT_TITLE, usd } : { funding, kind: 'dollars', usd };
  if (unit === 'usd') return { kind: 'dash', title: NOTHING_BILLED_TITLE, usd };
  return r.costEquivalent === null ? { kind: 'unpriced', usd } : { figure: r.costEquivalent, hint, kind: 'equivalent_dollars', reason, usd };
}

/** Parts of a title as sentences, an absent part left out. */
const sentences = (parts: (string | null)[]): string => parts.filter((part): part is string => part !== null && part !== '').join('. ');

/**
 * A dollar equivalent unit's title: the hint, then the figure in the chosen week and the dollars. The figure waits on
 * the scale, so the title reads the dollars alone while it is out.
 */
const equivalentReading = (unit: Extract<CostUnit, { kind: 'equivalent_dollars' }>, week: WeekPosition, scale: number | null): string =>
  sentences([unit.hint, [...(scale === null ? [] : [fmtWeek(unit.figure * scale, week)]), `${fmtUsd(unit.usd)} equivalent`].join(', ')]);

/** A plan unit's reading, led by the free sentence where the route gave a reason for its zero figure. */
const freeLed = (reason: 'client_classifier' | null, reading: string): string => (reason === null ? reading : sentences([FREE_SHARE_TITLE, reading]));

/**
 * The table cell's main line, second line and title. Native reads the figure in the chosen week's short form on one
 * line, the title the hint, then the dollar equivalent, behind the free sentence where the route gave a reason for a
 * zero figure. A dollar equivalent draws the dollars over `equivalent` with the figure in full and the dollars on the
 * title, behind the same sentence, a dollar unit the dollars over the funding word or `equivalent`, a dash its title
 * alone, and an unserved request's dollars alone, with no second line and no title. Null under native while `scale` is
 * out, which the surface draws as its skeleton, so no Pro-week figure reads under the chosen week's label.
 */
export function costCellOf(unit: CostUnit, week: WeekPosition, scale: number | null): { main: string; sub: string; title: string } | null {
  if (unit.kind === 'dash') return { main: '-', sub: '', title: unit.title };
  if (unit.kind === 'dollars') return { main: fmtUsd(unit.usd), sub: FUNDING_WORD[unit.funding], title: `Billed in dollars: ${FUNDING_WORD[unit.funding]}` };
  if (unit.kind === 'equivalent_dollars') return { main: fmtUsd(unit.usd), sub: 'equivalent', title: freeLed(unit.reason, equivalentReading(unit, week, scale)) };
  if (unit.kind === 'unpriced') return { main: fmtUsd(unit.usd), sub: 'equivalent', title: NO_SHARE_TITLE };
  if (unit.kind === 'unserved') return { main: fmtUsd(unit.usd), sub: '', title: '' };
  if (scale === null) return null;
  return { main: weekShort(unit.figure * scale, week), sub: '', title: freeLed(unit.reason, sentences([unit.hint, `${fmtUsd(unit.usd)} equivalent`])) };
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
 * The Dashboard's recent Cost cell, a glance on one line. Every unit reads as the Requests cell, and the native figure's
 * title carries the revisions in force after the reading. Null while the scale is out, as the Requests cell is.
 */
export function glanceCellOf(unit: CostUnit, revisions: string | null, week: WeekPosition, scale: number | null): { main: string; sub: string; title: string } | null {
  const cell = costCellOf(unit, week, scale);
  return cell === null || unit.kind !== 'equivalent' ? cell : { ...cell, title: sentences([cell.title, revisions]) };
}
