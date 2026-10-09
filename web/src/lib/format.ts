import type { Placement } from '@/api/types';

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
/** A share of a window as a percent to three significant digits, trailing zeros dropped: 8.41%, 0.219%, 12.5%. */
export const fmtShare = (ratio: number) => `${Number((ratio * 100).toPrecision(3))}%`;
export const fmtMs = (ms: number | null) => (ms === null ? '-' : ms >= 1000 ? `${(ms / 1000).toFixed(ms >= 10_000 ? 0 : 1)} s` : `${Math.round(ms)} ms`);
export const fmtCredits = (credits: number) => usd.format(credits / 100);
export const fmtMiB = (bytes: number) => {
  const mib = bytes / 1_048_576;
  return `${Number.isInteger(mib) ? mib : mib.toFixed(1)} MiB`;
};

export function fmtDuration(ms: number): string {
  const abs = Math.abs(ms);
  const m = Math.floor(abs / 60_000);
  const h = Math.floor(m / 60);
  const d = Math.floor(h / 24);
  if (abs < 60_000) return `${Math.max(1, Math.round(abs / 1000))}s`;
  if (h < 1) return `${m}m`;
  if (d < 1) return `${h}h ${m % 60}m`;
  return `${d}d ${h % 24}h`;
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

export function fmtDate(isoTime: string): string {
  return new Date(isoTime).toLocaleDateString('en-US', { day: '2-digit', month: 'short', year: 'numeric' });
}

/** The day in `timeZone`, or in the browser's zone when none is named. */
export function fmtDayShort(ms: number, timeZone?: string): string {
  return new Date(ms).toLocaleDateString('en-US', { day: '2-digit', month: 'short', timeZone });
}

export function fmtHourTick(ms: number): string {
  return new Date(ms).toLocaleTimeString('en-US', { hour: '2-digit', hourCycle: hourCycle(), minute: '2-digit' });
}

/**
 * The analytics label of a key generation no live key holds, as the router renders it. Generation 0, the rows the
 * ledger left unstamped, and a generation that is no instant carry no creation time; `toISOString` would throw on
 * the second.
 */
export const earlierHolderLabel = (id: string, generation: number): string => {
  const created = new Date(generation);
  if (generation === 0 || Number.isNaN(created.getTime())) return `${id} (deleted)`;
  return `${id} (deleted, created ${created.toISOString().slice(0, 16).replace('T', ' ')} UTC)`;
};

/** A client names itself in full, which is wider than any table cell affords; the whole string rides the title. */
export const shortAgent = (ua: string) => (ua.length > 28 ? `${ua.slice(0, 28)}…` : ua);
export const shortId = (id: string, n = 8) => (id.length > n ? `${id.slice(0, n)}…` : id);

/** The characters a session or conversation id shows where the whole id does not fit a label. */
export const SESSION_CHARS = 13;

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
