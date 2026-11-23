import { iso } from '@/api/decode';
import type { Account, AllowanceAccount, AllowanceReset, AllowanceWindow, SubscriptionCosts, UnusedAllowanceAnswer } from '@/api/types';
import { anchorTier, pricedWindow, tierCapacity, weekScale, weekUnpricedTitle } from '@/lib/costs';
import { byteOrder, fmtAbsolute, fmtDated, fmtShare, plural, weekShort } from '@/lib/format';
import { planWordLabel, pricedTier, readWindows } from '@/lib/quota';
import { DAY, MINUTE } from '@/lib/time';
import type { WeekPosition } from '@/lib/unit';
import { WINDOW_HOURS, inWindowOrder, windowHint } from '@/lib/windows';

/** The weeks the Accounts block reads: the latest reset and the three before it. */
const ALLOWANCE_WEEKS = 4;

/** The span the block reads: 29 days behind now to now, 28 days and a day of slack so the fourth weekly reset is never
 *  cut by the hour of the week or by a clock-walk reading up to a poll interval old. */
export function allowanceSpan(now: number): { from: string; to: string } {
  return { from: iso(now - (7 * ALLOWANCE_WEEKS + 1) * DAY), to: iso(now) };
}

/** The longest span a route that reads `range` takes: the admin API refuses a custom range of more than 400 days. */
const MAX_RANGE_DAYS = 400;

/** The span the Analytics card reads: the range with a week ahead of its start, so a range that holds no reset still
 *  answers the newest one before it. The lead shortens where the week would carry the span past the routes' longest
 *  range, and the range itself is never cut, so a range longer than that leaves whole for the route to refuse. */
export function rangeAllowanceSpan(fromMs: number, toMs: number): { from: string; to: string } {
  return { from: iso(Math.min(fromMs, Math.max(fromMs - 7 * DAY, toMs - MAX_RANGE_DAYS * DAY))), to: iso(toMs) };
}

/** The resets of the accounts' 168-hour windows, the Fable window's among them, as one key that moves once a week has
 *  passed, so a read keyed on it runs again at a weekly reset and not at a poll's cadence. Each reset counts to its
 *  nearest minute, since a poll moves a reset by a fraction of a second. Each account's resets left follow its resets,
 *  since a used limit reset moves no reset and the read would keep the rows from before it. */
export const weeklyResetsKey = (accounts: readonly Account[]): string =>
  accounts
    .flatMap((a) => [
      ...readWindows(a)
        .filter((w) => w.periodHours === WINDOW_HOURS['7d'])
        .map((w) => (w.resetsAt === null ? '' : String(Math.round(Date.parse(w.resetsAt) / MINUTE)))),
      String(a.limitResets?.left ?? ''),
    ])
    .join(',');

/** The uses among an account's claim rows: one poll reads every window of the account, so the rows it saw, by the
 *  minute of `observedAt`, are one use. */
const usesOf = (claims: readonly AllowanceReset[]): number => new Set(claims.map((r) => Math.floor(Date.parse(r.observedAt) / MINUTE))).size;

/** The rows the four weeks keep: the newest `weeks` rows of the window's `resets` and every row of its `claims` newer
 *  than the oldest of them. `counted` counts the kept resets, `used` the uses among the kept claims, and `total` sums
 *  the kept rows of both lists: the answer's own total where it keeps both lists whole. */
export function newestResets(window: AllowanceWindow, weeks = ALLOWANCE_WEEKS): { claims: AllowanceReset[]; counted: number; resets: AllowanceReset[]; total: number; used: number } {
  const resets = window.resets.slice(0, weeks);
  const oldest = resets.at(-1);
  // A claim row is dated by its drop, since its `resetAt` is the reset of the week the use fell in.
  const claims = window.claims.filter((r) => oldest === undefined || Date.parse(r.observedAt) > Date.parse(oldest.resetAt));
  const whole = resets.length === window.resets.length && claims.length === window.claims.length;
  const total = whole ? window.totalShareLeft : [...resets, ...claims].reduce((sum, r) => sum + r.shareLeft, 0);
  return { claims, counted: resets.length, resets, total, used: usesOf(claims) };
}

