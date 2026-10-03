import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from 'react';
import { api, type SessionInfo } from '@/api';
import { ApiError, setHooks } from '@/api/http';
import { Button, Input } from '@/components/ui/primitives';
import { Mark } from '@/components/ui/Mark';

interface SessionState {
  clearToast: () => void;
  logout: () => Promise<void>;
  sessionId: string;
  /** What the transport reported last: a refusal, or a call that failed. The shell shows it once. */
  toast: string | null;
}

const Ctx = createContext<SessionState | null>(null);

export function SessionProvider({ children }: { children: ReactNode }) {
  const [session, setSession] = useState<SessionInfo | null | undefined>(undefined);
  const [notice, setNotice] = useState<string | null>(null);
  const [toast, setToast] = useState<string | null>(null);

  useEffect(() => {
    setHooks({
      // The route is named because a page draws several lists, and the operator has to know which one is gone.
      onFailure: ({ message, path, status }) => setToast(`${path} failed: ${message ?? (status === 0 ? 'the router did not answer' : `the router answered ${status}`)}`),
      // The server's own wording: the origin check is what answers this status, and its message names the cause.
      onForbidden: (message) => setToast(message),
      onUnauthorized: () => {
        setNotice('Your session ended; log in again.');
        setSession(null);
      },
    });
    api.getSession().then(setSession, () => setSession(null));
  }, []);

  const logout = useCallback(async () => {
    try {
      await api.logout();
    } finally {
      setNotice(null);
      setSession(null);
    }
  }, []);

  const value = useMemo<SessionState | null>(
    () => (session ? { clearToast: () => setToast(null), logout, sessionId: session.sessionId, toast } : null),
    [session, toast, logout],
  );

  if (session === undefined) return null;
  if (!session || !value)
    return (
      <LoginScreen
        notice={notice}
        onLoggedIn={(s) => {
          setNotice(null);
          setSession(s);
        }}
      />
    );
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useSession(): SessionState {
  const v = useContext(Ctx);
  if (!v) throw new Error('useSession outside SessionProvider');
  return v;
}

export function LoginScreen({ notice, onLoggedIn }: { notice: string | null; onLoggedIn: (s: SessionInfo) => void }) {
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
        e instanceof ApiError && e.status === 429
          ? 'Too many failed logins; try again in a minute.'
          : e instanceof ApiError && e.status === 401
            ? 'That secret does not match.'
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
        <div className="brand">
          <span className="brand__mark" aria-hidden>
            <Mark size={48} />
          </span>
          <span className="brand__name">Phoebis Router</span>
          <p className="brand__couplet">Place every request. Keep every session.</p>
        </div>
        <label className="field">
          <span className="field__label">Admin secret</span>
          <Input type="password" autoComplete="current-password" autoFocus value={secret} onChange={(e) => setSecret(e.target.value)} />
        </label>
        {notice && (
          <p className="secondary" role="status">
            {notice}
          </p>
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
