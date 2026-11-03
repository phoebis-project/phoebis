import { DAY, HOUR, MINUTE } from '@/lib/time';

export { DAY, HOUR, MINUTE };

// Anchored once at load so every relative label counts down from the same instant.
export const NOW = Date.now();

export const iso = (ms: number) => new Date(ms).toISOString();
export const ago = (ms: number) => iso(NOW - ms);
export const ahead = (ms: number) => iso(NOW + ms);
export const hoursAgo = (h: number) => ago(h * HOUR);
export const daysAgo = (d: number) => ago(d * DAY);
export const startOfHour = (ms: number) => Math.floor(ms / HOUR) * HOUR;
/** The UTC calendar edges a budget's spend period runs between, as the ledger reads them. */
export const startOfDay = (at: number) => Math.floor(at / DAY) * DAY;
export const startOfMonth = (at: number) => { const d = new Date(at); return Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), 1); };
export const endOfMonth = (at: number) => { const d = new Date(at); return Date.UTC(d.getUTCFullYear(), d.getUTCMonth() + 1, 1); };

/**
 * The story's wall-clock time on its pinned day, 2026-09-16 at 14:30 UTC, as an offset from the instant the mock loads
 * at, so every story fact holds at whichever clock the mock is loaded at. Kept here so the modules `requests.ts`
 * imports can date a story time as it does.
 */
export const clock = (h: number, m: number, s = 0): number => NOW - ((14 - h) * HOUR + (30 - m) * MINUTE - s * 1_000);

/**
 * When nightly's spend cap was raised, the instant the story fixes for the start of its overage, which the accounts
 * seed reads before the day's requests date the overage's first head.
 */
export const NIGHTLY_CAP_RAISED = clock(11, 30, 12);
/** When the Overflow workspace's credit balance ran out, so it needs funding from then on. */
export const OVERFLOW_LAPSED_AT = clock(11, 30, 12);
/** When research's spend crossed its day's budget. */
export const RESEARCH_BUDGET_AT = clock(12, 41, 7);

/**
 * When an earlier holder of the `desktop` key id was deleted and the live key created: on the hour, twenty hours back,
 * so the holder's session is still inside the session expiry and its newest rows inside the request log's default day,
 * listed under the earlier key generation, while the Sessions view lists the live key's session alone, the holder's
 * last use having passed the non-companion timeout. Kept here because `pools.ts`, which dates the key, cannot import
 * `requests.ts`, which draws the rows.
 */
export const DELETED_HOLDER_END = startOfHour(NOW) - 20 * HOUR;
export const DELETED_HOLDER_DAY = new Date(DELETED_HOLDER_END).toDateString();
