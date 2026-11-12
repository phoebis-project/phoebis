import { speedWord } from '@/lib/selection';
import { Badge } from './primitives';

/**
 * `0.5x` in the warning tone with the hover of `speedHover`, nothing at null or at a speed of 1 or more. The figure is
 * the speed the holds realized, which reads 1 on a slowed account until a pin there has a cycle, and the chip draws
 * under 1 alone.
 */
export function SpeedChip({ hover, speed }: { hover: string | null; speed: number | null }) {
  if (speed === null || speed >= 1) return null;
  // The wrapper lets a check pick the chip out of, or leave it out of, the badges beside it.
  return (
    <span className="speed-chip">
      <Badge tone="warning" title={hover ?? undefined}>
        {speedWord(speed)}
      </Badge>
    </span>
  );
}
