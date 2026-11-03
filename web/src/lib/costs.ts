import type { Attempt, ModelTiers, SubscriptionCosts, TypeMultiplier, WindowFactors } from '@/api/types';
import type { WeekReading } from '@/lib/analytics';
import { has, lookup } from '@/lib/counts';
import { formatIn, listWords, undatedModel } from '@/lib/format';
import { planLabel } from '@/lib/quota';
import { DAY, HOUR, MINUTE } from '@/lib/time';
import { WEEK_TIER, type WeekPosition } from '@/lib/unit';
import { WINDOW_LABEL } from '@/lib/windows';

/**
 * The share rule of the subscription cost model, the console's one copy of what the ledger prices: a token costs the
 * model's weight times its type's multiplier in units, and a share is those units over the tier's capacity on the
 * window, over the peak band's capacity factor inside the band and a model week's divisor.
 */

export type CostWindow = 'five_hour' | 'seven_day';
/** The two windows the cost model sizes, as the cost views draw them: each window's key, the key of its peak capacity
 *  factor and its label. */
export const COST_WINDOWS: readonly { factor: keyof WindowFactors; key: CostWindow; label: string }[] = [
  { factor: 'fiveHour', key: 'five_hour', label: WINDOW_LABEL['5h'] },
  { factor: 'sevenDay', key: 'seven_day', label: WINDOW_LABEL['7d'] },
];
const factorKey = (window: CostWindow): keyof WindowFactors => COST_WINDOWS.find((w) => w.key === window)?.factor ?? 'sevenDay';
export type TokenType = keyof TypeMultiplier;
export interface Usage { cacheRead: number; cacheWrite1h: number; cacheWrite5m: number; input: number; output: number }

const TOKEN_TYPES: TokenType[] = ['cacheRead', 'cacheWrite1h', 'cacheWrite5m', 'input', 'output'];

export const anchorTier = (costs: SubscriptionCosts): string => costs.unit.tier;

/** The tier's capacity row as stored, then with `default_` taken off or put on; undefined for a tier the
 *  model lacks. */
export const tierRow = (costs: SubscriptionCosts, tier: string): SubscriptionCosts['capacity'][string] | undefined => {
  const bare = tier.startsWith('default_') ? tier.slice('default_'.length) : null;
  const key = [tier, bare, `default_${tier}`].find((k): k is string => k !== null && has(costs.capacity, k));
  return key === undefined ? undefined : costs.capacity[key];
};

export const tierCapacity = (costs: SubscriptionCosts, tier: string, window: CostWindow): number | null => tierRow(costs, tier)?.[factorKey(window)] ?? null;
/** The tiers the cost model sizes on either window, in the file's order. */
export const sizedTiers = (costs: SubscriptionCosts): string[] => Object.keys(costs.capacity).filter((tier) => COST_WINDOWS.some((w) => tierCapacity(costs, tier, w.key) !== null));

/** The shared window `window` prices on and the model-week divisor, as the ledger reads it; null for no plan window. */
export const pricedWindow = (costs: SubscriptionCosts, window: string): { divisor: number; priced: CostWindow } | null => {
  if (window === 'five_hour' || window === 'seven_day') return { divisor: 1, priced: window };
  if (has(costs.modelWindows, window)) {
    const row = costs.modelWindows[window];
    return row.of === 'seven_day' ? { divisor: row.capacityFactor, priced: 'seven_day' } : null;
  }
  // Any other week prices as the 7-day share once, as the ledger's window claims allow.
  return window.startsWith('seven_day') ? { divisor: 1, priced: 'seven_day' } : null;
};

/** The regimes' names in the file's order. */
export const regimeNames = (costs: SubscriptionCosts): string[] => Object.keys(costs.regimes);

// The map is the console's: the cost model file keeps its keys and tokens, and a key or token the map lacks reads raw,
// so a regime the file adds later draws with no code change.
const REGIME_WORD: Readonly<Record<string, string>> = { interactive: 'Interactive' };
const CLIENT_NAME: Readonly<Record<string, string>> = { 'claude-desktop': 'Claude Desktop', 'claude-desktop-3p': 'Claude Desktop', 'claude-vscode': 'Claude VSCode', cli: 'Claude Code CLI' };
const BASE_WORD = 'Headless';
const BASE_CLIENTS = 'Claude Code CLI headless mode, the Agent SDK, Claude in Slack, etc.';

/** The headline word of a regime, the raw key for one the map lacks, and "Headless" for the base (null). */
export const regimeWord = (name: string | null): string => (name === null ? BASE_WORD : (lookup(REGIME_WORD, name) ?? name));

/** The clients a regime prices: its entrypoints' client names in alphabetical order, each named once, a token the map
 *  lacks read raw. The base's fixed list for null. The order compares letters, so a raw lowercase token sorts among
 *  the capitalized names rather than after them all. */
