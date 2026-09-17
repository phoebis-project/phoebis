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
