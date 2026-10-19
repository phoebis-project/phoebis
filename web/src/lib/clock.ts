/** The one clock the pages' timers and instants read, so a test can move it. */
export interface Clock {
  clearInterval(id: number): void;
  clearTimeout(id: number): void;
  now(): number;
  setInterval(fn: () => void, ms: number): number;
  setTimeout(fn: () => void, ms: number): number;
}

// The window is read inside each interval arrow, so a bundle that loads this module without a DOM never touches it.
// The timeout pair reads the global, since the mock's answers wait on this timeout under Node's test runner too: there
// the handle is a timer object, and the global clear takes its number form.
export const wallClock: Clock = {
  clearInterval: (id) => window.clearInterval(id),
  clearTimeout: (id) => globalThis.clearTimeout(id),
  now: () => Date.now(),
  setInterval: (fn, ms) => window.setInterval(fn, ms),
  setTimeout: (fn, ms) => Number(globalThis.setTimeout(fn, ms)),
};

let current: Clock = wallClock;

/** Delegates to the installed clock on every call, so a module that imported it early still follows a swap. */
export const clock: Clock = {
  clearInterval: (id) => current.clearInterval(id),
  clearTimeout: (id) => current.clearTimeout(id),
  now: () => current.now(),
  setInterval: (fn, ms) => current.setInterval(fn, ms),
  setTimeout: (fn, ms) => current.setTimeout(fn, ms),
};

/** The test seam: `null` puts the wall clock back. */
export function installClock(next: Clock | null): void {
  current = next ?? wallClock;
}
