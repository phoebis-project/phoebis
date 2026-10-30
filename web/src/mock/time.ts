export const MINUTE = 60_000;
export const HOUR = 60 * MINUTE;
export const DAY = 24 * HOUR;

// Anchored once at load so every relative label counts down from the same instant.
export const NOW = Date.now();

export const iso = (ms: number) => new Date(ms).toISOString();
export const ago = (ms: number) => iso(NOW - ms);
export const ahead = (ms: number) => iso(NOW + ms);
export const hoursAgo = (h: number) => ago(h * HOUR);
export const daysAgo = (d: number) => ago(d * DAY);
export const startOfHour = (ms: number) => Math.floor(ms / HOUR) * HOUR;

/**
 * When an earlier holder of the `desktop` key id was deleted and the live key created: on the hour, twenty hours back,
 * so the holder's session is still inside the session expiry and its newest rows inside the request log's default day,
 * listed under the earlier key generation, while the Sessions view lists the live key's session alone, the holder's
 * last use having passed the non-companion timeout. Kept here because `pools.ts`, which dates the key, cannot import
 * `requests.ts`, which draws the rows.
 */
export const DELETED_HOLDER_END = startOfHour(NOW) - 20 * HOUR;
export const DELETED_HOLDER_DAY = new Date(DELETED_HOLDER_END).toDateString();
