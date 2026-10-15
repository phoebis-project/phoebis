/**
 * The router's lead over pace, as the account answer carries it, computed by the mock in the router's place: the
 * window's utilization less the share of it elapsed from its start to the earlier of its reset and the account's end
 * (`endOf`), clamped to 0 to 1, and the utilization alone without a reset. The production bundle reads the field.
 */
export function leadOf(window: { resetsAt: string | null; utilization: number }, lengthMs: number, now: number, end: number | null): number {
  if (window.resetsAt === null) return window.utilization;
  const reset = Date.parse(window.resetsAt);
  const start = reset - lengthMs;
  const span = Math.min(reset, end ?? reset) - start;
  // Past its reset a window reads fully elapsed, where the router reads its own use since: an accepted approximation.
  const elapsed = span <= 0 ? 1 : (now - start) / span;
  return window.utilization - Math.min(1, Math.max(0, elapsed));
}
