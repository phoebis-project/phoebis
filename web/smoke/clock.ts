import type { Clock } from '@/lib/clock';

export type FakeClock = Clock & { freeze(at: number | null): void; tick(ms: number): void };

interface Timer {
  due: number;
  every: number | null;
  fn: () => void;
  realId: number | null;
}

export function fakeClock(): FakeClock {
  // Live (frozenAt null) reads the wall and backs each timer with a window timer, so a check that waits real time
  // sees it fire. Frozen arms no window timer: only `tick` moves the instant or fires a timer, so a slow settle fires
  // none and a check reads the same timers on any machine.
  let frozenAt: number | null = null;
  let nextId = 1;
  const timers = new Map<number, Timer>();
  const now = () => frozenAt ?? Date.now();
  const back = (id: number) => {
    const t = timers.get(id);
    if (t && frozenAt === null) t.realId = window.setTimeout(() => run(id), Math.max(0, t.due - now()));
  };
  const unback = (t: Timer) => {
    if (t.realId !== null) window.clearTimeout(t.realId);
    t.realId = null;
  };
  // An interval's next deadline is set before its callback runs, so a clearInterval inside the callback holds.
  const run = (id: number) => {
    const t = timers.get(id);
    if (!t) return;
    unback(t);
    if (t.every === null) timers.delete(id);
    else t.due += t.every;
    t.fn();
    if (t.every !== null) back(id);
  };
  const add = (fn: () => void, ms: number, every: number | null) => {
    const id = nextId++;
    timers.set(id, { due: now() + ms, every, fn, realId: null });
    back(id);
    return id;
  };
  const drop = (id: number) => {
    const t = timers.get(id);
    if (t) unback(t);
    timers.delete(id);
  };
  return {
    clearInterval: drop,
    clearTimeout: drop,
    // A pending timer crosses a mode switch: freezing takes its window timer away, thawing arms one for the time left.
    freeze: (at) => {
      for (const t of timers.values()) unback(t);
      frozenAt = at;
      for (const id of timers.keys()) back(id);
    },
    now,
    setInterval: (fn, ms) => add(fn, ms, ms),
    setTimeout: (fn, ms) => add(fn, ms, null),
    tick: (ms) => {
      if (frozenAt === null) throw new Error('tick needs a frozen clock: mount(path, { now })');
      const end = frozenAt + ms;
      for (;;) {
        const next = [...timers.entries()].filter(([, t]) => t.due <= end).sort((x, y) => x[1].due - y[1].due || x[0] - y[0])[0];
        if (!next) break;
        // The timer runs at its own deadline, so what it reads and arms starts there.
        frozenAt = next[1].due;
        run(next[0]);
      }
      frozenAt = end;
    },
  };
}
