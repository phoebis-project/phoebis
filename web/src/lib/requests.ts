import { effectiveLabel } from '@/api/decode';
import type { Account, CohortHold, RequestLog, SessionSpend, SessionSpendPart, Settings } from '@/api/types';
import type { RequestOrderWord, SortDir } from '@/lib/address';
import { heldSpan, hintPlan, ownShareHint, sentences, shareHint } from '@/lib/analytics';
import { type Funding, fundingOf, inBand, revisionAt } from '@/lib/costs';
import { has, lookup } from '@/lib/counts';
import { servingAttempt } from '@/lib/dimensions';
import { byteOrder, fmtUsd, fmtWeek, plural, weekShort } from '@/lib/format';
import { heldAccount, WAIT_REASON_WORD } from '@/lib/routingLog';
import { compareFigures } from '@/lib/sort';
import type { UnitPosition, WeekPosition } from '@/lib/unit';

/** The route's order, newest first and the higher id first at one instant, which a poll's merge and the mock keep. */
export const compareRequests = (a: RequestLog, b: RequestLog) => Date.parse(b.at) - Date.parse(a.at) || byteOrder(b.id, a.id);

/** The route's order key read off the row, null where the row carries no figure. */
function figureOf(r: RequestLog, order: RequestOrderWord): number | null {
  switch (order) {
    case 'received_at':
      return Date.parse(r.at);
    case 'attempts':
      return r.attempts.length;
    case 'duration_ms':
      return r.durationMs;
    case 'ttfb_ms':
      return r.ttfbMs;
  }
}

/**
 * The read's cursor pair as a comparator, so a poll's rows merge where the route would page them: the figure in `dir`,
 * a row without one last under either direction, and on a tie the id in `dir`, as the route orders `id {dir}`.
 */
export function requestOrder(order: RequestOrderWord, dir: SortDir): (a: RequestLog, b: RequestLog) => number {
  const flip = dir === 'desc' ? -1 : 1;
  return (a, b) => compareFigures(figureOf(a, order), figureOf(b, order), dir) || flip * byteOrder(a.id, b.id);
}

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

const heldOn = (held: CohortHold) => heldAccount(held) ?? 'an account';

/**
 * A held row's badge, `waited 42 s`, and its hover, each wait with its reason in the row's order and the account it
 * waited on: `waited for the slow-down 42 s, for a slot 3 s on acct-a`, or where the waits span two accounts each with
 * its own, `waited for the slow-down 42 s on studio, for a slot 3 s on primary`. A row from before the waits names no
 * reason, a reason the console has no words for reads as the router sent it, and a wait under a second reads `1 s`.
 */
