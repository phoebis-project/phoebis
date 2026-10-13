import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from 'react';
import type { ModelTiers } from '@/api/types';
import { setTimeFormat as applyTimeFormat, type TimeFormat } from '@/lib/format';
import { useLocalStorage, useMediaQuery } from '@/lib/hooks';
import { setModelTiers as applyModelTiers } from '@/lib/routingLog';
import { positionOf, type UnitPosition } from '@/lib/unit';

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
  theme: ThemeChoice;
  timeFormat: TimeFormat;
  /** The unit every view prices a request in, kept in this browser alone. */
  unit: UnitPosition;
}

const Ctx = createContext<AppSettings | null>(null);

const NO_TIERS: ModelTiers = { modelTier: {}, tierOrder: [] };

export function AppSettingsProvider({ children }: { children: ReactNode }) {
  const [theme, setTheme] = useLocalStorage<ThemeChoice>('phoebis.theme', 'system');
  const [storedFormat, storeFormat] = useLocalStorage<TimeFormat>('phoebis.timeFormat', '24h');
  const [storedExpert, storeExpert] = useLocalStorage<ExpertChoice>('phoebis.expertView', 'off');
  // Read by equality: the hook hands back whatever string the browser holds under the key.
  const expertView = storedExpert === 'on';
  // The setters keep their identity across renders, so the context value changes with the settings alone.
  const setExpertView = useCallback((on: boolean) => storeExpert(on ? 'on' : 'off'), [storeExpert]);
  const [storedUnit, storeUnit] = useLocalStorage<string>('phoebis.unit', 'native');
  // Read through the positions: the hook hands back whatever string the browser holds under the key.
  const unit = positionOf(storedUnit);
  const setUnit = useCallback((u: UnitPosition) => storeUnit(u), [storeUnit]);
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
    () => ({ expertView, formatVersion, modelTiers, resolvedTheme, setExpertView, setModelTiers, setTheme, setTimeFormat, setUnit, theme, timeFormat: storedFormat, unit }),
    [expertView, formatVersion, modelTiers, resolvedTheme, setExpertView, setModelTiers, setTheme, setTimeFormat, setUnit, storedFormat, theme, unit],
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