export const regimeClients = (costs: SubscriptionCosts, name: string | null): string =>
  name === null ? BASE_CLIENTS : [...new Set((costs.regimes[name]?.entrypoints ?? []).map((t) => (lookup(CLIENT_NAME, t) ?? t)))].sort((a, b) => a.localeCompare(b, 'en')).join(', ');

/** "Interactive (Claude Code CLI, Claude Desktop, Claude VSCode)": a headline row's and the caption's text. */
export const regimeTitle = (costs: SubscriptionCosts, name: string | null): string => `${regimeWord(name)} (${regimeClients(costs, name)})`;

/** The Peak band card's sentence after the week word. A regime at a 5-hour factor of 1 while the base's is below 1
 *  holds its window whole inside the band, so the band applies to the base and to the rest alone. */
export const bandSentence = (costs: SubscriptionCosts): string => {
  const base = costs.peak.capacityFactor.fiveHour;
  const spared = regimeNames(costs).filter((name) => costs.regimes[name].peakCapacityFactor.fiveHour === 1 && base !== 1);
  if (spared.length === 0) return 'A request completing inside the band spends more of its 5-hour window.';
  const applies = [BASE_WORD, ...regimeNames(costs).filter((name) => !spared.includes(name)).map(regimeWord)];
  return `The band applies to ${listWords(applies)} traffic only: a request completing inside it spends more of its 5-hour window. ${listWords(spared.map(regimeWord))} sessions hold the full window at every hour.`;
};

/** The regime listing the entrypoint, null for null or an entrypoint no regime lists. */
export const regimeOf = (costs: SubscriptionCosts, entrypoint: string | null): string | null =>
  entrypoint === null ? null : (regimeNames(costs).find((name) => costs.regimes[name].entrypoints.includes(entrypoint)) ?? null);

/** The model as the named regime prices it: its capacity rows over the base's, a tier it does not name keeping the
 *  base row, and its band factors in place of the base's. The base itself for null or an unknown name. */
export const withRegime = (costs: SubscriptionCosts, name: string | null): SubscriptionCosts => {
  if (name === null || !has(costs.regimes, name)) return costs;
  const regime = costs.regimes[name];
  return { ...costs, capacity: { ...costs.capacity, ...regime.capacity }, peak: { ...costs.peak, capacityFactor: regime.peakCapacityFactor } };
};

/** `withRegime` of the regime listing the entrypoint: one derived model every reader prices through. */
export const underRegime = (costs: SubscriptionCosts, entrypoint: string | null): SubscriptionCosts => withRegime(costs, regimeOf(costs, entrypoint));

/** The model's row key: its id as sent, else its undated name, since the price rows carry dated ids. */
const keyOf = (costs: SubscriptionCosts, model: string): string | null => (has(costs.modelWeight, model) ? model : has(costs.modelWeight, undatedModel(model)) ? undatedModel(model) : null);

/** The model's own weight on `window`: its override on the window, else its row's weight, else null. */
export const weightOf = (costs: SubscriptionCosts, model: string, window: string): number | null => {
  const key = keyOf(costs, model);
  if (key === null) return null;
  const byWindow = costs.overrides[key]?.weightByWindow;
  return lookup(byWindow ?? {}, window) ?? costs.modelWeight[key];
};

/** The model's tier: its `model_tier` entry, else its undated id's, else the id's segment after `claude-` when
 *  `tier_order` names it. Named apart from routingLog's `tierOf`, the engine's account tier, which the test entry
 *  star-exports beside this module. */
export const modelTierOf = (costs: ModelTiers, model: string): string | null => {
  const undated = undatedModel(model);
  if (has(costs.modelTier, model)) return costs.modelTier[model];
  if (has(costs.modelTier, undated)) return costs.modelTier[undated];
  const named = undated.startsWith('claude-') ? undated.slice('claude-'.length).split('-')[0] : '';
  return costs.tierOrder.includes(named) ? named : null;
};

/** The model weeks a model carries: `seven_day_fable` for a Fable model, by the family its undated id names. */
export const modelWeeks = (costs: SubscriptionCosts, model: string): string[] =>
  Object.keys(costs.modelWindows).filter((key) => costs.modelWindows[key].of === 'seven_day' && undatedModel(model).startsWith(`claude-${key.slice('seven_day_'.length)}`));

/** The windows the model has a weight on, the two shared windows first and then its model weeks: an override's weight
 *  on a window prices that window even under a null shared weight. */
export const weightedWindows = (costs: SubscriptionCosts, model: string): string[] => ['five_hour', 'seven_day', ...modelWeeks(costs, model)].filter((w) => weightOf(costs, model, w) !== null);

/** Whether any window the model can be priced on carries a weight for it. */
export const isWeighted = (costs: SubscriptionCosts, model: string): boolean => weightedWindows(costs, model).length > 0;

const multipliersOf = (costs: SubscriptionCosts, model: string): TypeMultiplier => {
  const key = keyOf(costs, model);
  return { ...costs.typeMultiplier, ...(key === null ? {} : costs.overrides[key]?.typeMultiplier ?? {}) };
};

