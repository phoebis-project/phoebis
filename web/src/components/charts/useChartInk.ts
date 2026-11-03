import { useEffect, useState } from 'react';
import { useAppSettings } from '@/app/settings';
import { readChartInk, sameInk, type ChartInk } from './theme';

export function useChartInk(): ChartInk {
  const { resolvedTheme } = useAppSettings();
  const [ink, setInk] = useState<ChartInk>(() => readChartInk());
  useEffect(() => {
    // The attribute flips in an effect upstream, so the read waits for paint, when the new variables apply. A read that
    // matches the held ink keeps it, so the mount's own read draws the component no second time.
    const id = requestAnimationFrame(() => {
      const next = readChartInk();
      setInk((was) => (sameInk(was, next) ? was : next));
    });
    return () => cancelAnimationFrame(id);
  }, [resolvedTheme]);
  return ink;
}
