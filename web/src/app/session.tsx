import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { api, type SessionInfo } from '@/api';
import { ApiError, SessionNotKept, setHooks } from '@/api/http';
import { Button, Input } from '@/components/ui/primitives';
import { Mark } from '@/components/ui/Mark';

export interface SessionState {
  /** Ends the dwell of the toast with this id, so the next one in the queue shows. */
  done: (id: number) => void;
  logout: () => Promise<void>;
  /** Puts a page's confirmation or refusal on the shell's queue. */
  notify: (message: string) => void;
  /** The toasts waiting, the first one on screen: the transport's failures and refusals and the pages' messages. */
  queue: ToastItem[];
  sessionId: string;
}

const Ctx = createContext<SessionState | null>(null);

export interface ToastItem {
  dwellMs: number;
  id: number;
  message: string;
  /** The route a failure names, so the read's recovery can take it down; null for anything else. */
  path: string | null;
}

/**
 * Long enough to read the route a failure names. A confirmation only acknowledges the action just taken, for the 2.4 s
 * the smoke run's confirmation reads are timed against.
 */
const FAILURE_MS = 6_000;
const CONFIRMATION_MS = 2_400;
const isConfirmation = (t: ToastItem) => t.dwellMs === CONFIRMATION_MS;

/**
 * A message already queued adds nothing, and neither does a failure of a route already queued. A confirmation takes
 * the place of the one queued, on screen or waiting, since the newest action's result is the one the operator wants
 * to read; it never displaces a failure. Anything else waits its turn.
 */
export function queueToast(queue: ToastItem[], item: ToastItem): ToastItem[] {
  if (queue.some((t) => t.message === item.message || (item.path !== null && t.path === item.path))) return queue;
  const replaced = isConfirmation(item) ? queue.findIndex(isConfirmation) : -1;
  return replaced < 0 ? [...queue, item] : queue.map((t, i) => (i === replaced ? item : t));
}

/** What the session read found: still asking, no session, no answer (its status, 0 when none came), or the session. */
type Probe = 'pending' | 'none' | { noAnswer: number } | SessionInfo;
const sessionOf = (probe: Probe): SessionInfo | null => (typeof probe === 'object' && !('noAnswer' in probe) ? probe : null);

export function SessionProvider({ children }: { children: ReactNode }) {
  const [probe, setProbe] = useState<Probe>('pending');
  const [notice, setNotice] = useState<string | null>(null);
  const [queue, setQueue] = useState<ToastItem[]>([]);
  const lastId = useRef(0);

  const push = useCallback((message: string, dwellMs: number, path: string | null) => {
    lastId.current += 1;
    const item = { dwellMs, id: lastId.current, message, path };
    setQueue((q) => queueToast(q, item));
  }, []);
  const notify = useCallback((message: string) => push(message, CONFIRMATION_MS, null), [push]);
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
      onFailure: ({ message, path, status }) => push(`${path} failed: ${message ?? (status === 0 ? 'the router did not answer' : `the router answered ${status}`)}`, FAILURE_MS, path),
      // The server's own wording: the origin check is what answers this status, and its message names the cause.
      onForbidden: (message) => push(message, FAILURE_MS, null),
      onRecovered: dismiss,
      // The queue belongs to the session that ended, so the next login starts with none of its toasts.
      onUnauthorized: () => {
        setQueue([]);
        setNotice('Your session ended; log in again.');
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
    () => (session ? { done, logout, notify, queue, sessionId: session.sessionId } : null),
    [done, logout, notify, queue, session],
  );

  if (probe === 'pending')
    return (
      <main className="login">
        <Brand />
      </main>
    );
  if (!value)
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
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
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
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      onLoggedIn(await api.login(secret));
    } catch (e) {
      setError(
        e instanceof SessionNotKept
          ? 'The router accepted the secret, but the browser kept no session cookie. The usual causes: a proxy that rewrites the path or drops Set-Cookie, cookies blocked for this site, or a Secure cookie served over plain HTTP.'
          : e instanceof ApiError && e.status === 429
            ? 'Too many failed logins; try again in a minute.'
            : e instanceof ApiError && e.status === 401
              ? 'That secret does not match.'
              : e instanceof ApiError && e.status === 0
                ? 'The router did not answer.'
                : e instanceof Error
                  ? e.message
                  : String(e),
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