// The band readers below read a zone's clock a few thousand times per render, and building a formatter costs far more
// than using one, so each zone's is built once.
const formatters = new Map<string, Intl.DateTimeFormat>();
const formatterFor = (zone: string): Intl.DateTimeFormat => {
  let f = formatters.get(zone);
  if (f === undefined) {
    f = new Intl.DateTimeFormat('en-US', { day: '2-digit', hour: 'numeric', hourCycle: 'h23', minute: 'numeric', month: '2-digit', timeZone: zone, weekday: 'short', year: 'numeric' });
    formatters.set(zone, f);
  }
  return f;
};
/** The local date, weekday and minute of the day of `at` in `zone`; throws for a zone the browser does not know. */
export const partsIn = (at: number, zone: string): { date: string; minute: number; weekday: string } => {
  const p: Partial<Record<Intl.DateTimeFormatPartTypes, string>> = {};
  for (const part of formatterFor(zone).formatToParts(at)) p[part.type] = part.value;
  return { date: `${p.year}-${p.month}-${p.day}`, minute: Number(p.hour) * 60 + Number(p.minute), weekday: p.weekday ?? '' };
};
/** Whether this browser's time zone data holds `zone`, which the router's bundled database may know and it may not. */
const readable = (zone: string): boolean => {
  if (zone === '') return false;
  try {
    partsIn(0, zone);
    return true;
  } catch {
    return false;
  }
};

/** Whether `at` falls inside the peak band's hours in its zone, on a weekday when the band covers weekdays alone; null
 *  when this browser cannot read the zone, which leaves the band's state unknown. */
export const inBand = (costs: SubscriptionCosts, at: number): boolean | null => {
  const { fromHour, toHour, weekdays, zone } = costs.peak;
  if (!readable(zone)) return null;
  const parts = partsIn(at, zone);
  const hour = Math.floor(parts.minute / 60);
  const weekend = ['Sat', 'Sun'].includes(parts.weekday);
  return (!weekdays || !weekend) && fromHour <= hour && hour < toHour;
};

const factorOf = (costs: SubscriptionCosts, window: string, inside: boolean): number => {
  const w = pricedWindow(costs, window);
  if (w === null || !inside) return 1;
  return 1 / costs.peak.capacityFactor[factorKey(w.priced)];
};

/** The weight the band puts on a charge landing on `window` at `at`: the reciprocal of the priced window's capacity
 *  factor inside the band, else 1; null when the band's state at `at` cannot be read. */
export const bandFactor = (costs: SubscriptionCosts, window: string, at: number): number | null => {
  const inside = inBand(costs, at);
  return inside === null ? null : factorOf(costs, window, inside);
};

/** Per million tokens of each type, the share of the tier's window out of band, or null for an unpriced model or
 *  tier. */
export const ratesOf = (costs: SubscriptionCosts, window: string, tier: string, model: string): Record<TokenType, number> | null => {
  const w = pricedWindow(costs, window);
  const capacity = w === null ? null : tierCapacity(costs, tier, w.priced);
  const weight = weightOf(costs, model, window);
  if (w === null || weight === null || capacity === null) return null;
  const m = multipliersOf(costs, model);
  const per = (k: TokenType) => (weight * m[k] * 1_000_000) / capacity / w.divisor;
  return { cacheRead: per('cacheRead'), cacheWrite1h: per('cacheWrite1h'), cacheWrite5m: per('cacheWrite5m'), input: per('input'), output: per('output') };
};

/** The share of the tier's own window `usage` spends for a request completing inside the band or outside it, or null
 *  where unpriced. */
export const shareIn = (costs: SubscriptionCosts, window: string, tier: string, model: string, usage: Usage, inside: boolean): number | null => {
  const rates = ratesOf(costs, window, tier, model);
  if (rates === null) return null;
  const units = TOKEN_TYPES.reduce((s, k) => s + (usage[k] * rates[k]) / 1_000_000, 0);
  return units * factorOf(costs, window, inside);
};

/** The share `usage` spends for a request completing at `at`; null where unpriced, and where the band's state at `at`
 *  cannot be read, since a share on an assumed state could read half what the ledger charges. */
export const shareOf = (costs: SubscriptionCosts, window: string, tier: string, model: string, usage: Usage, at: number): number | null => {
  const inside = inBand(costs, at);
  return inside === null ? null : shareIn(costs, window, tier, model, usage, inside);
};

/** The Pro-week figure: the share of the anchor tier's 7-day window, so the figure adds across accounts. Null where
 *  the model has no weight on the window or the band's state at `at` cannot be read. */
export const equivalentOf = (costs: SubscriptionCosts, model: string, usage: Usage, at: number): number | null => shareOf(costs, 'seven_day', anchorTier(costs), model, usage, at);

/** The chosen week's figure per Pro-week figure: the anchor tier's 7-day capacity over the chosen tier's in the
 *  revision `costs` is (the one in force at the read), 1 under Pro with no table, null where either side is null. */
