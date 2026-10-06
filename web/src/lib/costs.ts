import type { SubscriptionCosts, TypeMultiplier } from '@/api/types';
import { undatedModel } from '@/lib/format';

/**
 * The share rule of the subscription cost model, the console's one copy of what the ledger prices: a token costs the
 * model's weight times its type's multiplier in units, and a share is those units over the tier's capacity on the
 * window, over the peak band's capacity factor inside the band and a model week's divisor.
 */

export type CostWindow = 'five_hour' | 'seven_day';
export type TokenType = keyof TypeMultiplier;
export interface Usage { cacheRead: number; cacheWrite1h: number; cacheWrite5m: number; input: number; output: number }

const TOKEN_TYPES: TokenType[] = ['cacheRead', 'cacheWrite1h', 'cacheWrite5m', 'input', 'output'];

// A local copy rather than `lib/analytics`'s, which reads `tierRow` from this module: the two would import each other.
const own = (map: object, key: string): boolean => Object.prototype.hasOwnProperty.call(map, key);

export const anchorTier = (costs: SubscriptionCosts): string => costs.unit.definedBy.tier;

/** The tier's capacity row as stored, then with `default_` taken off or put on; undefined for a tier the model lacks. */
export const tierRow = (costs: SubscriptionCosts, tier: string): SubscriptionCosts['capacity'][string] | undefined => {
  const bare = tier.startsWith('default_') ? tier.slice('default_'.length) : null;
  const key = [tier, bare, `default_${tier}`].find((k): k is string => k !== null && own(costs.capacity, k));
  return key === undefined ? undefined : costs.capacity[key];
};

export const tierCapacity = (costs: SubscriptionCosts, tier: string, window: CostWindow): number | null => tierRow(costs, tier)?.[window === 'five_hour' ? 'fiveHour' : 'sevenDay'] ?? null;

/** The shared window `window` prices on and the model-week divisor, as the ledger reads it; null for no plan window. */
export const pricedWindow = (costs: SubscriptionCosts, window: string): { divisor: number; priced: CostWindow } | null => {
  if (window === 'five_hour' || window === 'seven_day') return { divisor: 1, priced: window };
  if (own(costs.modelWindows, window)) {
    const row = costs.modelWindows[window];
    return row.of === 'seven_day' ? { divisor: row.capacityFactor, priced: 'seven_day' } : null;
  }
  // Any other week prices as the 7-day share once, as the ledger's window claims allow.
  return window.startsWith('seven_day') ? { divisor: 1, priced: 'seven_day' } : null;
};

/** The model's row key: its id as sent, else its undated name, since the price rows carry dated ids. */
const keyOf = (costs: SubscriptionCosts, model: string): string | null => (own(costs.modelWeight, model) ? model : own(costs.modelWeight, undatedModel(model)) ? undatedModel(model) : null);

export const weightOf = (costs: SubscriptionCosts, model: string, window: string): number | null => {
  const key = keyOf(costs, model);
  if (key === null) return null;
  const byWindow = costs.overrides[key]?.weightByWindow;
  return byWindow !== undefined && own(byWindow, window) ? byWindow[window] : costs.modelWeight[key];
};

/** Whether the model has a weight on either shared window: an override's weight on a window prices that window even
 *  under a null shared weight. */
export const isWeighted = (costs: SubscriptionCosts, model: string): boolean => (['five_hour', 'seven_day'] as const).some((w) => weightOf(costs, model, w) !== null);

const multipliersOf = (costs: SubscriptionCosts, model: string): TypeMultiplier => {
  const key = keyOf(costs, model);
  return { ...costs.typeMultiplier, ...(key === null ? {} : costs.overrides[key]?.typeMultiplier ?? {}) };
};

// The band helpers below sample thousands of instants per render, and building a formatter costs far more than using
// one, so each zone's is built once.
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
const partsIn = (at: number, zone: string): { date: string; minute: number; weekday: string } => {
  const p: Partial<Record<Intl.DateTimeFormatPartTypes, string>> = {};
  for (const part of formatterFor(zone).formatToParts(at)) p[part.type] = part.value;
  return { date: `${p.year}-${p.month}-${p.day}`, minute: Number(p.hour) * 60 + Number(p.minute), weekday: p.weekday ?? '' };
};