/** A figure in the chosen week: the share of the account's own window priced through the tier's 7-day capacity over
 *  the chosen week's tier's (a model week at its capacity factor), `share` the figure in that week, and a dash where
 *  the router prices no capacity for the plan or the cost model sizes none for the tier on that window. */
export type AllowanceFigure = { kind: 'equivalent'; share: number; text: string } | { kind: 'unpriced'; text: '-'; title: string } | { kind: 'none'; text: '-'; title: string };

/** The dash's hover where the account's own plan prices no capacity. */
const ALLOWANCE_UNPRICED_TITLE = "Unpriced: the cost model holds no capacity for this account's plan";
const NO_READING_TITLE = 'no reading at this reset';
/** The Analytics card's dash for a window with no reset to draw: a row's range figure where the window never reset
 *  inside the range, its last figure where only a use ended, the cell of a window the account carries no entry for,
 *  and a fleet figure where no account counts a reset in the range. */
export const NO_RESET_IN_RANGE = 'No reset of this window in the range';
export const FREE_NOTE = 'Free has no priced capacity in the cost model, so a reset the router recorded no tier for and the total read a dash, and a week with its own tier prices at it.';

/**
 * What one share of the account's own week weighs in Pro weeks: the tier's 7-day capacity times the window's capacity
 * factor (1 for the 7-day window, the model week's own for a model week) over the anchor's 7-day capacity, so a
 * Fable-week reset reads half its 7-day figure. Null with no cost model, for a plan the router prices no tier for, a
 * null tier, a window that is no week of the 7-day window, and where the cost model sizes either side at nothing.
 */
function capacityWeight(account: Pick<AllowanceAccount, 'plan' | 'tier'>, window: string, costs: SubscriptionCosts | null): number | null {
  if (costs === null) return null;
  const gate = pricedTier(account.plan, account.tier, costs);
  if (gate.tier === null) return null;
  const priced = pricedWindow(costs, window);
  if (priced === null || priced.priced !== 'seven_day') return null;
  const capacity = tierCapacity(costs, gate.tier, 'seven_day');
  const anchor = tierCapacity(costs, anchorTier(costs), 'seven_day');
  return capacity === null || anchor === null || anchor === 0 ? null : (capacity * priced.divisor) / anchor;
}

/**
 * The pair a reset prices at: the plan and the tier of the reading it replaced where the router recorded them, the
 * account's current pair where it did not. A missing key reads as null, since the wire writes both or neither.
 */
export const pairOf = (account: Pick<AllowanceAccount, 'plan' | 'tier'>, reset?: Pick<AllowanceReset, 'plan' | 'tier'>): Pick<AllowanceAccount, 'plan' | 'tier'> =>
  reset !== undefined && (reset.plan ?? null) !== null && (reset.tier ?? null) !== null ? reset : account;

/** Whether a block draws FREE_NOTE: a free account with a figure that prices at Free, which is a drawn reset whose pair
 *  reads free (the account's own where the router recorded none) or a window's total with no reset under it. */
export const drawsFreeNote = (account: AllowanceAccount): boolean =>
  account.plan === 'free' &&
  allowanceWindows(account).some((w) => {
    const { claims, resets } = newestResets(w);
    const rows = [...resets, ...claims];
    return rows.length === 0 || rows.some((r) => pairOf(account, r).plan === 'free');
  });

/** The unpriced dash's hover. It keeps the account's own plan where the figure was priced at the account's pair,
 *  its plan and its tier, and names the plan of each other pair by its label. */
const unpricedTitle = (account: Pick<AllowanceAccount, 'plan' | 'tier'>, pair: Pick<AllowanceAccount, 'plan' | 'tier'>): string =>
  pair.plan === null || (pair.plan === account.plan && pair.tier === account.tier) ? ALLOWANCE_UNPRICED_TITLE : `Unpriced: the cost model holds no capacity for the ${planWordLabel(pair.plan)} plan`;