export const weekScale = (costs: SubscriptionCosts | null, week: WeekPosition): number | null => {
  if (costs === null) return week === 'pro' ? 1 : null;
  const anchor = tierCapacity(costs, anchorTier(costs), 'seven_day');
  const chosen = tierCapacity(costs, WEEK_TIER[week], 'seven_day');
  return anchor === null || chosen === null ? null : anchor / chosen;
};

/** The dash's hover and the fleet's note where the chosen week's tier has no 7-day capacity to read a figure in. */
export const weekUnpricedTitle = (week: WeekPosition): string => `Unpriced: the cost model holds no 7-day capacity for the ${planLabel(WEEK_TIER[week])} week`;

/** A read's failure as a surface draws it, which a retry may clear, so it never reads unpriced. */
export const failed = (e: string | null): WeekReading['failure'] => (e === null ? null : { text: e, unpriced: false });

/** A page's week reading from its settings read: `error` is that read's failure. A table that loaded and sizes no week
 *  for the chosen tier will not size one on a later read either, so its figures draw the unpriced dash, not the
 *  skeleton that waits for the table. */
export const weekReadingOf = (costs: SubscriptionCosts | null, error: string | null, week: WeekPosition): WeekReading => {
  const scale = weekScale(costs, week);
  if (scale !== null) return { failure: null, scale, week };
  if (costs === null) return { failure: failed(error), scale, week };
  return { failure: { text: weekUnpricedTitle(week), unpriced: true }, scale, week };
};

/** The account tiers a model week applies to: the cost model sizes the Fable week for every tier, and Pro has no
 *  Fable access, so the hint and the cost reference draw the week for these tiers alone. Each tier is listed in both
 *  spellings a profile reports, as `planLabel` maps them, so the match stays exact. */
export const MODEL_WEEK_TIERS: Record<string, readonly string[]> = { seven_day_fable: ['claude_max_20x', 'claude_max_5x', 'default_claude_max_20x', 'default_claude_max_5x'] };

/** The entry in force at `at` (an instant or an ISO string): the latest `effectiveDate` at or before it, else the
 *  first, since a blank history reads worse than the earliest known table. Null for an empty list. */
export const revisionAt = <T extends { effective: string; effectiveDate: string | null }>(revisions: T[], at: string | number): T | null => {
  const atMs = typeof at === 'number' ? at : Date.parse(at);
  let found: T | null = null;
  let foundMs = -Infinity;
  for (const r of revisions) {
    const ms = r.effectiveDate === null ? Number.NaN : Date.parse(r.effectiveDate);
    if (ms <= atMs && ms >= foundMs) [found, foundMs] = [r, ms];
  }
  return found ?? revisions[0] ?? null;
};

/** Who paid for an attempt: the subscription's window, its overage, a fast answer's usage credits, an API key, or a
 *  usage-based seat's dollars. */
export type Funding = 'api_key' | 'fast' | 'overage' | 'plan' | 'usage_based';

/** Who paid for an attempt, in the ledger's order: the account's kind first (an API-key account's answer is billed in
 *  dollars whatever its speed or overage flag), then the usage-based class (a seat has no plan window to fund either),
 *  then a fast answer, then overage, else the subscription's window. A request with no attempt is billed in dollars
 *  (its cost is zero). */
export const fundingOf = (attempt: Pick<Attempt, 'accountKind' | 'overageInUse' | 'response' | 'usageBased'> | null): Funding => {
  if (attempt === null || attempt.accountKind === 'api_key') return 'api_key';
  if (attempt.usageBased) return 'usage_based';
  if (attempt.response.speed === 'fast') return 'fast';
  if (attempt.overageInUse === true) return 'overage';
  return 'plan';
};

export type NativeUnit = 'equivalent' | 'usd';
/** The unit a funding pays in: a subscription window as a share of the chosen week, everything else in dollars. */
export const nativeOf = (funding: Funding): NativeUnit => (funding === 'plan' ? 'equivalent' : 'usd');

/** Minutes of a day in the viewing zone. */
export interface BandRun { from: number; to: number }
/** A day of the week in the viewing zone: its date, its label ("Wed 16 Sep") and the band's runs on it. */
export interface BandDay { date: string; label: string; runs: BandRun[]; today: boolean }
/** The band's interval on one band day, in UTC milliseconds. */
export interface BandSpan { end: number; start: number }
/** The peak zone, which this browser cannot read. */
export interface Unresolved { unresolved: string }

const WEEKDAYS = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];
// Calendar dates are `YYYY-MM-DD`, stepped and counted as UTC days, which have no clock changes.
const dayStart = (date: string): number => Date.parse(`${date}T00:00:00Z`);
const addDays = (date: string, days: number): string => new Date(dayStart(date) + days * DAY).toISOString().slice(0, 10);
const daysBetween = (from: string, to: string): number => Math.round((dayStart(to) - dayStart(from)) / DAY);
const weekdayOf = (date: string): string => WEEKDAYS[(new Date(dayStart(date)).getUTCDay() + 6) % 7];
const dayLabel = (date: string): string => {
  const noon = `${date}T12:00:00Z`;
  const part = (o: Intl.DateTimeFormatOptions) => formatIn({ ...o, timeZone: 'UTC' }, noon);
  return `${part({ weekday: 'short' })} ${part({ day: 'numeric' })} ${part({ month: 'short' })}`;
};
export const hhmm = (minutes: number) => `${String(Math.floor(minutes / 60)).padStart(2, '0')}:${String(minutes % 60).padStart(2, '0')}`;