/** Whether `at` falls inside the peak band's hours in its zone, on a weekday when the band covers weekdays alone. */
export const inBand = (costs: SubscriptionCosts, at: number): boolean => {
  const { fromHour, toHour, weekdays, zone } = costs.peak;
  if (zone === '') return false;
  let parts: { minute: number; weekday: string };
  try {
    parts = partsIn(at, zone);
  } catch {
    // A zone the browser does not know prices out of band, as the ledger reads a zone it cannot resolve.
    return false;
  }
  const hour = Math.floor(parts.minute / 60);
  const weekend = ['Sat', 'Sun'].includes(parts.weekday);
  return (!weekdays || !weekend) && fromHour <= hour && hour < toHour;
};

/** The weight the band puts on a charge landing on `window` at `at`: the reciprocal of the priced window's capacity
 *  factor inside the band, else 1. */
export const bandFactor = (costs: SubscriptionCosts, window: string, at: number): number => {
  const w = pricedWindow(costs, window);
  if (w === null || !inBand(costs, at)) return 1;
  return 1 / costs.peak.capacityFactor[w.priced === 'five_hour' ? 'fiveHour' : 'sevenDay'];
};

/** Per million tokens of each type, the share of the tier's window out of band; null for an unpriced model or tier. */
export const ratesOf = (costs: SubscriptionCosts, window: string, tier: string, model: string): Record<TokenType, number> | null => {
  const w = pricedWindow(costs, window);
  const weight = weightOf(costs, model, window);
  const capacity = w === null ? null : tierCapacity(costs, tier, w.priced);
  if (w === null || weight === null || capacity === null) return null;
  const m = multipliersOf(costs, model);
  const per = (k: TokenType) => (weight * m[k] * 1_000_000) / capacity / w.divisor;
  return { cacheRead: per('cacheRead'), cacheWrite1h: per('cacheWrite1h'), cacheWrite5m: per('cacheWrite5m'), input: per('input'), output: per('output') };
};

/** The share of the tier's own window `usage` spends for a request completing at `at`; null where unpriced. */
export const shareOf = (costs: SubscriptionCosts, window: string, tier: string, model: string, usage: Usage, at: number): number | null => {
  const rates = ratesOf(costs, window, tier, model);
  if (rates === null) return null;
  const units = TOKEN_TYPES.reduce((s, k) => s + (usage[k] * rates[k]) / 1_000_000, 0);
  return units * bandFactor(costs, window, at);
};

/** Minutes of a day in the viewing zone. */
export interface BandRun { from: number; to: number }
/** A day of the week in the viewing zone: its date, its label ("Wed 16 Sep") and the band's runs on it. */
export interface BandDay { date: string; label: string; runs: BandRun[]; today: boolean }

// Every zone's offset is a whole number of quarter hours and the band turns on whole hours in its own zone, so samples
// on the quarter-hour grid land exactly on every edge in any viewing zone.
const STEP = 15 * 60_000;
const DAY = 86_400_000;
const WEEKDAYS = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];
const onGrid = (at: number) => Math.floor(at / STEP) * STEP;
const dayLabel = (date: string): string => {
  const d = new Date(`${date}T12:00:00Z`);
  const part = (o: Intl.DateTimeFormatOptions) => d.toLocaleDateString('en-US', { ...o, timeZone: 'UTC' });
  return `${part({ weekday: 'short' })} ${part({ day: 'numeric' })} ${part({ month: 'short' })}`;
};
const hhmm = (minutes: number) => `${String(Math.floor(minutes / 60)).padStart(2, '0')}:${String(minutes % 60).padStart(2, '0')}`;

type ScannedDay = BandDay & { starts: number[]; weekday: string };

/** The days from `from` to `to` in `zone`, each with the band's runs in its minutes and the instant each run's first
 *  sample falls on, sampled on the quarter-hour grid. */
const scanDays = (costs: SubscriptionCosts, from: number, to: number, zone: string): ScannedDay[] => {
  const days = new Map<string, ScannedDay>();
  for (let t = onGrid(from); t < to; t += STEP) {
    const p = partsIn(t, zone);
    let day = days.get(p.date);
    if (day === undefined) {
      day = { date: p.date, label: dayLabel(p.date), runs: [], starts: [], today: false, weekday: p.weekday };
      days.set(p.date, day);
    }
    if (!inBand(costs, t)) continue;
    const last = day.runs[day.runs.length - 1];
    // A run continues only across adjacent samples, so a clock change inside the band starts a new one.
    if (last !== undefined && last.to === p.minute) last.to = p.minute + 15;
    else {
      day.runs.push({ from: p.minute, to: p.minute + 15 });
      day.starts.push(t);
    }
  }
  return [...days.values()];
};

