import { useEffect, useState } from 'react';
import { useAppSettings } from '@/app/settings';
import { readChartInk, type ChartInk } from './theme';

export function useChartInk(): ChartInk {
  const { resolvedTheme } = useAppSettings();
  const [ink, setInk] = useState<ChartInk>(() => readChartInk());
  useEffect(() => {
    // The attribute flips in an effect upstream; read after paint so the new
    // variables have been applied.
    const id = requestAnimationFrame(() => setInk(readChartInk()));
    return () => cancelAnimationFrame(id);
  }, [resolvedTheme]);
  return ink;
}
