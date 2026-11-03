import type { Placement } from '@/api/types';
import { lookup } from '@/lib/counts';
import { WEEK_LABEL, type WeekPosition } from '@/lib/unit';
import { type ByWindow, WINDOW_LABEL, windowKeyId } from '@/lib/windows';

export type TimeFormat = '12h' | '24h';

let timeFormat: TimeFormat = '24h';
export const setTimeFormat = (f: TimeFormat) => {
  timeFormat = f;
};
export const getTimeFormat = () => timeFormat;
// `hour12: false` prints the first hour of a day as 24 in en-US (`24:05`, which reads as the day before); the h23
// cycle prints it as 00.
const hourCycle = () => (timeFormat === '12h' ? 'h12' : 'h23');

const compact = new Intl.NumberFormat('en-US', { notation: 'compact', maximumFractionDigits: 1 });
const compact2 = new Intl.NumberFormat('en-US', { notation: 'compact', maximumFractionDigits: 2 });
const plain = new Intl.NumberFormat('en-US');
const usd = new Intl.NumberFormat('en-US', { style: 'currency', currency: 'USD' });
const usd4 = new Intl.NumberFormat('en-US', { style: 'currency', currency: 'USD', minimumFractionDigits: 4, maximumFractionDigits: 4 });

export const fmtInt = (n: number) => plain.format(Math.round(n));
export const fmtCompact = (n: number) => (Math.abs(n) < 1000 ? plain.format(Math.round(n)) : compact.format(n));
export const fmtCompact2 = (n: number) => (Math.abs(n) < 1000 ? plain.format(Math.round(n)) : compact2.format(n));
/** What a cut dropped, from the counts the ledger took as it read; a rendered body measures neither side. */
export const bytesDropped = (seen: number, stored: number) => Math.max(0, seen - stored);
// `n + 0` folds a negative zero onto a positive one: an empty sum of f64 costs reaches the wire as -0.0 and
// Intl would print its sign as "-$0.00".
export const fmtUsd = (n: number, v = n + 0) => (Math.abs(v) > 0 && Math.abs(v) < 0.01 ? usd4.format(v) : usd.format(v));
/**
 * The decimals every tick of one dollar axis carries, read once from the largest figure the axis draws either side of
 * zero, so the axis never mixes $0.0040 with $0.01 and no two of its ticks read the same: whole dollars from 10, cents
 * from 10 cents, a tenth of a cent from a cent, four decimals from a tenth of a cent, one more per decade below that up
 * to eight, and cents for an axis at zero.
 */
export function usdTickDigits(values: Iterable<number>): number {
  let max = 0;
  for (const v of values) max = Math.max(max, Math.abs(v));
  if (max === 0) return 2;
  return max >= 10 ? 0 : max >= 0.1 ? 2 : max >= 0.01 ? 3 : max >= 0.001 ? 4 : Math.min(8, Math.floor(-Math.log10(max)) + 2);
}
/** A dollar axis tick at the axis's `usdTickDigits`; a figure that rounds to zero carries no sign. */
export const fmtUsdTick = (v: number, digits: number) => {
  const text = Math.abs(v).toFixed(digits);
  return `${v < 0 && Number(text) !== 0 ? '-' : ''}$${text}`;
};
/** A dated model id's label: the catalog lists `claude-haiku-4-5-20251001`, the operator reads `claude-haiku-4-5`. */
export const undatedModel = (id: string) => id.replace(/-\d{8}$/, '');
/**
 * A model id as a narrow column shows it: every Anthropic id starts `claude-`, so the prefix drops with the date and
 * the family and version remain (`haiku-4-5`); the cell's title carries the id as sent.
 */
export const shortModel = (id: string) => undatedModel(id).replace(/^claude-/, '');
export const fmtPct = (ratio: number, digits = 0) => `${(ratio * 100).toFixed(digits)}%`;
/**
 * A figure already in percent to the whole point, a dash without one, and the sign of a lead where `signed` asks. The
 * sign follows the rounded figure, since `toFixed` prints -0.4 as "-0" and a lead that rounds to zero is no lead.
 */
export const pct = (v: number | null | undefined, signed = false) => {
  if (v === null || v === undefined) return '-';
  const n = Math.round(v);
  return `${signed && n > 0 ? '+' : ''}${n === 0 ? 0 : n}%`;
};
/** A share of a window as a percent to three significant digits, trailing zeros dropped: 8.41%, 0.219%, 12.5%. */
export const fmtShare = (ratio: number) => `${Number((ratio * 100).toPrecision(3))}%`;
/** A share with its window named ("0.42% of 5-hour", "0.12% of 7-day Fable"), a window the pages do not know under its key. */
export const shareLabel = (windowKey: string, share: number): string => `${fmtShare(share)} of ${lookup(WINDOW_LABEL, windowKeyId(windowKey) ?? '') ?? windowKey}`;
const WINDOW_SHORT: ByWindow<string> = { '5h': '5h', '7d': '7d', '7d_fable': '7-day Fable' };
/**
 * A share with its window's short word ("0.42% 5h", "0.12% 7d", "0.12% 7-day Fable"), for a cell too narrow for the
 * full form. A window the pages do not know reads under its key, and `shareLabel` stays the full form.
 */
