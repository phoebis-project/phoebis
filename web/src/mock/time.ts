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
 * The day an earlier holder of the `desktop` key id was deleted, nine days back, and the instant its rows end and the
 * live key is created: 11:00 that day on the local clock the story draws its hours by. Kept here because `pools.ts`,
 * which dates the key, cannot import `requests.ts`, which draws the rows.
 */
export const DELETED_HOLDER_DAY = new Date(NOW - 9 * DAY).toDateString();
export const DELETED_HOLDER_END = new Date(NOW - 9 * DAY).setHours(11, 0, 0, 0);
