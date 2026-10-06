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
const partsIn = (at: number, zone: string): { date: string; minute: number; weekday: string } => {
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
  return 1 / costs.peak.capacityFactor[w.priced === 'five_hour' ? 'fiveHour' : 'sevenDay'];
};

/** The weight the band puts on a charge landing on `window` at `at`: the reciprocal of the priced window's capacity
 *  factor inside the band, else 1; null when the band's state at `at` cannot be read. */
export const bandFactor = (costs: SubscriptionCosts, window: string, at: number): number | null => {
  const inside = inBand(costs, at);
  return inside === null ? null : factorOf(costs, window, inside);
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

/** The share of the tier's own window `usage` spends for a request completing inside the band or outside it; null
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

/** Minutes of a day in the viewing zone. */
export interface BandRun { from: number; to: number }
/** A day of the week in the viewing zone: its date, its label ("Wed 16 Sep") and the band's runs on it. */
export interface BandDay { date: string; label: string; runs: BandRun[]; today: boolean }
/** The band's interval on one band day, in UTC milliseconds. */
export interface BandSpan { end: number; start: number }
/** The peak zone, which this browser cannot read. */
export interface Unresolved { unresolved: string }

const MINUTE = 60_000;
const HOUR = 3_600_000;
const DAY = 86_400_000;
const WEEKDAYS = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];
// Calendar dates are `YYYY-MM-DD`, stepped and counted as UTC days, which have no clock changes.
const dayStart = (date: string): number => Date.parse(`${date}T00:00:00Z`);
const addDays = (date: string, days: number): string => new Date(dayStart(date) + days * DAY).toISOString().slice(0, 10);
const daysBetween = (from: string, to: string): number => Math.round((dayStart(to) - dayStart(from)) / DAY);
const weekdayOf = (date: string): string => WEEKDAYS[(new Date(dayStart(date)).getUTCDay() + 6) % 7];
const dayLabel = (date: string): string => {
  const d = new Date(`${date}T12:00:00Z`);
  const part = (o: Intl.DateTimeFormatOptions) => d.toLocaleDateString('en-US', { ...o, timeZone: 'UTC' });
  return `${part({ weekday: 'short' })} ${part({ day: 'numeric' })} ${part({ month: 'short' })}`;
};
const hhmm = (minutes: number) => `${String(Math.floor(minutes / 60)).padStart(2, '0')}:${String(minutes % 60).padStart(2, '0')}`;

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

/** The first instant at which `zone`'s clock reads `wall` or later. A zone changes its clocks at most once within a day
 *  of `wall`, so the instant is `wall` read at the offset a day before or the one a day after, the earlier where both
 *  read it (an hour the clocks repeat), or the change itself where the clocks skip `wall`. */
const firstReading = (wall: number, zone: string): number => {
  const early = wall - offsetIn(wall - DAY, zone);
  const late = wall - offsetIn(wall + DAY, zone);
  const exact = [early, late].filter((t) => wallIn(t, zone) === wall);
  if (exact.length > 0) return Math.min(...exact);
  // Skipped: the clock reads before `wall` at `late` and past it at `early`, and changes on a whole minute between.
  let [lo, hi] = [late, early];
  while (hi - lo > MINUTE) {
    const mid = lo + Math.floor((hi - lo) / (2 * MINUTE)) * MINUTE;
    if (wallIn(mid, zone) >= wall) hi = mid;
    else lo = mid;
  }
  return hi;
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
    if (offsetIn(stop - MINUTE, zone) !== offset) {
      let lo = t;
      let hi = stop - MINUTE;
      while (hi - lo > MINUTE) {
        const mid = lo + Math.floor((hi - lo) / (2 * MINUTE)) * MINUTE;
        if (offsetIn(mid, zone) === offset) lo = mid;
        else hi = mid;
      }
      stop = hi;
    }
    pieces.push({ date: p.date, run: { from: p.minute, to: p.minute + (stop - t) / MINUTE } });
    t = stop;
  }
  return pieces;
};

/** Today's week, Monday to Sunday in `zone`: the week holding the start of today's band as the caption and the change
 *  read it, the next band day's on a day the band is off. Each day carries the runs of the band days that fall on it,
 *  and `today` marks the day holding `at`, which that week may not hold. */
export const bandWeek = (costs: SubscriptionCosts, at: number, zone: string): BandDay[] => {
  const now = partsIn(at, zone);
  const band = readable(costs.peak.zone) ? todayOf(costs.peak, at) : null;
  const shown = band === null ? now : partsIn(band.span.start, zone);
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

/** "Mon to Fri, 14:00 to 20:00 here (Europe/Berlin). In Los Angeles: Mon to Fri, 05:00 to 11:00." or the one-zone form,
 *  the hours here read from the band day of `at`. */
export const bandCaption = (costs: SubscriptionCosts, at: number, zone: string, clock: (minutes: number) => string): string => {
  const { fromHour, toHour, weekdays, zone: own } = costs.peak;
  const defined = `${weekdays ? 'Mon to Fri' : 'Every day'}, ${clock(fromHour * 60)} to ${clock(toHour * 60)}`;
  // A zone this browser cannot read has no hours here, so the band reads in its own zone alone.
  if (zone === own || !readable(own)) return `${defined} in ${zoneCity(own)} (${own}), the zone the cost model names.`;
  if (everyHour(costs.peak)) return `Every day, at every hour here (${zone}). In ${zoneCity(own)}: ${defined}.`;
  const today = todayOf(costs.peak, at);
  if (today === null) return `Never here (${zone}). In ${zoneCity(own)}: ${defined}.`;
  const here = hereOf(today.span, today.date, zone);
  // The days follow the band's weekdays as they fall here: in Auckland a Monday's band starts on a Tuesday.
  const days = weekdays ? `${WEEKDAYS[(7 + here.shift) % 7]} to ${WEEKDAYS[(11 + here.shift) % 7]}` : 'Every day';
  return `${days}, ${hoursText(here, clock)} here (${zone}). In ${zoneCity(own)}: ${defined}.`;
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

/** The band's in-band intervals between two instants, `{ from, to }` pairs clipped to them; none for a zone this
 *  browser cannot read. */
export const bandRuns = (costs: SubscriptionCosts, fromMs: number, toMs: number): { from: number; to: number }[] => {
  const { zone } = costs.peak;
  if (!readable(zone) || toMs <= fromMs) return [];
  const spans = spansBetween(costs.peak, addDays(partsIn(fromMs, zone).date, -1), addDays(partsIn(toMs, zone).date, 1));
  return spans.filter((s) => s.end > fromMs && s.start < toMs).map((s) => ({ from: Math.max(s.start, fromMs), to: Math.min(s.end, toMs) }));
};

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
      // Against today's band rather than `at`, which on a day the band is off can lie before a change today's band follows.
      moved: offsetIn(span.start, peak.zone) !== offsetIn(today.span.start, peak.zone),
      text: hoursText(first, clock),
      to: back ? dayLabel(last.startDate) : null,
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