/** The days from eight before `at` to `after` past it in `zone`, and the index of the Monday of `at`'s week. */
const weeksAround = (costs: SubscriptionCosts, at: number, zone: string, after: number): { days: ScannedDay[]; monday: number; today: string } => {
  const today = partsIn(at, zone);
  // Eight days either side hold the whole week whatever the weekday, a clock change included.
  const days = scanDays(costs, at - 8 * DAY, at + after * DAY, zone);
  return { days, monday: days.findIndex((d) => d.date === today.date) - WEEKDAYS.indexOf(today.weekday), today: today.date };
};

/** The week holding `at`, Monday to Sunday in `zone`, the band's runs per day sampled every 15 minutes. */
export const bandWeek = (costs: SubscriptionCosts, at: number, zone: string): BandDay[] => {
  const { days, monday, today } = weeksAround(costs, at, zone, 8);
  return days.slice(monday, monday + 7).map(({ date, label, runs }) => ({ date, label, runs, today: date === today }));
};

/** "14:00 to 20:00", or "21:00 to 03:00 the next day", for the run starting at `start` as `zone` reads it. */
const runFrom = (costs: SubscriptionCosts, start: number, zone: string, clock: (minutes: number) => string): string => {
  let end = start;
  while (inBand(costs, end) && end < start + DAY) end += STEP;
  const s = partsIn(start, zone);
  const e = partsIn(end, zone);
  return `${clock(s.minute)} to ${clock(e.minute)}${e.date !== s.date && e.minute > 0 ? ' the next day' : ''}`;
};

/** The band's first run in the fortnight around `at` that starts on a day the band covers in its own zone, if any. */
const firstStart = (costs: SubscriptionCosts, at: number): number | null => {
  for (let t = onGrid(at - 7 * DAY); t < at + 7 * DAY; t += STEP) {
    if (!inBand(costs, t) || inBand(costs, t - STEP)) continue;
    if (costs.peak.weekdays && partsIn(t, costs.peak.zone).weekday !== 'Mon') continue;
    return t;
  }
  return null;
};

/** "Los Angeles" for `America/Los_Angeles`: a zone's last segment, its underscores as spaces. */
export const zoneCity = (zone: string): string => (zone.split('/').pop() ?? zone).replace(/_/g, ' ');

/** The zone's offset from UTC at `at`, in milliseconds, for an instant on the minute. */
const offsetIn = (at: number, zone: string): number => {
  const p = partsIn(at, zone);
  return Date.parse(`${p.date}T00:00:00Z`) + p.minute * 60_000 - (at - (at % 60_000));
};

/** "Mon to Fri, 14:00 to 20:00 here (Europe/Berlin). In Los Angeles: Mon to Fri, 05:00 to 11:00." or the one-zone form. */
export const bandCaption = (costs: SubscriptionCosts, at: number, zone: string, clock: (minutes: number) => string): string => {
  const { fromHour, toHour, weekdays, zone: own } = costs.peak;
  const defined = `${weekdays ? 'Mon to Fri' : 'Every day'}, ${clock(fromHour * 60)} to ${clock(toHour * 60)}`;
  if (zone === own) return `${defined} in ${zoneCity(own)} (${own}), the zone the cost model names.`;
  const start = firstStart(costs, at);
  // The days follow the band's Monday as it falls here: in Auckland it starts on a Tuesday.
  const first = start === null ? null : partsIn(start, zone).weekday;
  const days = first === null || !weekdays ? 'Every day' : `${first} to ${WEEKDAYS[(WEEKDAYS.indexOf(first) + 4) % 7]}`;
  const here = start === null ? (inBand(costs, at) ? 'Every day, at every hour' : 'Never') : `${days}, ${runFrom(costs, start, zone, clock)}`;
  return `${here} here (${zone}). In ${zoneCity(own)}: ${defined}.`;
};

/** Whether `at` is inside the band and the next instant the state flips, searched eight days ahead; `at` null when
 *  it never does (`from_hour: 0`, `to_hour: 24`, `weekdays: false` keeps the band on at every hour). */
export const bandNow = (costs: SubscriptionCosts, at: number): { at: number | null; inside: boolean } => {
  const inside = inBand(costs, at);
  for (let t = onGrid(at) + STEP; t <= at + 8 * DAY; t += STEP) if (inBand(costs, t) !== inside) return { at: t, inside };
  return { at: null, inside };
};

/** The band state no instant reaches, with the reason a control names for it, from `bandNow`'s answer; null when the
 *  band flips. A band on at every hour never reads outside; a zone the browser cannot resolve reads as out of band at
 *  every hour, so it never reads inside. */