export function heldWords(held: CohortHold): { badge: string; title: string } {
  const waits = held.waits.map((w) => `${lookup(WAIT_REASON_WORD, w.reason) ?? `for ${w.reason}`} ${heldSpan(w.heldMs)}`);
  const badge = `waited ${heldSpan(held.heldMs)}`;
  // The router fills the accounts waited on from the same holds in the same order, so each wait names its own.
  if (waits.length > 0 && new Set(held.waitedOn).size > 1) return { badge, title: `waited ${waits.map((w, i) => `${w} on ${held.waitedOn[i] ?? heldOn(held)}`).join(', ')}` };
  return { badge, title: `waited ${waits.length > 0 ? waits.join(', ') : heldSpan(held.heldMs)} on ${heldOn(held)}` };
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

/** The cost model with its revisions, as the settings carry it. */
export type CostTable = Pick<Settings['subscriptionCosts'], 'peak' | 'revisions'>;
/**
 * The hint's arguments for a request: the serving account's plan, its display name where the accounts read holds it,
 * the shares the route answered, the tier the attempt stored and the band at the request's completion where the cost
 * model is at hand. Null while the accounts read is out, and where the row names no plan to label.
 */
export function requestHintArgs(r: Pick<RequestLog, 'accountId' | 'at' | 'costShares' | 'durationMs' | 'tier'>, accounts: Account[] | null, costs: CostTable | null): [string, Record<string, number | null> | null, string | null, boolean | null] | null {
  if (accounts === null) return null;
  const plan = hintPlan(accounts.find((a) => a.id === r.accountId), r.tier);
  if (plan === null) return null;
  const completion = Date.parse(r.at) + (r.durationMs ?? 0);
  // The band in force when the request completed, which the loaded table's band can differ from after a revision.
  const table = costs === null ? null : (revisionAt(costs.revisions, completion)?.content ?? costs);
  return [plan, r.costShares, r.tier, table === null ? null : inBand(table, completion)];
}

/**
 * A funding's unit under a position, alike for a request and a session's spend. `dollars` is a billed funding's
 * figure, a request's own dollars or a spend's billed ones, and `usd` the list dollars the other units carry.
 */
function fundedUnit(funding: Funding, unit: UnitPosition, p: { dollars: number; figure: number | null; hint: string | null; reason: 'client_classifier' | null; usd: number }): CostUnit {
  const { usd } = p;
  // A billed request has no subscription equivalent to read, and a subscription request bills no dollars.
  if (funding !== 'plan') return unit === 'equivalent' ? { kind: 'dash', title: NO_EQUIVALENT_TITLE, usd } : { funding, kind: 'dollars', usd: p.dollars };
  if (unit === 'usd') return { kind: 'dash', title: NOTHING_BILLED_TITLE, usd };
  if (p.figure === null) return { kind: 'unpriced', usd };
  return { figure: p.figure, hint: p.hint, kind: unit === 'native' ? 'equivalent' : 'equivalent_dollars', reason: p.reason, usd };
}

/** `hint` is `shareHint` over the row, composed by the surface that holds the account and the cost model. */
export function costUnitOf(r: Pick<RequestLog, 'attempts' | 'costEquivalent' | 'costShares' | 'costUsd' | 'shareReason' | 'tier'>, unit: UnitPosition, hint: string | null): CostUnit {
  const { costUsd: usd } = r;
  const last = servingAttempt(r);
  if (last === null) return { kind: 'unserved', usd };
  return fundedUnit(fundingOf(last), unit, { dollars: usd, figure: r.costEquivalent, hint, reason: r.shareReason, usd });
}

/** A request's cost unit with its hint composed from the accounts read and the cost model. */
export function requestCostUnit(r: RequestLog, unit: UnitPosition, accounts: Account[] | null, costs: CostTable | null): CostUnit {
  const args = requestHintArgs(r, accounts, costs);
  return costUnitOf(r, unit, args === null ? null : shareHint(...args));
}

/** The Cost cell's reading of a session's spend. */
export interface SpendReading {
  /** The other accounts whose plan windows the requests spent on, in id order, which the "+N" badge names. An API-key
   *  account bills dollars and writes no window share, so none is among them. */
  accounts: string[];
  /** The dollars billed outside a plan beside a plan figure, summed over the fundings, the `+$` badge's figure, 0 with
   *  none and where the figure is the billed dollars themselves. */
  billed: number;
  /** The cell's unit as the request log's cell reads one, which `costCellOf` prints in the chosen week. */
  unit: CostUnit;
}

const billedSum = (part: Pick<SessionSpendPart, 'billedUsd'>): number => Object.values(part.billedUsd).reduce((n, usd) => n + usd, 0);

/** Every plan-funded request writes its serving account's key in `shares`, a free one with no share, so a key there
 *  marks a plan as the payer. */
const planFunded = (part: Pick<SessionSpendPart, 'shares'>): boolean => Object.keys(part.shares).length > 0;

/** The funding word of the largest billed entry, ties by word, null where no entry names a funding. */
function largestFunding(billedUsd: Record<string, number>): Exclude<Funding, 'plan'> | null {
  const named = Object.entries(billedUsd).filter((e): e is [Exclude<Funding, 'plan'>, number] => has(FUNDING_WORD, e[0]));
  named.sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
  return named.length === 0 ? null : named[0][0];
}

/**
 * A spend read through `costUnitOf`'s arms as one request: plan-funded where `shares` names an account, since every
 * plan-funded request writes its serving account's key there (a free one with no share), and billed under the largest
 * `billedUsd` entry's funding otherwise. A plan spend reads its Pro-week figure under native and its dollar equivalent
 * under All in dollars and Dollar equivalent, the dollars billed beside it on the `+$` badge, and under Billed dollars
 * those dollars or a dash. A spend no plan paid reads its billed dollars as the figure, a dash under Dollar equivalent,
 * with no `+$` beside a figure that already is the dollars. A spend of no request reads zero dollars, and requests no
 * account served their list dollars, as an unserved request reads. Nothing is read off `equivalent === 0`, since a free
 * plan-funded request reads 0 too.
 */
export function spendUnitOf(spend: SessionSpendPart, unit: UnitPosition, accountId: string | null, hint: string | null): SpendReading {
  const accounts = Object.keys(spend.shares).filter((id) => id !== accountId).sort();
  if (spend.requests === 0) return { accounts, billed: 0, unit: { kind: 'unserved', usd: 0 } };
  const billed = billedSum(spend);
  const figures = { dollars: billed, figure: spend.equivalent, hint, reason: null, usd: spend.usd };
  if (planFunded(spend)) {
    // The plan requests' list dollars: `usd` sums every served request's, and `billedUsd` holds the billed ones' by
    // funding, which the `+$` badge carries.
    const plan = { ...figures, usd: spend.usd - billed };
    if (unit !== 'usd') return { accounts, billed, unit: fundedUnit('plan', unit, plan) };
    const funding = largestFunding(spend.billedUsd);
    return { accounts, billed: 0, unit: funding === null ? fundedUnit('plan', unit, plan) : { funding, kind: 'dollars', usd: billed } };
  }
  const funding = largestFunding(spend.billedUsd);
  if (funding === null) return { accounts, billed: 0, unit: { kind: 'unserved', usd: spend.usd } };
  return { accounts, billed: 0, unit: fundedUnit(funding, unit, figures) };
}

/**
 * The hint over the one account the shares name, its own share hint at its current tier, null where they name none or
 * more than one or the accounts read is out.
 */
export function spendHint(spend: Pick<SessionSpendPart, 'shares'>, accounts: Account[] | null): string | null {
  const ids = Object.keys(spend.shares);
  if (accounts === null || ids.length !== 1) return null;
  return ownShareHint(accounts.find((a) => a.id === ids[0]), spend.shares[ids[0]]);
}

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
 * A spend's figure in full: the Pro-week figure in the chosen week where a plan paid and the scale is at hand, else its
 * dollars, the billed ones where no plan paid.
 */
function spendFigure(part: SessionSpendPart, week: WeekPosition, scale: number | null): string {
  const plan = planFunded(part);
  if (plan && part.equivalent !== null && scale !== null) return fmtWeek(part.equivalent * scale, week);
  const billed = billedSum(part);
  return fmtUsd(plan ? part.usd - billed : billed === 0 ? part.usd : billed);
}

/**
 * The cell's hover: `{n} requests`, the figure in full in the chosen week (or the dollars), the hint where one stands,
 * the billed dollars by funding, then `+{figure} in {n} probes` where the spend carries probes.
 */
export function spendHover(spend: SessionSpend, week: WeekPosition, scale: number | null, hint: string | null): string {
  const billed = Object.entries(spend.billedUsd)
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([funding, dollars]) => `${fmtUsd(dollars)} ${lookup(FUNDING_WORD, funding) ?? funding}`);
  const probes = spend.probes.requests > 0 ? `+${spendFigure(spend.probes, week, scale)} in ${plural(spend.probes.requests, 'probe')}` : null;
  return sentences([plural(spend.requests, 'request'), spendFigure(spend, week, scale), hint, billed.length === 0 ? null : `Billed in dollars: ${billed.join(', ')}`, probes]);
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