/** With no cost model a figure reads the unpriced dash, an arm the pages never draw, since they hold the skeleton
 *  while the model loads. */
export function allowanceFigure(share: number | null, window: string, account: Pick<AllowanceAccount, 'plan' | 'tier'>, costs: SubscriptionCosts | null, week: WeekPosition, reset?: Pick<AllowanceReset, 'plan' | 'tier'>): AllowanceFigure {
  if (share === null) return { kind: 'none', text: '-', title: NO_READING_TITLE };
  const scale = weekScale(costs, week);
  // No figure reads in a week whose tier the cost model does not size, so the dash names that week, not the plan.
  if (costs !== null && scale === null) return { kind: 'unpriced', text: '-', title: weekUnpricedTitle(week) };
  const pair = pairOf(account, reset);
  const weight = capacityWeight(pair, window, costs);
  if (weight === null || scale === null) return { kind: 'unpriced', text: '-', title: unpricedTitle(account, pair) };
  const figure = share * weight * scale;
  return { kind: 'equivalent', share: figure, text: weekShort(figure, week) };
}

/** The resets' figures, each at its own pair, added, or the dash of the first that prices nothing. */
function summedFigure(resets: readonly AllowanceReset[], window: string, account: Pick<AllowanceAccount, 'plan' | 'tier'>, costs: SubscriptionCosts | null, week: WeekPosition): AllowanceFigure {
  let sum = 0;
  for (const reset of resets) {
    const figure = allowanceFigure(reset.shareLeft, window, account, costs, week, reset);
    if (figure.kind !== 'equivalent') return figure;
    sum += figure.share;
  }
  return { kind: 'equivalent', share: sum, text: weekShort(sum, week) };
}

/**
 * A window's four weeks' figure: each reset's figure at its own pair added, so a week before a tier change weighs at
 * the tier it ran under. A reset that prices nothing leaves the total unpriced, since a sum without that week would
 * count fewer weeks than its label names.
 */
export function weeksFigure(window: AllowanceWindow, account: Pick<AllowanceAccount, 'plan' | 'tier'>, costs: SubscriptionCosts | null, week: WeekPosition): AllowanceFigure {
  const { claims, resets, total } = newestResets(window);
  const rows = [...resets, ...claims];
  if (rows.length === 0) return allowanceFigure(total, window.window, account, costs, week);
  return summedFigure(rows, window.window, account, costs, week);
}

/** The block's subtitle, which the week control's position leaves alike. */
export const ALLOWANCE_SUBTITLE = 'Each weekly window left unused when it reset, as a share of the chosen week, the last four weeks';

/** The account's weekly windows in the console's order, the 7-day window first and then each model week, a week the
 *  console does not know after the known ones in the answer's order. */
export function allowanceWindows(account: AllowanceAccount): AllowanceWindow[] {
  return inWindowOrder(account.windows, (w) => w.window);
}

/** The weekly windows the Analytics card draws a pair of columns for: the 7-day window and the Fable week always, then
 *  each further model week the accounts name, in the console's order. */
export function weeklyKeys(accounts: AllowanceAccount[]): string[] {
  const keys = ['seven_day', 'seven_day_fable'];
  for (const a of accounts) for (const w of a.windows) if (!keys.includes(w.window)) keys.push(w.window);
  return inWindowOrder(keys, (k) => k);
}

/** The block's own words for where a reading came from, since the routing log's `window reset` panel names none. */
const SOURCE_WORD = new Map([
  ['claim', 'a used limit reset'],
  ['clock', 'the clock walk'],
  ['sample', 'a usage sample'],
  ['term_ended', "the term's end"],
]);

/** The instant a row draws: the drop's for a claim row, whose `resetAt` can be a day still to come, else the reset's. */
export const drawnInstant = (reset: AllowanceReset): string => (reset.source === 'claim' ? reset.observedAt : reset.resetAt);

