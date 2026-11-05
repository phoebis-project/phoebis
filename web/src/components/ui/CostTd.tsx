import type { WeekReading } from '@/lib/analytics';
import { Skeleton } from './primitives';

/**
 * A request's Cost cell from its built cell: the skeleton while the scale is out, a dash titled with the reading's
 * failure, else the main line over its second, the full reading on the title.
 */
export function CostTd({ cell, className, failure }: { cell: { main: string; sub: string; title: string } | null; className?: string; failure: WeekReading['failure'] }) {
  const classes = className ? `right num col-fit ${className}` : 'right num col-fit';
  if (cell === null)
    return failure === null ? (
      <td className={classes}>
        <Skeleton width={72} />
      </td>
    ) : (
      <td className={classes} title={failure.text}>
        -
      </td>
    );
  return (
    <td className={classes} title={cell.title || undefined}>
      <CostLines cell={cell} />
    </td>
  );
}

export function CostLines({ cell }: { cell: { main: string; sub: string } }) {
  return (
    <>
      {cell.main}
      {cell.sub && <span className="sub">{cell.sub}</span>}
    </>
  );
}
