export type TimeFormat = '12h' | '24h';

let timeFormat: TimeFormat = '24h';
export const setTimeFormat = (f: TimeFormat) => {
  timeFormat = f;
};
export const getTimeFormat = () => timeFormat;

const compact = new Intl.NumberFormat('en-US', { notation: 'compact', maximumFractionDigits: 1 });
const compact2 = new Intl.NumberFormat('en-US', { notation: 'compact', maximumFractionDigits: 2 });
const plain = new Intl.NumberFormat('en-US');
const usd = new Intl.NumberFormat('en-US', { style: 'currency', currency: 'USD' });
const usd4 = new Intl.NumberFormat('en-US', { style: 'currency', currency: 'USD', minimumFractionDigits: 4, maximumFractionDigits: 4 });

export const fmtInt = (n: number) => plain.format(Math.round(n));
export const fmtCompact = (n: number) => (Math.abs(n) < 1000 ? plain.format(Math.round(n)) : compact.format(n));
export const fmtCompact2 = (n: number) => (Math.abs(n) < 1000 ? plain.format(Math.round(n)) : compact2.format(n));
// `n + 0` folds a negative zero onto a positive one: an empty sum of f64 costs reaches the wire as -0.0 and
// Intl would print its sign as "-$0.00".
export const fmtUsd = (n: number, v = n + 0) => (Math.abs(v) > 0 && Math.abs(v) < 0.01 ? usd4.format(v) : usd.format(v));
export const fmtPct = (ratio: number, digits = 0) => `${(ratio * 100).toFixed(digits)}%`;
export const fmtMs = (ms: number | null) => (ms === null ? '-' : ms >= 1000 ? `${(ms / 1000).toFixed(ms >= 10_000 ? 0 : 1)} s` : `${Math.round(ms)} ms`);
export const fmtCredits = (credits: number) => usd.format(credits / 100);

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

export function fmtAbsolute(isoTime: string | null): string {
  if (!isoTime) return '-';
  const d = new Date(isoTime);
  return d.toLocaleString('en-US', {
    day: '2-digit',
    hour: '2-digit',
    hour12: timeFormat === '12h',
    minute: '2-digit',
    month: 'short',
    second: '2-digit',
    year: 'numeric',
  });
}

export function fmtTime(isoTime: string): string {
  return new Date(isoTime).toLocaleTimeString('en-US', { hour: '2-digit', hour12: timeFormat === '12h', minute: '2-digit', second: '2-digit' });
}

export function fmtShortTime(isoTime: string): string {
  return new Date(isoTime).toLocaleTimeString('en-US', { hour: '2-digit', hour12: timeFormat === '12h', minute: '2-digit' });
}

export function fmtDate(isoTime: string): string {
  return new Date(isoTime).toLocaleDateString('en-US', { day: '2-digit', month: 'short', year: 'numeric' });
}

export function fmtDayShort(ms: number): string {
  return new Date(ms).toLocaleDateString('en-US', { day: '2-digit', month: 'short' });
}

export function fmtHourTick(ms: number): string {
  return new Date(ms).toLocaleTimeString('en-US', { hour: '2-digit', hour12: timeFormat === '12h', minute: '2-digit' });
}

export const shortId = (id: string, n = 8) => (id.length > n ? `${id.slice(0, n)}…` : id);
export const tokensTotal = (u: { cacheRead: number; cacheWrite1h: number; cacheWrite5m: number; input: number; output: number }) =>
  u.input + u.cacheRead + u.cacheWrite5m + u.cacheWrite1h + u.output;