/** The rows of a window's two lists in the order one column draws them: by the drawn instant, newest first, a reset
 *  ahead of a claim of the same instant. */
export const drawnRows = (resets: readonly AllowanceReset[], claims: readonly AllowanceReset[]): AllowanceReset[] =>
  [...resets, ...claims].sort((a, b) => Date.parse(drawnInstant(b)) - Date.parse(drawnInstant(a)));

/** A reset's hover: its absolute instant and where its reading came from, a word the router adds read as sent. */
export const resetHover = (reset: AllowanceReset): string => `${fmtAbsolute(drawnInstant(reset))}, read from ${SOURCE_WORD.get(reset.source) ?? reset.source}`;

/** A reset's own share for the week's hover, in the hint's words: "Ultra: 39% of the 7-day window left", "Max: 12% of
 *  the Fable week left", a week the console does not know under its key. */
export const ownShareHover = (plan: string, window: string, shareLeft: number): string => `${plan}: ${fmtShare(shareLeft)} of ${windowHint(window)} left`;

/** A reset's day and time, "Sep 12 20:10", the day written even within a day of now. */
export const resetInstant = (isoTime: string): string => fmtDated(Date.parse(isoTime));

/** One account of the Analytics card: each weekly window's figures keyed by the wire key, the 7-day window's count of
 *  natural resets in the range, its used resets apart, the instant of a last reset that is the term's end, and whether
 *  a 7-day reset the row draws prices no capacity at the pair it ran under, the account's own pair standing in where it
 *  draws none. */
export interface AllowanceRow {
  accountId: string;
  /** The claim rows of the account's weekly windows in the range, each with its window, newest first. */
  claims: { reset: AllowanceReset; window: string }[];
  /** Per weekly window: the resets inside the range counted, their figures added, and the newest reset at or
   *  before the range's end, which a reset of the lead week can be, the dash where only a use ended. */
  figures: Record<string, { counted: number; inRange: AllowanceFigure; last: AllowanceFigure }>;
  resets: number;
  termEndedAt: string | null;
  unpriced: boolean;
  /** The uses among `claims`. */
  used: number;
}

/** A figure's value to rank and add by, none for a dash. */
const valueOf = (figure: AllowanceFigure | undefined): number | null => (figure === undefined || figure.kind === 'none' || figure.kind === 'unpriced' ? null : figure.share);

/** One row of the Analytics card per account over `range`: its 7-day resets in the range counted, each weekly window's
 *  last and range figures, the term-ended mark of its last reset where one is, sorted with an unpriced account last,
 *  then most left first on the range's 7-day figure in the chosen week, then the Fable figure, then the id. */
