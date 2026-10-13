import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore, type KeyboardEvent } from 'react';
import { useLocation, useNavigate, useSearchParams } from 'react-router-dom';
import { api } from '@/api';
import { ApiError, quietly, reportRecovery } from '@/api/http';
import type { Codec, RangeValue } from './address';
import { createAttentionPoll, type AttentionRead } from './attention';
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
   * Re-run `load` every `refreshMs` while the document is visible and `paused` is false. A refresh keeps `data` drawn,
   * never turns `loading` on, replaces `data` on success and keeps it on failure with `error` set, and one still in
   * flight when `paused` turns true is dropped. When the pause ends, the period is set or the tab comes back, a refresh
   * runs at once if the last answer is a full period old. A read that settles after a newer one has landed is dropped.
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
  // The effects outlive renders, so they read the current load, deps and stamp through refs.
  const latest = useRef({ deps, load });
  useLayoutEffect(() => {
    latest.current = { deps, load };
  });
  // The deps the load effect keys on, replaced only when an element differs, so a caller's new array with the same
  // values reads nothing again.
  const [held, setHeld] = useState(deps);
  if (!sameDeps(held, deps)) setHeld(deps);
  const fetched = useRef<string | null>(null);
  // The load effect's run for the current deps: a refresh that lands after the deps changed or the page left is
  // dropped by it, as the load's own answer is.
  const run = useRef({ alive: false });
  // Every read takes the next number, and one that settles after a higher number has landed is dropped, success and
  // failure alike, so an older answer never replaces a newer one; a slow read still lands while nothing newer has.
  const started = useRef(0);
  const landedSeq = useRef(0);
  /** Whether the read numbered `seq` may land, which marks it as the newest landed. */
  const lands = (seq: number) => {
    if (seq < landedSeq.current) return false;
    landedSeq.current = seq;
    return true;
  };
  // Whether the last read, the load or a refresh, failed: the next refresh runs quiet, so a failing streak toasts once.
  const failing = useRef(false);
  // The routes the streak failed on, each reported recovered once by the read that ends it, which takes down the
  // failure notice the streak raised.
  const failedPaths = useRef(new Set<string>());
  // The deps the streak's routes failed for.
  const failedFor = useRef(deps);

  const landed = useCallback((value: T, answered: readonly unknown[]) => {
    const at = stamp();
    for (const path of failedPaths.current) reportRecovery(path);
    failedPaths.current.clear();
    failing.current = false;
    fetched.current = at;
    setData(value);
    setLoadedFor(answered);
    setError(null);
    setFetchedAt(at);
  }, []);
  const failed = (err: unknown) => {
    failing.current = true;
    if (err instanceof ApiError && err.path !== null) failedPaths.current.add(err.path);
    setError(messageOf(err));
  };

  useEffect(() => {
    // A read for other deps answers other routes, so its success must not report the old streak's routes recovered; a
    // reload of the same deps keeps them, so its success still takes the streak's notice down.
    if (!sameDeps(failedFor.current, held)) {
      failedPaths.current.clear();
      failedFor.current = held;
    }
    const current = { alive: true };
    run.current = current;
    // eslint-disable-next-line react-hooks/set-state-in-effect -- a read for new deps starts here, and the page draws its loading state until it lands
    setLoading(true);
    started.current += 1;
    const seq = started.current;
    latest.current.load().then(
      (value) => {
        if (!current.alive || !lands(seq)) return;
        landed(value, held);
        setLoading(false);
      },
      (err: unknown) => {
        if (!current.alive || !lands(seq)) return;
        failed(err);
        setLoading(false);
      },
    );
    return () => {
      current.alive = false;
    };
    // Callers pass load inline, a new closure each render, so it is read through `latest` and the deps alone decide
    // when the effect reads again.
  }, [held, landed, nonce]);

  useEffect(() => {
    if (!refreshMs || paused) return;
    // Ends as the pause starts, so an answer that lands after a dialog, an editor or a menu opened changes nothing
    // under it: a page's reset effect keyed on the data would wipe what the operator typed.
    const armed = { alive: true };
    const refresh = () => {
      const loadRun = run.current;
      const { deps: answered, load: read } = latest.current;
      started.current += 1;
      const seq = started.current;
      // A refresh that outruns the load it supersedes ends the loading too, since that load's answer will be dropped.
      (failing.current ? quietly(read) : read()).then(
        (value) => {
          if (!armed.alive || !loadRun.alive || !lands(seq)) return;
          landed(value, answered);
          setLoading(false);
        },
        (err: unknown) => {
          if (!armed.alive || !loadRun.alive || !lands(seq)) return;
          failed(err);
          setLoading(false);
        },
      );
    };
    const id = clock.setInterval(() => {
      if (!document.hidden) refresh();
    }, refreshMs);
    // A tab that comes back, and a pause that ends, read at once only when the last answer is a full period old.
    const onVisible = () => {
      if (!document.hidden && fetched.current !== null && clock.now() - Date.parse(fetched.current) >= refreshMs) refresh();
    };
    onVisible();
    document.addEventListener('visibilitychange', onVisible);
    return () => {
      armed.alive = false;
      clock.clearInterval(id);
      document.removeEventListener('visibilitychange', onVisible);
    };
    // The load is read through `latest` when the period fires, so the effect re-arms only when the period or the pause
    // changes.
  }, [landed, paused, refreshMs]);

  return { data, error, fetchedAt, loading, reload: () => setNonce((n) => n + 1), stale: error !== null || (loadedFor !== null && !sameDeps(loadedFor, deps)) };
}

