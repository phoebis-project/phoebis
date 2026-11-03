import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { api, type SessionInfo } from '@/api';
import { ApiError, SessionNotKept, setHooks } from '@/api/http';
import { Button, Field, Input } from '@/components/ui/primitives';
import { Mark } from '@/components/ui/Mark';
import { messageOf } from '@/lib/format';

export interface SessionState {
  /** Ends the dwell of the toast with this id, so the next one in the queue shows. */
  done: (id: number) => void;
  logout: () => Promise<void>;
  /** Puts a page's confirmation or refusal on the shell's queue. */
  notify: (message: string) => void;
  sessionId: string;
}

const Ctx = createContext<SessionState | null>(null);
// The queue changes with every toast, so it has a context of its own: a page that reads the session alone does not
// render again for a toast.
const QueueCtx = createContext<ToastItem[]>([]);

/** The toasts waiting, the first one on screen: the transport's failures and refusals and the pages' messages. */
export const useToastQueue = (): ToastItem[] => useContext(QueueCtx);

export interface ToastItem {
  dwellMs: number;
  id: number;
  kind: 'confirmation' | 'failure';
  message: string;
  /** The route a failure names, so the read's recovery can take it down; null for anything else. */
  path: string | null;
}

/**
 * A failure's dwell is long enough to read the route it names. A confirmation only acknowledges the action just taken,
 * for the 2.4 s the smoke run's confirmation reads are timed against.
 */
const DWELL_MS: Record<ToastItem['kind'], number> = { confirmation: 2_400, failure: 6_000 };
const isConfirmation = (t: ToastItem) => t.kind === 'confirmation';

/**
 * A message already queued adds nothing, and neither does a failure of a route already queued. A confirmation takes
 * the place of a queued confirmation, else the slot behind the toast on screen (a confirmation showing is replaced,
 * a failure showing keeps its turn), ahead of every queued failure. Anything else waits its turn.
 */
export function queueToast(queue: ToastItem[], item: ToastItem): ToastItem[] {
  if (queue.some((t) => t.message === item.message || (item.path !== null && t.path === item.path))) return queue;
  if (!isConfirmation(item)) return [...queue, item];
  const replaced = queue.findIndex(isConfirmation);
  if (replaced >= 0) return queue.map((t, i) => (i === replaced ? item : t));
  // The toast on screen keeps its turn: a failure cut short would lose the route it names.
  const slot = queue.length === 0 ? 0 : 1;
  return [...queue.slice(0, slot), item, ...queue.slice(slot)];
}

/** What the session read found: still asking, no session, no answer (its status, 0 when none came), or the session. */
type Probe = 'pending' | 'none' | { noAnswer: number } | SessionInfo;
const sessionOf = (probe: Probe): SessionInfo | null => (typeof probe === 'object' && !('noAnswer' in probe) ? probe : null);