/** What `zone`'s clock reads at `at`, an instant on the minute, as the UTC instant that reads the same. */
const wallIn = (at: number, zone: string): number => {
  const p = partsIn(at, zone);
  return dayStart(p.date) + p.minute * MINUTE;
};
/** The zone's offset from UTC at `at`, in milliseconds. */
const offsetIn = (at: number, zone: string): number => {
  const minute = Math.floor(at / MINUTE) * MINUTE;
  return wallIn(minute, zone) - minute;
};

/** The first whole minute past `lo` at which `reached` holds, which it does not at `lo` and does at `hi`. */
const firstMinute = (lo: number, hi: number, reached: (at: number) => boolean): number => {
  while (hi - lo > MINUTE) {
    const mid = lo + Math.floor((hi - lo) / (2 * MINUTE)) * MINUTE;
    if (reached(mid)) hi = mid;
    else lo = mid;
  }
  return hi;
};

/** The first instant at which `zone`'s clock reads `wall` or later. A zone changes its clocks at most once within a day
 *  of `wall`, so the instant is `wall` read at the offset a day before or the one a day after, the earlier where both
 *  read it (an hour the clocks repeat), or the change itself where the clocks skip `wall`. */
const firstReading = (wall: number, zone: string): number => {
  const early = wall - offsetIn(wall - DAY, zone);
  const late = wall - offsetIn(wall + DAY, zone);
  const exact = [early, late].filter((t) => wallIn(t, zone) === wall);
  if (exact.length > 0) return Math.min(...exact);
  // Skipped: the clock reads before `wall` at `late` and past it at `early`, and changes on a whole minute between.
  return firstMinute(late, early, (at) => wallIn(at, zone) >= wall);
};

const spanOn = (peak: SubscriptionCosts['peak'], date: string): BandSpan | null => {
  if (peak.weekdays && ['Sat', 'Sun'].includes(weekdayOf(date))) return null;
  const start = firstReading(dayStart(date) + peak.fromHour * HOUR, peak.zone);
  const end = firstReading(dayStart(date) + peak.toHour * HOUR, peak.zone);
  // A clock change can skip every hour of a short band.
  return start < end ? { end, start } : null;
};

/** The band on `date`, a calendar day in the peak zone: from the first instant that day whose hour there is
 *  `from_hour` or later to the first whose hour reaches `to_hour` (24 the next midnight), which is how the ledger reads
 *  the band through a clock change too; null on a day the band does not cover; the zone when this browser cannot read
 *  it. Every reader below builds on it. */
export const bandSpan = (costs: SubscriptionCosts, date: string): BandSpan | null | Unresolved => (readable(costs.peak.zone) ? spanOn(costs.peak, date) : { unresolved: costs.peak.zone });

/** The band days' spans from `first` to `last`, two band days, those that abut joined into one. */
const spansBetween = (peak: SubscriptionCosts['peak'], first: string, last: string): BandSpan[] => {
  const spans: BandSpan[] = [];
  for (let date = first; date <= last; date = addDays(date, 1)) {
    const span = spanOn(peak, date);
    if (span === null) continue;
    const open = spans[spans.length - 1];
    if (open !== undefined && open.end >= span.start) open.end = Math.max(open.end, span.end);
    else spans.push({ ...span });
  }
  return spans;
};

/** The span cut at each midnight of `zone` and at each of its clock changes, as the day and the minutes there: a run
 *  continues only across minutes that follow each other on the clock it is drawn on. */
const piecesIn = (span: BandSpan, zone: string): { date: string; run: BandRun }[] => {
  const pieces: { date: string; run: BandRun }[] = [];
  for (let t = span.start; t < span.end; ) {
    const p = partsIn(t, zone);
    const offset = offsetIn(t, zone);
    let stop = Math.min(span.end, t + (1440 - p.minute) * MINUTE);
    if (offsetIn(stop - MINUTE, zone) !== offset) stop = firstMinute(t, stop - MINUTE, (at) => offsetIn(at, zone) !== offset);
    pieces.push({ date: p.date, run: { from: p.minute, to: p.minute + (stop - t) / MINUTE } });
    t = stop;
  }
  return pieces;
};

/** The week drawn, Monday to Sunday in `zone`: the week holding `at` or the one holding the start of today's band as
 *  the caption and the change read it, whichever is later, so a day the band is off draws the coming week and a band
 *  that started on the day before still draws the week of `at`. Each day carries the runs of the band days that fall on
 *  it, and `today` marks the day holding `at`, which that week may not hold. */
