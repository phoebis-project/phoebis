export const MINUTE = 60_000;
export const HOUR = 3_600_000;

/** A span the way the Fact cell reads one: `2.0 s`, `2 min 41 s`, `1 h 2 min`, `1 d 4 h`. */
export function span(ms: number): string {
  const abs = Math.abs(ms);
  if (abs < 1_000) return `${Math.round(abs)} ms`;
  if (abs < MINUTE) return `${(abs / 1_000).toFixed(abs >= 10_000 ? 0 : 1)} s`;
  const pair = (major: number, majorUnit: string, minor: number, minorUnit: string) => (minor > 0 ? `${major} ${majorUnit} ${minor} ${minorUnit}` : `${major} ${majorUnit}`);
  if (abs < HOUR) return pair(Math.floor(abs / MINUTE), 'min', Math.floor((abs % MINUTE) / 1_000), 's');
  if (abs < 24 * HOUR) return pair(Math.floor(abs / HOUR), 'h', Math.floor((abs % HOUR) / MINUTE), 'min');
  return pair(Math.floor(abs / (24 * HOUR)), 'd', Math.floor((abs % (24 * HOUR)) / HOUR), 'h');
}

/** A reading's age, whole seconds under a minute. */
export const ageWord = (ms: number) => (ms < MINUTE ? `${Math.round(ms / 1_000)} s` : span(ms));