export function allowanceRows(answer: UnusedAllowanceAnswer, costs: SubscriptionCosts | null, week: WeekPosition, range: { fromMs: number; toMs: number }): AllowanceRow[] {
  const rows = answer.accounts.map((account): AllowanceRow => {
    const figures: AllowanceRow['figures'] = {};
    const claims: AllowanceRow['claims'] = [];
    let termEndedAt: string | null = null;
    let sevenDay: AllowanceReset[] = [];
    for (const w of allowanceWindows(account)) {
      // A claim row falls in the range by its drop, since a use in a week still open carries that week's reset, which
      // lies past the range's end.
      const used = w.claims.filter((r) => Date.parse(r.observedAt) >= range.fromMs && Date.parse(r.observedAt) <= range.toMs);
      claims.push(...used.map((reset) => ({ reset, window: w.window })));
      const ended = w.resets.filter((r) => Date.parse(r.resetAt) <= range.toMs);
      // The route answers a use whatever natural resets the span holds, so an account before its first recorded weekly
      // reset can hold a use alone, which the range's figure keeps.
      if (ended.length === 0 && used.length === 0) continue;
      const last = ended.length === 0 ? null : ended.reduce((newest, r) => (Date.parse(r.resetAt) > Date.parse(newest.resetAt) ? r : newest));
      const inside = ended.filter((r) => Date.parse(r.resetAt) >= range.fromMs);
      const summed = [...inside, ...used];
      const noReset: AllowanceFigure = { kind: 'none', text: '-', title: NO_RESET_IN_RANGE };
      // An empty sum would read 0%, a week used whole, so a range with no reset draws the dash.
      const inRange = summed.length === 0 ? noReset : summedFigure(summed, w.window, account, costs, week);
      figures[w.window] = { counted: inside.length, inRange, last: last === null ? noReset : allowanceFigure(last.shareLeft, w.window, account, costs, week, last) };
      if (termEndedAt === null && last?.source === 'term_ended') termEndedAt = last.resetAt;
      if (w.window === 'seven_day') sevenDay = last === null ? summed : [...summed, last];
    }
    // Read off each drawn 7-day reset's own pair, so the fleet's note names every account the sum leaves out.
    const pairs = sevenDay.length === 0 ? [account] : sevenDay.map((r) => pairOf(account, r));
    const unpriced = pairs.some((pair) => capacityWeight(pair, 'seven_day', costs) === null);
    claims.sort((a, b) => Date.parse(b.reset.observedAt) - Date.parse(a.reset.observedAt));
    return { accountId: account.accountId, claims, figures, resets: figures.seven_day?.counted ?? 0, termEndedAt, unpriced, used: usesOf(claims.map((c) => c.reset)) };
  });
  const lead = (r: AllowanceRow, window: string) => valueOf(r.figures[window]?.inRange) ?? Number.NEGATIVE_INFINITY;
  // A dash ranks under every figure, so an equal pair of dashes compares as a tie rather than as NaN.
  const by = (a: number, b: number) => (a === b ? 0 : b - a);
  return rows.sort(
    (a, b) =>
      Number(a.unpriced) - Number(b.unpriced) ||
      by(lead(a, 'seven_day'), lead(b, 'seven_day')) ||
      by(lead(a, 'seven_day_fable'), lead(b, 'seven_day_fable')) ||
      byteOrder(a.accountId, b.accountId),
  );
}

/** A fleet figure's words, and the hover of a dash that says why it stands. */
export type FleetFigure = { text: string; title: string | null };

/** The two fleet figures: the priced accounts' figures in the chosen week added, with the unpriced accounts named, and
 *  where the week's `scale` is null, every figure a dash, the note naming the week the cost model does not size. A
 *  dash where no account counts a reset of the window in the range carries the rows' hover for it. */
export function fleetFigures(rows: AllowanceRow[], week: WeekPosition, scale: number | null): { fable: FleetFigure; note: string; sevenDay: FleetFigure } {
  const sum = (window: string): FleetFigure => {
    const figures = rows.map((r) => r.figures[window]?.inRange);
    const values = figures.map(valueOf).filter((v): v is number => v !== null);
    if (values.length > 0) return { text: weekShort(values.reduce((a, b) => a + b, 0), week), title: null };
    // A range figure reads `none` only where its row counts no reset in the range, and a row with no entry counts none.
    return { text: '-', title: figures.every((f) => f === undefined || f.kind === 'none') ? NO_RESET_IN_RANGE : null };
  };
  // The rows' flags read each account's own pair, which would count accounts as priced over a column of dashes.
  if (scale === null) return { fable: sum('seven_day_fable'), note: `${weekUnpricedTitle(week)}.`, sevenDay: sum('seven_day') };
  const priced = rows.filter((r) => !r.unpriced).length;
  const unpriced = rows
    .filter((r) => r.unpriced)
    .map((r) => r.accountId)
    .sort();
  return { fable: sum('seven_day_fable'), note: `${plural(priced, 'account')} priced${unpriced.length === 0 ? '' : `, ${unpriced.join(', ')} left out unpriced`}`, sevenDay: sum('seven_day') };
}