export const bandWeek = (costs: SubscriptionCosts, at: number, zone: string): BandDay[] => {
  const now = partsIn(at, zone);
  const band = readable(costs.peak.zone) ? todayOf(costs.peak, at) : null;
  const start = band === null ? now : partsIn(band.span.start, zone);
  const shown = start.date > now.date ? start : now;
  const monday = addDays(shown.date, -WEEKDAYS.indexOf(shown.weekday));
  const dates = Array.from({ length: 7 }, (_, n) => addDays(monday, n));
  const runs = new Map(dates.map((date) => [date, [] as BandRun[]]));
  if (readable(costs.peak.zone)) {
    // One instant's dates in two zones lie at most two days apart, so the band days from two before Monday to two past
    // Sunday hold every run the week draws.
    for (let n = -2; n <= 8; n += 1) {
      const span = spanOn(costs.peak, addDays(monday, n));
      if (span !== null) for (const { date, run } of piecesIn(span, zone)) runs.get(date)?.push(run);
    }
  }
  return dates.map((date) => {
    const joined: BandRun[] = [];
    for (const run of (runs.get(date) ?? []).sort((a, b) => a.from - b.from)) {
      const last = joined[joined.length - 1];
      if (last !== undefined && last.to === run.from) last.to = run.to;
      else joined.push({ ...run });
    }
    return { date, label: dayLabel(date), runs: joined, today: date === now.date };
  });
};

/** "Los Angeles" for `America/Los_Angeles`: a zone's last segment, its underscores as spaces. */
export const zoneCity = (zone: string): string => (zone.split('/').pop() ?? zone).replace(/_/g, ' ');

const everyHour = (peak: SubscriptionCosts['peak']): boolean => peak.fromHour === 0 && peak.toHour === 24 && !peak.weekdays;

/** A band day's hours as `zone` reads them: the minutes its span starts and ends at there, whether it ends on the next
 *  day there, the date its start falls on, and the days that lies from the band day. */
interface Here { end: number; nextDay: boolean; shift: number; start: number; startDate: string }
const hereOf = (span: BandSpan, date: string, zone: string): Here => {
  const s = partsIn(span.start, zone);
  const e = partsIn(span.end, zone);
  return { end: e.minute, nextDay: e.date !== s.date && e.minute > 0, shift: daysBetween(date, s.date), start: s.minute, startDate: s.date };
};
const sameHours = (a: Here, b: Here): boolean => a.start === b.start && a.end === b.end && a.nextDay === b.nextDay && a.shift === b.shift;
const hoursText = (h: Here, clock: (minutes: number) => string): string => `${clock(h.start)} to ${clock(h.end)}${h.nextDay ? ' the next day' : ''}`;

/** The band day of `at`, the peak zone's date then, with its span; on a day the band does not cover, the next band day,
 *  whose hours stand for today's. */
const todayOf = (peak: SubscriptionCosts['peak'], at: number): { date: string; span: BandSpan } | null => {
  let date = partsIn(at, peak.zone).date;
  for (let n = 0; n < 7; n += 1) {
    const span = spanOn(peak, date);
    if (span !== null) return { date, span };
    date = addDays(date, 1);
  }
  return null;
};

/** A band day, the peak zone's date, with its span. */
interface PeakDay { date: string; span: BandSpan }
/** The first band day of the week that holds `day`, Monday to Sunday in the peak zone. A band of every day has no week
 *  to read the ends of, so it answers `day`. */
const firstBandDayOf = (peak: SubscriptionCosts['peak'], day: PeakDay): PeakDay => {
  if (!peak.weekdays) return day;
  const monday = addDays(day.date, -WEEKDAYS.indexOf(weekdayOf(day.date)));
  for (let date = monday; date < day.date; date = addDays(date, 1)) {
    const span = spanOn(peak, date);
    if (span !== null) return { date, span };
  }
  return day;
};
/** The last band day of the week that holds `day`, Monday to Sunday in the peak zone. A band of every day has no week
 *  to read the ends of, so it answers `day`. */
const lastBandDayOf = (peak: SubscriptionCosts['peak'], day: PeakDay): PeakDay => {
  if (!peak.weekdays) return day;
  const sunday = addDays(day.date, 6 - WEEKDAYS.indexOf(weekdayOf(day.date)));
  for (let date = sunday; date > day.date; date = addDays(date, -1)) {
    const span = spanOn(peak, date);
    if (span !== null) return { date, span };
  }
  return day;
};

/** "Mon to Fri, 14:00 to 20:00 here (Europe/Berlin). In Los Angeles: Mon to Fri, 05:00 to 11:00." or the one-zone form,
 *  the hours here read from the band day of `at`. */