export const unreachableBand = (now: { at: number | null; inside: boolean }): { inside: boolean; title: string } | null => {
  if (now.at !== null) return null;
  return now.inside ? { inside: false, title: 'The band is on at every hour.' } : { inside: true, title: 'The band is off at every hour, as this browser cannot resolve its zone.' };
};

/** The band's in-band intervals between two instants, `{ from, to }` pairs clipped to them, sampled every 15 minutes
 *  through `inBand`. */
export const bandRuns = (costs: SubscriptionCosts, fromMs: number, toMs: number): { from: number; to: number }[] => {
  const runs: { from: number; to: number }[] = [];
  let open: { from: number; to: number } | null = null;
  for (let t = onGrid(fromMs); t < toMs; t += STEP) {
    if (!inBand(costs, t)) {
      open = null;
      continue;
    }
    const to = Math.min(t + STEP, toMs);
    if (open === null) {
      open = { from: Math.max(t, fromMs), to };
      runs.push(open);
    } else open.to = to;
  }
  return runs;
};

/** The next change of the band's local hours starting within ten weeks; null when none. The change is the stretch of
 *  days from the first that differs from the same weekday this week until a day reads as this week's again, a day
 *  without a band continuing it. The stretch reads over a scan three weeks and a weekend longer than the search for its
 *  start: two zones change their clocks up to three weeks apart and the weekend after a gap continues its stretch, so
 *  a gap opening on the search's last day needs that much more scan to reach a day that reads as this week's. `from`
 *  and `to` label the stretch's first day and its last day with a band, `text` the first band run starting in it. The
 *  change is `lasting` when the stretch runs to the end of the longer scan, as one zone's clock change does until the
 *  other zone changes too, and `moved` says the band's own zone changed its clocks rather than the viewing one. */
export const nextLocalChange = (
  costs: SubscriptionCosts,
  at: number,
  zone: string,
  clock: (minutes: number) => string = hhmm,
): { from: string; lasting: boolean; moved: boolean; text: string; to: string } | null => {
  const longer = 3 * 7 + 2;
  const { days, monday } = weeksAround(costs, at, zone, 10 * 7 + 8 + longer);
  const usual = days.slice(monday, monday + 7).map((d) => JSON.stringify(d.runs));
  const differs = (i: number) => JSON.stringify(days[i].runs) !== usual[(i - monday) % 7];
  // The scan's last day is cut short, so it never counts as a change.
  const end = days.length - 1;
  for (let i = monday + 7; i < end - longer; i += 1) {
    if (!differs(i)) continue;
    // A weekend inside a shifted stretch has no band on either side, so it does not end the stretch.
    let j = i;
    while (j + 1 < end && (differs(j + 1) || days[j + 1].runs.length === 0)) j += 1;
    let last = j;
    while (last > i && days[last].runs.length === 0) last -= 1;
    // A run carried over midnight from the day before is no start of its own, so the first true start names the hours.
    const start = days.slice(i, j + 1).flatMap((d) => d.starts).find((t) => !inBand(costs, t - STEP)) ?? null;
    return {
      from: days[i].label,
      lasting: j === end - 1,
      moved: start !== null && offsetIn(start, costs.peak.zone) !== offsetIn(onGrid(at), costs.peak.zone),
      text: start === null ? 'no band' : runFrom(costs, start, zone, clock),
      to: days[last].label,
    };
  }
  return null;
};

const FAMILIES = ['fable', 'opus', 'sonnet', 'haiku'];
const familyKey = (model: string): { rank: number; version: number[] } => {
  const [, family = '', ...version] = undatedModel(model).split('-');
  const rank = FAMILIES.indexOf(family);
  return { rank: rank === -1 ? FAMILIES.length : rank, version: version.map((v) => Number.parseInt(v, 10) || 0) };
};

/** The models family by family, Fable, Opus, Sonnet, Haiku, then any other, each family's newest version first: the
 *  order the shipped files keep, so a file kept in another order lists the same. */
export const familyOrder = (models: string[]): string[] =>
  [...models].sort((a, b) => {
    const x = familyKey(a);
    const y = familyKey(b);
    if (x.rank !== y.rank) return x.rank - y.rank;
    for (let i = 0; i < Math.max(x.version.length, y.version.length); i += 1) {
      const step = (y.version[i] ?? 0) - (x.version[i] ?? 0);
      if (step !== 0) return step;
    }
    return a.localeCompare(b);
  });

/** The tier whose window is smallest among the sized tiers (Pro on the shipped file), the one the multiples are against. */
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
