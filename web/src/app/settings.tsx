import { createContext, useContext, useEffect, useMemo, useState, type ReactNode } from 'react';
import type { Role } from '@/api';
import { setTimeFormat as applyTimeFormat, type TimeFormat } from '@/lib/format';
import { useLocalStorage, useMediaQuery } from '@/lib/hooks';
import { useSession } from './session';

export type ThemeChoice = 'dark' | 'light' | 'system';
export type { Role };

interface AppSettings {
  canMutate: boolean;
  /** Bumped by every time-format change, so a component that only formats times has something to depend on. */
  formatVersion: number;
  resolvedTheme: 'dark' | 'light';
  role: Role;
  setTheme: (t: ThemeChoice) => void;
  setTimeFormat: (f: TimeFormat) => void;
  theme: ThemeChoice;
  timeFormat: TimeFormat;
}

const Ctx = createContext<AppSettings | null>(null);

export function AppSettingsProvider({ children }: { children: ReactNode }) {
  const { role } = useSession();
  const [theme, setTheme] = useLocalStorage<ThemeChoice>('phoebis.theme', 'system');
  const [storedFormat, storeFormat] = useLocalStorage<TimeFormat>('phoebis.timeFormat', '24h');
  // Applied while the first render is still being built, so the timestamps below are already in the stored format.
  const [formatVersion, setFormatVersion] = useState(() => {
    applyTimeFormat(storedFormat);
    return 0;
  });
  const prefersDark = useMediaQuery('(prefers-color-scheme: dark)');
  const resolvedTheme: 'dark' | 'light' = theme === 'system' ? (prefersDark ? 'dark' : 'light') : theme;

  useEffect(() => {
    document.documentElement.setAttribute('data-theme', resolvedTheme);
  }, [resolvedTheme]);

  // The module formatters have to answer in the new format before the context change re-renders the components below.
  const setTimeFormat = (f: TimeFormat) => {
    applyTimeFormat(f);
    storeFormat(f);
    setFormatVersion((v) => v + 1);
  };

  const value = useMemo<AppSettings>(
    () => ({ canMutate: role === 'admin', formatVersion, resolvedTheme, role, setTheme, setTimeFormat, theme, timeFormat: storedFormat }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [formatVersion, resolvedTheme, role, theme, storedFormat],
  );
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useAppSettings(): AppSettings {
  const v = useContext(Ctx);
  if (!v) throw new Error('useAppSettings outside AppSettingsProvider');
  return v;
}

/** For a component that formats times and reads no other setting: it re-renders on the preference, not on a clock tick. */
export const useFormatVersion = (): number => useAppSettings().formatVersion;