export const bandCaption = (costs: SubscriptionCosts, at: number, zone: string, clock: (minutes: number) => string): string => {
  const { fromHour, toHour, weekdays, zone: own } = costs.peak;
  const wholeDays = fromHour === 0 && toHour === 24;
  const defined = `${weekdays ? 'Mon to Fri' : 'Every day'}, ${wholeDays ? 'all day' : `${clock(fromHour * 60)} to ${clock(toHour * 60)}`}`;
  // A zone this browser cannot read has no hours here, so the band reads in its own zone alone.
  if (zone === own || !readable(own)) return `${defined} in ${zoneCity(own)} (${own}), the zone the cost model names.`;
  if (everyHour(costs.peak)) return `Every day, at every hour here (${zone}). In ${zoneCity(own)}: ${defined}.`;
  const today = todayOf(costs.peak, at);
  if (today === null) return `Never here (${zone}). In ${zoneCity(own)}: ${defined}.`;
  const here = hereOf(today.span, today.date, zone);
  if (wholeDays) {
    // Whole days read all day only where the week they cover starts and ends at midnight here. Elsewhere the week's two
    // ends are what the now line and the change beside the caption move against, so the caption names them: the first
    // band day's start and the last band day's end, which differ from today's across a clock change inside the week.
    const first = firstBandDayOf(costs.peak, today);
    const last = lastBandDayOf(costs.peak, today);
    const start = hereOf(first.span, first.date, zone);
    const end = hereOf(last.span, last.date, zone);
    if (start.start !== 0 || end.end !== 0) {
      return `${WEEKDAYS[(7 + start.shift) % 7]} ${clock(start.start)} to ${partsIn(last.span.end, zone).weekday} ${clock(end.end)} here (${zone}). In ${zoneCity(own)}: ${defined}.`;
    }
  }
  // The days follow the band's weekdays as they fall here: in Auckland a Monday's band starts on a Tuesday.
  const days = weekdays ? `${WEEKDAYS[(7 + here.shift) % 7]} to ${WEEKDAYS[(11 + here.shift) % 7]}` : 'Every day';
  return `${days}, ${wholeDays ? 'all day' : hoursText(here, clock)} here (${zone}). In ${zoneCity(own)}: ${defined}.`;
};

/** The band's state at `at`, or the peak zone when this browser cannot read it, which leaves the state unknown. */
export type BandNow = { at: number | null; inside: boolean } | Unresolved;

/** Whether `at` is inside the band and the next instant the state flips, within eight days; `at` null when it never
 *  does (`from_hour: 0`, `to_hour: 24`, `weekdays: false` keeps the band on at every hour). */
export const bandNow = (costs: SubscriptionCosts, at: number): BandNow => {
  const { zone } = costs.peak;
  if (!readable(zone)) return { unresolved: zone };
  const today = partsIn(at, zone).date;
  const horizon = at + 8 * DAY;
  const spans = spansBetween(costs.peak, addDays(today, -1), addDays(today, 9));
  const current = spans.find((s) => s.start <= at && at < s.end);
  if (current !== undefined) return { at: current.end > horizon ? null : current.end, inside: true };
  const next = spans.find((s) => s.start > at);
  return { at: next === undefined || next.start > horizon ? null : next.start, inside: false };
};

/** The band state no instant reaches, with the reason a control names for it, from `bandNow`'s answer; null when both
 *  are in reach. A zone the browser reads and `from_hour` below `to_hour` flip the band within eight days, so only the
 *  band on at every hour leaves a state out of reach; a zone it cannot read leaves both to the viewer. */
export const unreachableBand = (now: BandNow): { inside: boolean; title: string } | null => ('unresolved' in now || now.at !== null || !now.inside ? null : { inside: false, title: 'The band is on at every hour.' });

/** A coming change of the band's local hours: its first and last band day, `to` null for one that lasts. */
export interface LocalChange { from: string; lasting: boolean; moved: boolean; text: string; to: string | null }

// Two zones change their clocks on dates months apart (Cairo's spring change falls seven weeks after Los Angeles'), so
// a stretch reads this far before it counts as lasting.
const STRETCH_DAYS = 26 * 7;
const SEARCH_DAYS = 10 * 7;

/** The next change of the band's local hours opening within ten weeks of `at`'s band day; null when none. Each band day
 *  after today's is read against today's hours. The change runs from the first band day that differs until the first
 *  later band day whose hours are today's again, read up to 26 weeks on; one still open then is `lasting`. `from` and
 *  `to` label its first and last band day as each falls here, `text` the first one's hours, and `moved` says the band's
 *  own zone changed its clocks between today's band and the change rather than the viewing one. */
