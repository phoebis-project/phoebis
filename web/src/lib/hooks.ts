import { useEffect, useRef, useState } from 'react';
import { quietly } from '@/api/http';
import { clock } from './clock';

export interface Loaded<T> {
  data: T | null;
  error: string | null;
  /** When the answer in `data` landed, as an ISO instant; null until the first one. */
  fetchedAt: string | null;
  loading: boolean;
  reload: () => void;
  /**
   * `data` does not answer the current deps: a load for other deps is running, or the last load failed. The
   * data is kept for callers that tolerate an older answer; one that draws it as the answer checks this first.
   */
  stale: boolean;
}

export interface AsyncOptions {
  /**
   * Re-run `load` every `refreshMs` while the document is visible and `paused` is false; a refresh keeps `data` drawn,
   * never sets `loading`, replaces `data` on success and keeps it on failure with `error` set; a refresh still in
   * flight when `paused` turns true is dropped.
   */
  refreshMs?: number;
  paused?: boolean;
}

const sameDeps = (a: readonly unknown[], b: readonly unknown[]) => a.length === b.length && a.every((v, i) => Object.is(v, b[i]));
const messageOf = (err: unknown) => (err instanceof Error ? err.message : String(err));
const stamp = () => new Date(clock.now()).toISOString();

export function useAsync<T>(load: () => Promise<T>, deps: readonly unknown[] = [], options: AsyncOptions = {}): Loaded<T> {
  const { paused = false, refreshMs } = options;
  const [data, setData] = useState<T | null>(null);
  const [loadedFor, setLoadedFor] = useState<readonly unknown[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [fetchedAt, setFetchedAt] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [nonce, setNonce] = useState(0);
  // The refresh effect outlives renders, so it reads the current load, deps and stamp through refs.
  const latest = useRef({ deps, load });
  latest.current = { deps, load };
  const fetched = useRef<string | null>(null);
  // The load effect's run for the current deps: a refresh that lands after the deps changed or the page left is
  // dropped by it, as the load's own answer is.
  const run = useRef({ alive: false });
  // Whether the last read, the load or a refresh, failed: the next refresh runs quiet, so a failing streak toasts once.
  const failing = useRef(false);

  const landed = (value: T, answered: readonly unknown[]) => {
    const at = stamp();
    failing.current = false;
    fetched.current = at;
    setData(value);
    setLoadedFor(answered);
    setError(null);
    setFetchedAt(at);
  };
  const failed = (err: unknown) => {
    failing.current = true;
    setError(messageOf(err));
  };

  useEffect(() => {
    const current = { alive: true };
    run.current = current;
    setLoading(true);
    load().then(
      (value) => {
        if (!current.alive) return;
        landed(value, deps);
        setLoading(false);
      },
      (err: unknown) => {
        if (!current.alive) return;
        failed(err);
        setLoading(false);
      },
    );
    return () => {
      current.alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, nonce]);

  useEffect(() => {
    if (!refreshMs || paused) return;
    // Ends as the pause starts, so an answer that lands after a dialog, an editor or a menu opened changes nothing
    // under it: a page's reset effect keyed on the data would wipe what the operator typed.
    const armed = { alive: true };
    const refresh = () => {
      const loadRun = run.current;
      const { deps: answered, load: read } = latest.current;
      (failing.current ? quietly(read) : read()).then(
        (value) => {
          if (armed.alive && loadRun.alive) landed(value, answered);
        },
        (err: unknown) => {
          if (armed.alive && loadRun.alive) failed(err);
        },
      );
    };
    const id = clock.setInterval(() => {
      if (!document.hidden) refresh();
    }, refreshMs);
    // A tab that comes back reads at once only when its last answer is a full period old.
    const onVisible = () => {
      if (!document.hidden && fetched.current !== null && clock.now() - Date.parse(fetched.current) >= refreshMs) refresh();
    };
    document.addEventListener('visibilitychange', onVisible);
    return () => {
      armed.alive = false;
      clock.clearInterval(id);
      document.removeEventListener('visibilitychange', onVisible);
    };
    // The load is read through `latest` when the period fires, so the effect re-arms only when the period or the pause
    // changes.
  }, [refreshMs, paused]);

  return { data, error, fetchedAt, loading, reload: () => setNonce((n) => n + 1), stale: error !== null || (loadedFor !== null && !sameDeps(loadedFor, deps)) };
}

/** A shared clock so every countdown on a page advances in the same tick. */
export function useNow(intervalMs = 15_000): number {
  const [now, setNow] = useState(() => clock.now());
  useEffect(() => {
    const id = clock.setInterval(() => setNow(clock.now()), intervalMs);
    return () => clock.clearInterval(id);
  }, [intervalMs]);
  return now;
}

export function useLocalStorage<T extends string>(key: string, fallback: T): [T, (v: T) => void] {
  const [value, setValue] = useState<T>(() => {
    try {
      return (localStorage.getItem(key) as T | null) ?? fallback;
    } catch {
      return fallback;
    }
  });
  const set = (v: T) => {
    setValue(v);
    try {
      localStorage.setItem(key, v);
    } catch {
      // Private mode or blocked storage: the in-memory value still applies.
    }
  };
  return [value, set];
}

export function useMediaQuery(query: string): boolean {
  const [matches, setMatches] = useState(() => window.matchMedia(query).matches);
  useEffect(() => {
    const mq = window.matchMedia(query);
    const onChange = () => setMatches(mq.matches);
    mq.addEventListener('change', onChange);
    return () => mq.removeEventListener('change', onChange);
  }, [query]);
  return matches;
}
