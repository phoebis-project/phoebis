import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from 'react';
import type { ModelTiers } from '@/api/types';
import { setTimeFormat as applyTimeFormat, type TimeFormat } from '@/lib/format';
import { useLocalStorage, useMediaQuery } from '@/lib/hooks';
import { setModelTiers as applyModelTiers } from '@/lib/routingLog';
import { positionOf, weekOf, type UnitPosition, type WeekPosition } from '@/lib/unit';

export type ExpertChoice = 'off' | 'on';
export type ThemeChoice = 'dark' | 'light' | 'system';

interface AppSettings {
  expertView: boolean;
  /** Bumped by every time-format change, so a component that only formats times has something to depend on. */
  formatVersion: number;
  /**
   * The cost model's two tier members, which every list of models orders by. The empty pair until the settings answer.
   */
  modelTiers: ModelTiers;
  resolvedTheme: 'dark' | 'light';
  setExpertView: (on: boolean) => void;
  setModelTiers: (tiers: ModelTiers) => void;
  setTheme: (t: ThemeChoice) => void;
  setTimeFormat: (f: TimeFormat) => void;
  setUnit: (u: UnitPosition) => void;
  setWeek: (w: WeekPosition) => void;
  theme: ThemeChoice;
  timeFormat: TimeFormat;
  /** The unit every view prices a request in, kept in this browser alone. */
  unit: UnitPosition;
  /** The week a subscription figure reads in under Native, kept in this browser alone beside the unit. */
  week: WeekPosition;
}

const Ctx = createContext<AppSettings | null>(null);

const NO_TIERS: ModelTiers = { modelTier: {}, tierOrder: [] };

const themeOf = (raw: string | null): ThemeChoice => (raw === 'dark' || raw === 'light' ? raw : 'system');
const timeFormatOf = (raw: string | null): TimeFormat => (raw === '12h' ? raw : '24h');
const expertOf = (raw: string | null): ExpertChoice => (raw === 'on' ? raw : 'off');

export function AppSettingsProvider({ children }: { children: ReactNode }) {
  const [theme, setTheme] = useLocalStorage('phoebis.theme', themeOf);
  const [storedFormat, storeFormat] = useLocalStorage('phoebis.timeFormat', timeFormatOf);
  const [storedExpert, storeExpert] = useLocalStorage('phoebis.expertView', expertOf);
  const expertView = storedExpert === 'on';
  // The setters keep their identity across renders, so the context value changes with the settings alone.
  const setExpertView = useCallback((on: boolean) => storeExpert(on ? 'on' : 'off'), [storeExpert]);
  const [unit, setUnit] = useLocalStorage('phoebis.unit', positionOf);
  const [week, setWeek] = useLocalStorage('phoebis.week', weekOf);
  const [modelTiers, storeModelTiers] = useState<ModelTiers>(NO_TIERS);
  // The routing log's comparator reads the module's pair, so it holds the new order before the context change
  // re-renders the lists that sort through it.
  const setModelTiers = useCallback((tiers: ModelTiers) => {
    applyModelTiers(tiers);
    storeModelTiers(tiers);
  }, []);
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
  const setTimeFormat = useCallback(
    (f: TimeFormat) => {
      applyTimeFormat(f);
      storeFormat(f);
      setFormatVersion((v) => v + 1);
    },
    [storeFormat],
  );

  const value = useMemo<AppSettings>(
    () => ({ expertView, formatVersion, modelTiers, resolvedTheme, setExpertView, setModelTiers, setTheme, setTimeFormat, setUnit, setWeek, theme, timeFormat: storedFormat, unit, week }),
    [expertView, formatVersion, modelTiers, resolvedTheme, setExpertView, setModelTiers, setTheme, setTimeFormat, setUnit, setWeek, storedFormat, theme, unit, week],
  );
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useAppSettings(): AppSettings {
  const v = useContext(Ctx);
  if (!v) throw new Error('useAppSettings outside AppSettingsProvider');
  return v;
}

/**
 * For a component that formats times and reads no other setting: it re-renders on the preference, not on a clock tick.
 */
export const useFormatVersion = (): number => useAppSettings().formatVersion;