export function SessionProvider({ children }: { children: ReactNode }) {
  const [probe, setProbe] = useState<Probe>('pending');
  const [notice, setNotice] = useState<string | null>(null);
  const [queue, setQueue] = useState<ToastItem[]>([]);
  const lastId = useRef(0);

  const push = useCallback((message: string, kind: ToastItem['kind'], path: string | null) => {
    lastId.current += 1;
    const item = { dwellMs: DWELL_MS[kind], id: lastId.current, kind, message, path };
    setQueue((q) => queueToast(q, item));
  }, []);
  const notify = useCallback((message: string) => push(message, 'confirmation', null), [push]);
  const done = useCallback((id: number) => setQueue((q) => q.filter((t) => t.id !== id)), []);
  const dismiss = useCallback((path: string) => setQueue((q) => q.filter((t) => t.path !== path)), []);

  // The card stays drawn while a Retry runs, and an answer that lands after a login went through leaves the session be.
  const probeSession = useCallback(() => {
    const settle = (next: Probe) => setProbe((current) => (sessionOf(current) ? current : next));
    api.getSession().then(
      (s) => settle(s ?? 'none'),
      (e: unknown) => settle({ noAnswer: e instanceof ApiError ? e.status : 0 }),
    );
  }, []);

  useEffect(() => {
    setHooks({
      // The route is named because a page draws several lists, and the operator has to know which one is gone.
      onFailure: ({ message, path, status }) => push(`${path} failed: ${message ?? (status === 0 ? 'the router did not answer' : `the router answered ${status}`)}`, 'failure', path),
      // The server's own wording: the origin check is what answers this status, and its message names the cause.
      onForbidden: (message) => push(message, 'failure', null),
      onRecovered: dismiss,
      // The queue belongs to the session that ended, so the next login starts with none of its toasts.
      onUnauthorized: () => {
        setQueue([]);
        setNotice('Your session ended, so log in again.');
        setProbe('none');
      },
    });
    probeSession();
  }, [dismiss, probeSession, push]);

  const logout = useCallback(async () => {
    try {
      await api.logout();
    } finally {
      setQueue([]);
      setNotice(null);
      setProbe('none');
    }
  }, []);

  const session = sessionOf(probe);
  const value = useMemo<SessionState | null>(
    () => (session ? { done, logout, notify, sessionId: session.sessionId } : null),
    [done, logout, notify, session],
  );

  if (probe === 'pending')
    return (
      <main className="login">
        <Brand />
      </main>
    );
  if (!session)
    return (
      <LoginScreen
        notice={notice}
        onLoggedIn={(s) => {
          setNotice(null);
          setProbe(s);
        }}
        onRetry={probeSession}
        outage={typeof probe === 'object' && 'noAnswer' in probe ? probe.noAnswer : null}
      />
    );
  return (
    <Ctx.Provider value={value}>
      <QueueCtx.Provider value={queue}>{children}</QueueCtx.Provider>
    </Ctx.Provider>
  );
}

export function useSession(): SessionState {
  const v = useContext(Ctx);
  if (!v) throw new Error('useSession outside SessionProvider');
  return v;
}

function Brand() {
  return (
    <div className="brand">
      <span className="brand__mark" aria-hidden>
        <Mark size={48} />
      </span>
      <span className="brand__name">Phoebis Router</span>
      <p className="brand__couplet">Place every request. Keep every session.</p>
    </div>
  );
}

/** `outage` is the status of a session read the router did not answer as 2xx or 401, 0 when no answer came at all. */
export function LoginScreen({ notice, onLoggedIn, onRetry, outage }: { notice: string | null; onLoggedIn: (s: SessionInfo) => void; onRetry: () => void; outage: number | null }) {
  const [secret, setSecret] = useState('');
  const [stay, setStay] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      onLoggedIn(await api.login(secret, stay));
    } catch (e) {
      setError(
        e instanceof SessionNotKept
          ? 'The router accepted the secret, but the browser kept no session cookie. The usual causes: a proxy that rewrites the path or drops Set-Cookie, cookies blocked for this site, or a Secure cookie served over plain HTTP.'
          : e instanceof ApiError && e.status === 429
            ? 'Too many failed logins, so try again in a minute.'
            : e instanceof ApiError && e.status === 401
              ? 'That secret does not match.'
              : e instanceof ApiError && e.status === 0
                ? 'The router did not answer.'
                : messageOf(e),
      );
    } finally {
      setBusy(false);
    }
  };
  return (
    <main className="login">
      <form
        className="login__card card card--padded"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <Brand />
        <label className="field">
          <span className="field__label">Admin secret</span>
          <Input type="password" autoComplete="current-password" autoFocus value={secret} onChange={(e) => setSecret(e.target.value)} />
        </label>
        <Field label="Stay logged in" beside hint="Keeps this browser logged in for 30 days, renewed on every visit. A secret rotation ends it earlier.">
          <input type="checkbox" checked={stay} onChange={(e) => setStay(e.target.checked)} />
        </Field>
        {notice && (
          <p className="secondary" role="status">
            {notice}
          </p>
        )}
        {outage !== null && (
          <div className="row">
            <p className="pairing__expired" role="alert">
              The router did not answer{outage ? ` (${outage})` : ''}.
            </p>
            <Button size="sm" onClick={onRetry}>
              Retry
            </Button>
          </div>
        )}
        {error && (
          <p className="pairing__expired" role="alert">
            {error}
          </p>
        )}
        <Button type="submit" variant="primary" disabled={!secret || busy}>
          {busy ? 'Logging in…' : 'Log in'}
        </Button>
      </form>
    </main>
  );
}