/**
 * A text field's draft that commits `delayMs` after the last keystroke, at once on Enter and on blur; an outside change
 * replaces the draft and cancels its pending commit.
 */
export function useCommittedText(value: string, onCommit: (next: string) => void, delayMs = 400): { draft: string; onBlur: () => void; onChange: (next: string) => void; onKeyDown: (e: KeyboardEvent<HTMLInputElement>) => void } {
  const [draft, setDraft] = useState(value);
  const committed = useRef(value);
  const timer = useRef<number | null>(null);
  // A pending commit fires after later renders, so it reads the caller's latest handler, never the one its keystroke saw.
  const latest = useRef(onCommit);
  useLayoutEffect(() => {
    latest.current = onCommit;
  });
  const clear = () => {
    if (timer.current !== null) clock.clearTimeout(timer.current);
    timer.current = null;
  };
  // An outside change (a reset, the URL) replaces the draft and drops the old draft's pending commit, which would
  // otherwise restore the value the caller just replaced; the caller echoing a commit back does not, so typing is never
  // undone mid-word. It runs before paint, so the field never shows the replaced draft for a frame.
  useLayoutEffect(() => {
    if (value !== committed.current) {
      clear();
      committed.current = value;
      setDraft(value);
    }
  }, [value]);
  const commit = (next: string) => {
    clear();
    if (next === committed.current) return;
    committed.current = next;
    latest.current(next);
  };
  useEffect(() => clear, []);
  return {
    draft,
    onBlur: () => commit(draft),
    onChange: (next) => {
      setDraft(next);
      clear();
      timer.current = clock.setTimeout(() => commit(next), delayMs);
    },
    onKeyDown: (e) => {
      if (e.key === 'Enter') commit(draft);
    },
  };
}

/** A view's filters in the URL's query string: read on every address, written whole with `replace`, so a link sets
 * them, a reload keeps them and Back returns to the view before. A filter change makes a new address, which a caller
 * keys its cursor on. The hash stays on every write, since a tab, a section or an anchor lives there. */
export function useQueryFilters<F>(codec: Codec<F>): { address: string; filters: F; update(patch: Partial<F>): void; replace(filters: F): void } {
  const [searchParams] = useSearchParams();
  const location = useLocation();
  const navigate = useNavigate();
  // Callers pass a module-level codec, so only a new address reads the filters again.
  const filters = useMemo(() => codec.fromParams(searchParams), [codec, searchParams]);
  // Written at once rather than in the router's transition: a click that follows typing blurs a field first, and its
  // own write or navigation would otherwise build on the address from before the blur's commit and drop it.
  const replace = (next: F) => {
    const search = codec.toParams(next).toString();
    navigate({ hash: location.hash, pathname: location.pathname, search: search ? `?${search}` : '' }, { flushSync: true, replace: true });
  };
  return { address: searchParams.toString(), filters, replace, update: (patch) => replace({ ...filters, ...patch }) };
}

/**
 * The instant the page took its range, read again only when the range moves: another preset, or a span with another
 * `from` or `to`. A link capped at now ends its span at the clock it was made on, so this instant tells it from a span
 * that has ended, and a chip, the search or a view control, which leave the range as it stands, keep it reading on.
 */
export function useRangeTakenAt(range: RangeValue): number {
  const key = `${range.preset} ${range.from} ${range.to}`;
  const [taken, setTaken] = useState(() => ({ at: clock.now(), for: key }));
  if (taken.for !== key) setTaken({ at: clock.now(), for: key });
  return taken.at;
}

const attention = createAttentionPoll(() => api.getAttention());

/**
 * The conditions that need attention, from one poll however many readers a page mounts: the dashboard's line and
 * every pool card's rows read the same answer. `useAsync`'s single load would never show a pool exhausting, or a
 * failure clearing, while the page stays open.
 */
export function useAttention(): AttentionRead {
  return useSyncExternalStore(attention.subscribe, attention.snapshot);
}

/** One read of the attention poll now, published to every mounted reader, so the Replay button clears the parked
 *  badge without waiting for the next tick or a focus. */
export function refreshAttention(): Promise<void> {
  return attention.refresh();
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

const onVisibility = (changed: () => void) => {
  document.addEventListener('visibilitychange', changed);
  return () => document.removeEventListener('visibilitychange', changed);
};

/** document.hidden, followed through visibilitychange. */
export function useDocumentHidden(): boolean {
  return useSyncExternalStore(onVisibility, () => document.hidden);
}

export function useLocalStorage<T extends string>(key: string, fallback: T): [T, (v: T) => void] {
  const [value, setValue] = useState<T>(() => {
    try {
      return (localStorage.getItem(key) as T | null) ?? fallback;
    } catch {
      return fallback;
    }
  });
  const set = useCallback(
    (v: T) => {
      setValue(v);
      try {
        localStorage.setItem(key, v);
      } catch {
        // Private mode or blocked storage: the in-memory value still applies.
      }
    },
    [key],
  );
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
