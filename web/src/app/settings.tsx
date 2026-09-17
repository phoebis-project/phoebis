import { createContext, useContext, useEffect, useMemo, type ReactNode } from 'react';
import { setTimeFormat as applyTimeFormat, type TimeFormat } from '@/lib/format';
import { useLocalStorage, useMediaQuery } from '@/lib/hooks';

export type ThemeChoice = 'dark' | 'light' | 'system';
export type Role = 'admin' | 'viewer';

interface AppSettings {
  canMutate: boolean;
  resolvedTheme: 'dark' | 'light';
  role: Role;
  setRole: (r: Role) => void;
  setTheme: (t: ThemeChoice) => void;
  setTimeFormat: (f: TimeFormat) => void;
  theme: ThemeChoice;
  timeFormat: TimeFormat;
}

const Ctx = createContext<AppSettings | null>(null);

export function AppSettingsProvider({ children }: { children: ReactNode }) {
  const [theme, setTheme] = useLocalStorage<ThemeChoice>('phoebis.theme', 'system');
  const [role, setRole] = useLocalStorage<Role>('phoebis.role', 'admin');
  const [timeFormat, setTimeFormat] = useLocalStorage<TimeFormat>('phoebis.timeFormat', '24h');
  const prefersDark = useMediaQuery('(prefers-color-scheme: dark)');
  const resolvedTheme: 'dark' | 'light' = theme === 'system' ? (prefersDark ? 'dark' : 'light') : theme;

  useEffect(() => {
    document.documentElement.setAttribute('data-theme', resolvedTheme);
  }, [resolvedTheme]);

  useEffect(() => {
    applyTimeFormat(timeFormat);
  }, [timeFormat]);

  const value = useMemo<AppSettings>(
    () => ({ canMutate: role === 'admin', resolvedTheme, role, setRole, setTheme, setTimeFormat, theme, timeFormat }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [resolvedTheme, role, theme, timeFormat],
  );
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useAppSettings(): AppSettings {
  const v = useContext(Ctx);
  if (!v) throw new Error('useAppSettings outside AppSettingsProvider');
  return v;
}
