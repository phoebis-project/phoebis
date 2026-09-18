import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from 'react';
import { api, MOCK, type Role, type SessionInfo } from '@/api';
import { ApiError, setHooks } from '@/api/http';
import { Button, Input } from '@/components/ui/primitives';
import { IconRoute } from '@/components/ui/icons';

/** Only the mock build previews a role, and the smoke runner remounts between checks, so the choice outlives a mount. */
const PREVIEW_KEY = 'phoebis.role';
const storedPreview = (): Role | null => {
  if (!MOCK) return null;
  try {
    const v = localStorage.getItem(PREVIEW_KEY);
    return v === 'admin' || v === 'viewer' ? v : null;
  } catch {
    return null;
  }
};

interface SessionState {
  clearToast: () => void;
  logout: () => Promise<void>;
  /** Mock builds may preview the viewer role; the served app reads the role from the session alone. */
  previewRole: Role | null;
  role: Role;
  sessionId: string;
  setPreviewRole: (role: Role | null) => void;
  /** What the transport reported last: a refusal, or a call that failed. The shell shows it once. */
  toast: string | null;
}

const Ctx = createContext<SessionState | null>(null);

export function SessionProvider({ children }: { children: ReactNode }) {
  const [session, setSession] = useState<SessionInfo | null | undefined>(undefined);
  const [notice, setNotice] = useState<string | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const [previewRole, setStoredPreviewRole] = useState<Role | null>(storedPreview);

  useEffect(() => {
    setHooks({
      // The route is named because a page draws several lists, and the operator has to know which one is gone.
      onFailure: ({ message, path, status }) => setToast(`${path} failed: ${message ?? (status === 0 ? 'the router did not answer' : `the router answered ${status}`)}`),
      // The server's own wording: the gate refuses a viewer's write and a request from an unconfigured
      // origin with the same status, and only it knows which one this was.
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

  const setPreviewRole = useCallback((role: Role | null) => {
    setStoredPreviewRole(role);
    if (!MOCK) return;
    try {
      if (role) localStorage.setItem(PREVIEW_KEY, role);
      else localStorage.removeItem(PREVIEW_KEY);
    } catch {
      // Private mode or blocked storage: the preview still applies to this mount.
    }
  }, []);

  const value = useMemo<SessionState | null>(
    () => (session ? { clearToast: () => setToast(null), logout, previewRole, role: (MOCK && previewRole) || session.role, sessionId: session.sessionId, setPreviewRole, toast } : null),
    [session, toast, logout, previewRole, setPreviewRole],
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
            <IconRoute size={18} />
          </span>
          <span className="brand__name">Phoebis Router</span>
        </div>
        <label className="field">
          <span className="field__label">Admin or viewer secret</span>
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