export const shareShort = (windowKey: string, share: number): string => `${fmtShare(share)} ${lookup(WINDOW_SHORT, windowKeyId(windowKey) ?? '') ?? windowKey}`;
export const WEEK_WORD: Record<WeekPosition, string> = { max: 'a Max week', pro: 'a Pro week', ultra: 'an Ultra week' };
/** "0.11% of an Ultra week", "1.11% of a Pro week", zero "0%". */
export const fmtWeek = (share: number, week: WeekPosition): string => (share === 0 ? '0%' : `${fmtShare(share)} of ${WEEK_WORD[week]}`);
/** The short form for a cell: "0.11% Ultra week". */
export const weekShort = (share: number, week: WeekPosition): string => `${fmtShare(share)} ${WEEK_LABEL[week]}`;
export const fmtMs = (ms: number | null) => (ms === null ? '-' : ms >= 1000 ? `${(ms / 1000).toFixed(ms >= 10_000 ? 0 : 1)} s` : `${Math.round(ms)} ms`);
export const fmtCredits = (credits: number, exponent = 2) => usd.format(credits / 10 ** exponent);
const minorDecimal = new Intl.NumberFormat('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 });
/** A spend amount in minor units, printed in its own currency at its exponent, since a seat may be billed in any. */
export const fmtMinor = (minor: number, currency: string, exponent: number) => {
  const amount = minor / 10 ** exponent;
  try {
    return new Intl.NumberFormat('en-US', { style: 'currency', currency }).format(amount);
  } catch (e) {
    // The router passes the provider's code on unchecked, and a throw in render would blank the view drawing it.
    if (!(e instanceof RangeError)) throw e;
    return currency === '' ? minorDecimal.format(amount) : `${minorDecimal.format(amount)} ${currency}`;
  }
};
export const fmtMiB = (bytes: number) => {
  const mib = bytes / 1_048_576;
  return `${Number.isInteger(mib) ? mib : mib.toFixed(1)} MiB`;
};
/** A byte count in the largest binary unit it reaches, each figure printed as `fmtMiB` prints its own. */
export const fmtBytes = (bytes: number) => {
  const units = ['KiB', 'MiB', 'GiB'];
  let unit = 'B';
  let figure = bytes;
  for (let i = 0; i < units.length && figure >= 1024; i += 1) {
    figure /= 1024;
    unit = units[i];
  }
  return `${Number.isInteger(figure) ? figure : figure.toFixed(1)} ${unit}`;
};

export function fmtDuration(ms: number): string {
  const abs = Math.abs(ms);
  const m = Math.floor(abs / 60_000);
  const h = Math.floor(m / 60);
  const d = Math.floor(h / 24);
  if (abs < 60_000) return `${Math.max(1, Math.round(abs / 1000))}s`;
  if (h < 1) return `${m}m`;
  if (d < 1) return m % 60 === 0 ? `${h}h` : `${h}h ${m % 60}m`;
  return h % 24 === 0 ? `${d}d` : `${d}d ${h % 24}h`;
}

/**
 * Whole hours from one hour on and whole minutes below it, floored like `fmtDuration` so a ceiling never reads longer
 * than it is: `44h` for 44.1 hours, `46m` for 46 minutes.
 */
export function fmtHours(ms: number): string {
  const m = Math.floor(Math.abs(ms) / 60_000);
  return m < 60 ? `${m}m` : `${Math.floor(m / 60)}h`;
}

/** "in 2h 14m" / "3m ago" style label; `now` is injected so lists tick together. */
export function fmtRelative(isoTime: string | null, now: number): string {
  if (!isoTime) return '-';
  const delta = Date.parse(isoTime) - now;
  if (Math.abs(delta) < 15_000) return 'now';
  return delta > 0 ? `in ${fmtDuration(delta)}` : `${fmtDuration(delta)} ago`;
}

/** "ends in 12 days", "ends in 3 hours", "ended 2 days ago": whole units counted down, so a part day is not a day. */
export function fmtEndsIn(at: string, now: number): string {
  const delta = Date.parse(at) - now;
  const abs = Math.abs(delta);
  const [n, unit]: [number, string] = abs >= 86_400_000 ? [Math.floor(abs / 86_400_000), 'day'] : abs >= 3_600_000 ? [Math.floor(abs / 3_600_000), 'hour'] : [Math.max(1, Math.floor(abs / 60_000)), 'minute'];
  const span = `${n} ${unit}${n === 1 ? '' : 's'}`;
  return delta > 0 ? `ends in ${span}` : `ended ${span} ago`;
}

/** A `datetime-local` value in the browser's zone, which is how the field reads and writes an instant. */
export const toLocalInput = (ms: number) => {
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
};

export const upperFirst = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

export function fmtAbsolute(isoTime: string | null): string {
  if (!isoTime) return '-';
  const d = new Date(isoTime);
  return d.toLocaleString('en-US', {
    day: '2-digit',
    hour: '2-digit',
    hourCycle: hourCycle(),
    minute: '2-digit',
    month: 'short',
    second: '2-digit',
    year: 'numeric',
  });
}

/** `fmtAbsolute` to the minute: "Sep 14, 2026, 09:31" under the 24-hour format, "09:31 AM" under the 12-hour one. */
export function fmtMinute(isoTime: string | null): string {
  if (!isoTime) return '-';
  return new Date(isoTime).toLocaleString('en-US', { day: '2-digit', hour: '2-digit', hourCycle: hourCycle(), minute: '2-digit', month: 'short', year: 'numeric' });
}

/** A budget period ends on a UTC day boundary, so its end drops the seconds and the year `fmtAbsolute` prints. */
export function fmtPeriodEnd(isoTime: string): string {
  return new Date(isoTime).toLocaleString('en-US', { day: '2-digit', hour: '2-digit', hourCycle: hourCycle(), minute: '2-digit', month: 'short' });
}

export function fmtTime(isoTime: string): string {
  return new Date(isoTime).toLocaleTimeString('en-US', { hour: '2-digit', hourCycle: hourCycle(), minute: '2-digit', second: '2-digit' });
}

export function fmtShortTime(isoTime: string): string {
  return new Date(isoTime).toLocaleTimeString('en-US', { hour: '2-digit', hourCycle: hourCycle(), minute: '2-digit' });
}

/** The hour alone in the viewer's cycle for a short timeline cell: "06" under 24h, "6 AM" or "12 PM" under 12h. */
export function fmtShortHour(isoTime: string): string {
  const hour = new Date(isoTime).getHours();
  if (timeFormat === '24h') return String(hour).padStart(2, '0');
  // Spelled here rather than by the formatter, which some ICU builds separate from the period with a narrow no-break
  // space, so the text reads the same in every browser.
  return `${hour % 12 === 0 ? 12 : hour % 12} ${hour < 12 ? 'AM' : 'PM'}`;
}

/**
 * The narrowest timeline cell that holds a label, per hour cycle: `short` the short hour ("06", "12 AM") and `full` the
 * full label ("Sep 13" under 24h, "12:00 AM" under 12h). A cell has no inline padding, so each figure is its label's
 * width in the sub's mono face, measured in the mock pass's Chromium and rounded up: "06" 12.6 px, "12 AM" 31.5 px and
 * "12:00 AM" 50.4 px. Each `full` keeps a margin past its label, so two labels side by side stay apart: the 24-hour
 * 40 px stands 2.2 px over the 37.8 px of "Sep 13", and the 12-hour takes the same 2.2 px over "12:00 AM", 52.6 px
 * rounded up to 53.
 */
export const CELL_MIN_PX: Record<TimeFormat, { full: number; short: number }> = {
  '12h': { full: 53, short: 32 },
  '24h': { full: 40, short: 13 },
};

/**
 * How many cells the 5-hour timeline draws across `widthPx`: the largest divisor of twelve whose cell holds the cycle's
 * short hour. A divisor keeps every cell a whole multiple of six hours, so each label falls on 00, 06, 12 or 18.
 */
export function cellsFit(widthPx: number, cycle: TimeFormat): number {
  return [12, 6, 4, 3, 2].find((count) => widthPx / count >= CELL_MIN_PX[cycle].short) ?? 1;
}

/**
 * An instant within a day of `now` reads as its clock time alone (`15:42`, a retry at the next midnight `00:00`). One
 * further off carries its date (`Sep 18 08:42`).
 */
export function instant(ms: number, now: number): string {
  const at = new Date(ms).toISOString();
  if (Math.abs(ms - now) < 86_400_000) return fmtShortTime(at);
  return `${new Date(ms).toLocaleDateString('en-US', { day: 'numeric', month: 'short' })} ${fmtShortTime(at)}`;
}

/**
 * An instant with its date always written ("Sep 16 13:11"): the Meter's window ends are read against each other across
 * days, so a window that ended today carries its date as `instant` would drop it.
 */
export function fmtDated(ms: number): string {
  return `${new Date(ms).toLocaleDateString('en-US', { day: 'numeric', month: 'short' })} ${fmtShortTime(new Date(ms).toISOString())}`;
}

/**
 * The clock time within a day of `now`, else the date before it ("Sep 14 09:12:05"), the seconds kept and the threshold
 * `instant`'s, so the two agree on which rows carry a date.
 */
export function fmtDatedTime(isoTime: string, now: number): string {
  const ms = Date.parse(isoTime);
  if (Math.abs(ms - now) < 86_400_000) return fmtTime(isoTime);
  return `${new Date(ms).toLocaleDateString('en-US', { day: 'numeric', month: 'short' })} ${fmtTime(isoTime)}`;
}

/** A custom span's words: the times alone inside one local calendar day, the date before each time across days. */
export function spanText(fromMs: number, toMs: number): string {
  const from = new Date(fromMs);
  const to = new Date(toMs);
  const sameDay = from.getFullYear() === to.getFullYear() && from.getMonth() === to.getMonth() && from.getDate() === to.getDate();
  const day = (d: Date) => d.toLocaleDateString('en-US', { day: 'numeric', month: 'short' });
  const time = (d: Date) => fmtShortTime(d.toISOString());
  return sameDay ? `${time(from)} to ${time(to)}` : `${day(from)} ${time(from)} to ${day(to)} ${time(to)}`;
}

export function fmtDate(isoTime: string): string {
  return new Date(isoTime).toLocaleDateString('en-US', { day: '2-digit', month: 'short', year: 'numeric' });
}

/** The day in `timeZone`, or in the browser's zone when none is named. */
export function fmtDayShort(ms: number, timeZone?: string): string {
  return new Date(ms).toLocaleDateString('en-US', { day: '2-digit', month: 'short', timeZone });
}

/**
 * A day tick in `timeZone` (the browser's when none is named): the date, and the time beside it where the instant
 * is not midnight there, so a bucket boundary that is not local midnight says when it falls.
 */
export function fmtDayTick(ms: number, timeZone?: string): string {
  const day = fmtDayShort(ms, timeZone);
  // Midnight is read on the 24-hour clock whatever the time format, so it compares as one string.
  const clock = new Date(ms).toLocaleTimeString('en-US', { hour: '2-digit', hourCycle: 'h23', minute: '2-digit', timeZone });
  return clock === '00:00' ? day : `${day} ${new Date(ms).toLocaleTimeString('en-US', { hour: '2-digit', hourCycle: hourCycle(), minute: '2-digit', timeZone })}`;
}

export function fmtHourTick(ms: number): string {
  return new Date(ms).toLocaleTimeString('en-US', { hour: '2-digit', hourCycle: hourCycle(), minute: '2-digit' });
}

/**
 * The analytics label of a key generation no live key holds, built here for the route's groups and the fold's alike
 * so both paths print one shape, the creation minute in the viewer's zone. Generation 0, the rows the ledger left
 * unstamped, and a generation that is no instant carry no creation time; `toISOString` would throw on the second.
 */
export const earlierHolderLabel = (id: string, generation: number): string => {
  const created = new Date(generation);
  if (generation === 0 || Number.isNaN(created.getTime())) return `${id} (deleted)`;
  return `${id} (deleted, created ${fmtMinute(created.toISOString())})`;
};

/** A client names itself in full, which is wider than any table cell affords; the whole string rides the title. */
export const shortAgent = (ua: string) => shortId(ua, 28);
export const shortId = (id: string, n = 8) => (id.length > n ? `${id.slice(0, n)}…` : id);

/** The characters a session or conversation id shows where the whole id does not fit a label. */
export const SESSION_CHARS = 13;

/** `fork of 1a2b3c4d-5e6f… at 3`, the base cut at the session id's width, without the depth where none is known. */
export function forkLabel(from: string, depth: number | null): string {
  return `fork of ${shortId(from, SESSION_CHARS)}${depth === null ? '' : ` at ${depth}`}`;
}

/**
 * A placement's verdict against the response's cache read, the rule the placements route counts hits by: a
 * `content` placement wants the whole prefix it was promised, the others any read at all. Null without a read.
 */
export function placementHit(placement: Placement, cacheRead: number | null): boolean | null {
  if (cacheRead === null) return null;
  return placement.rung === 'content' ? cacheRead >= placement.prefixTokens : cacheRead > 0;
}

export const tokensTotal = (u: { cacheRead: number; cacheWrite1h: number; cacheWrite5m: number; input: number; output: number }) =>
  u.input + u.cacheRead + u.cacheWrite5m + u.cacheWrite1h + u.output;