export const nextLocalChange = (costs: SubscriptionCosts, at: number, zone: string, clock: (minutes: number) => string = hhmm): LocalChange | null => {
  const { peak } = costs;
  if (!readable(peak.zone) || everyHour(peak)) return null;
  const today = todayOf(peak, at);
  if (today === null) return null;
  const usual = hereOf(today.span, today.date, zone);
  const atDate = partsIn(at, peak.zone).date;
  for (let n = daysBetween(atDate, today.date) + 1; n <= SEARCH_DAYS; n += 1) {
    const date = addDays(atDate, n);
    const span = spanOn(peak, date);
    if (span === null) continue;
    const first = hereOf(span, date, zone);
    if (sameHours(first, usual)) continue;
    let last = first;
    let back = false;
    for (let m = 1; m <= STRETCH_DAYS && !back; m += 1) {
      const later = addDays(date, m);
      const s = spanOn(peak, later);
      if (s === null) continue;
      const hours = hereOf(s, later, zone);
      if (sameHours(hours, usual)) back = true;
      else last = hours;
    }
    return {
      from: dayLabel(first.startDate),
      lasting: !back,
      // Against today's band rather than `at`, which on a day the band is off can lie before a change today's band
      // follows.
      moved: offsetIn(span.start, peak.zone) !== offsetIn(today.span.start, peak.zone),
      text: hoursText(first, clock),
      to: back ? dayLabel(last.startDate) : null,
    };
  }
  return null;
};

/** The numeric segments after the family in the undated id: `claude-opus-4-8` reads 4 and 8. */
const versionKey = (model: string): number[] =>
  undatedModel(model)
    .split('-')
    .slice(2)
    .map((v) => Number.parseInt(v, 10) || 0);

/** The newer version first, the numeric segments compared from the left, then the id. */
function newestFirst(a: string, b: string): number {
  const x = versionKey(a);
  const y = versionKey(b);
  for (let i = 0; i < Math.max(x.length, y.length); i += 1) {
    const step = (y[i] ?? 0) - (x[i] ?? 0);
    if (step !== 0) return step;
  }
  return a.localeCompare(b);
}

/** The model's tier when the order names it. A tier the order lacks has no place to list at, so it reads as none. */
const orderedTierOf = (tiers: ModelTiers, model: string): string | null => {
  const tier = modelTierOf(tiers, model);
  return tier !== null && tiers.tierOrder.includes(tier) ? tier : null;
};

/** Two models in the cost model's tier order, a model of no tier last, then by version descending and id: the one rule
 *  the cost reference and the routing log list models by, so a new or renamed tier is a file edit. */
export const compareByModelTier = (tiers: ModelTiers, a: string, b: string): number => {
  const rank = (model: string) => {
    const tier = orderedTierOf(tiers, model);
    return tier === null ? tiers.tierOrder.length : tiers.tierOrder.indexOf(tier);
  };
  return rank(a) - rank(b) || newestFirst(a, b);
};

/** The models in the cost model's tier order, a model of no tier last, inside a tier by version descending then id. */
export const tierOrder = (models: string[], costs: ModelTiers): string[] => [...models].sort((a, b) => compareByModelTier(costs, a, b));

/** Each tier group's newest model, and the older models a model table folds under a row of their own. */
export const newestAndOlder = (groups: { models: string[] }[]): { newest: string[]; older: string[] } => ({ newest: groups.map((g) => g.models[0]), older: groups.flatMap((g) => g.models.slice(1)) });

/** The same models grouped by tier in that order: each group's first model is the shown row, the rest fold. An id of
 *  no tier is a group of its own, so it never folds as an older model of another. */
export const tierGroups = (models: string[], costs: ModelTiers): { models: string[]; tier: string | null }[] => {
  const groups: { models: string[]; tier: string | null }[] = [];
  for (const model of tierOrder(models, costs)) {
    const tier = orderedTierOf(costs, model);
    const last = groups[groups.length - 1];
    if (tier !== null && last?.tier === tier) last.models.push(model);
    else groups.push({ models: [model], tier });
  }
  return groups;
};

/** The tier whose window is smallest among the sized tiers (Pro on the shipped file), the one the multiples
 *  are against. */
export const smallestTier = (costs: SubscriptionCosts, window: CostWindow): string | null => {
  let smallest: string | null = null;
  let least = Infinity;
  for (const tier of Object.keys(costs.capacity)) {
    const capacity = tierCapacity(costs, tier, window);
    if (capacity !== null && capacity > 0 && capacity < least) [smallest, least] = [tier, capacity];
  }
  return smallest;
};

/** The tier's capacity on `window` over the smallest tier's; null where either is unsized. */
export const windowMultiple = (costs: SubscriptionCosts, tier: string, window: CostWindow): number | null => {
  const smallest = smallestTier(costs, window);
  const capacity = tierCapacity(costs, tier, window);
  const base = smallest === null ? null : tierCapacity(costs, smallest, window);
  return capacity === null || base === null ? null : capacity / base;
};

/** `(seven_day x f7) / (five_hour x f5)`, each factor its window's `peak.capacityFactor` inside the band and 1
 *  outside; null unsized. */
export const fullWindowsAWeek = (costs: SubscriptionCosts, tier: string, inside: boolean): number | null => {
  const five = tierCapacity(costs, tier, 'five_hour');
  const seven = tierCapacity(costs, tier, 'seven_day');
  if (five === null || seven === null || five <= 0) return null;
  const { fiveHour, sevenDay } = inside ? costs.peak.capacityFactor : { fiveHour: 1, sevenDay: 1 };
  return (seven * sevenDay) / (five * fiveHour);
};
