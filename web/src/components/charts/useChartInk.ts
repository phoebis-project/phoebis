import { createContext, createElement, type ReactNode, useContext, useEffect, useState } from 'react';
import { useAppSettings } from '@/app/settings';
import { readChartInk, type ChartInk } from './theme';

const ChartInkContext = createContext<ChartInk | null>(null);

/** One read of the theme's chart colors for every chart under it, where each chart would read the properties itself. */
export function ChartInkProvider({ children }: { children: ReactNode }) {
  const { resolvedTheme } = useAppSettings();
  const [ink, setInk] = useState<ChartInk>(() => readChartInk());
  useEffect(() => {
    // The attribute flips in an effect upstream, so the read waits for paint, when the new variables apply. A read that
    // matches the held ink keeps it, so the mount's own read draws the charts no second time. Both reads come from one
    // literal, so their keys run in one order and the two texts match exactly when every color does.
    const id = requestAnimationFrame(() => {
      const next = readChartInk();
      setInk((was) => (JSON.stringify(was) === JSON.stringify(next) ? was : next));
    });
    return () => cancelAnimationFrame(id);
  }, [resolvedTheme]);
  return createElement(ChartInkContext.Provider, { value: ink }, children);
}

/** The shell's chart colors, or a fresh read for a chart drawn outside the shell, as the UI harness draws one. */
export function useChartInk(): ChartInk {
  return useContext(ChartInkContext) ?? readChartInk();
}
