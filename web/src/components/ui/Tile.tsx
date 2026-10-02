import { Skeleton } from './primitives';

/**
 * A compact stat tile. A null value or a null note draws a placeholder; an absent note draws no line; a failed read
 * draws a dash with its message in the tile's title.
 */
export function Tile({ error, label, note, tone, value }: { error?: string | null; label: string; note?: string | null; tone?: 'critical' | 'warning'; value: string | null }) {
  return (
    <div className="stat stat--compact" title={error ?? undefined}>
      <span className="stat__label">{label}</span>
      <span className={`stat__value ${tone ? `is-${tone}` : ''}`}>{error ? '-' : (value ?? <Skeleton width={60} height={22} />)}</span>
      {error ? <span className="stat__note">could not load</span> : note === undefined ? null : <span className="stat__note">{note ?? <Skeleton width={90} />}</span>}
    </div>
  );
}
