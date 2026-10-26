import type { Reconciliation } from '@/api/types';
import { Badge } from '@/components/ui/primitives';
import { ratioOff } from '@/lib/analytics';
import { judged, UNJUDGED_TITLE } from '@/lib/meter';

/**
 * A completed window's ratio at two decimals with the four on hover: the warning badge where the band marks it, muted
 * with the reason where the band does not judge it, so an unbadged ratio is not read as one the band passed.
 */
export function MeterRatio({ row }: { row: Pick<Reconciliation, 'observedRise' | 'predictedShare' | 'ratio'> }) {
  const four = row.ratio.toFixed(4);
  if (ratioOff(row)) {
    return (
      <Badge tone="warning" title={four}>
        {row.ratio.toFixed(2)}
      </Badge>
    );
  }
  return judged(row) ? (
    <span className="num" title={four}>
      {row.ratio.toFixed(2)}
    </span>
  ) : (
    <span className="num muted" title={`${four}, ${UNJUDGED_TITLE}`}>
      {row.ratio.toFixed(2)}
    </span>
  );
}
