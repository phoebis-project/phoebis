import { Skeleton } from './primitives';

/**
 * A request's Cost cell from its built cell: the skeleton while the scale is out, a dash titled with the reading's
 * error, else the main line over its second, the full reading on the title.
 */
export function CostTd({ cell, className, error }: { cell: { main: string; sub: string; title: string } | null; className?: string; error: string | null }) {
  const classes = className ? `right num col-fit ${className}` : 'right num col-fit';
  if (cell === null)
    return error === null ? (
      <td className={classes}>
        <Skeleton width={72} />
      </td>
    ) : (
      <td className={classes} title={error}>
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
