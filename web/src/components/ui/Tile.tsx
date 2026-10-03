import type { ReactNode } from 'react';
import { Skeleton } from './primitives';

/**
 * A compact stat tile. A null value or a null note draws a placeholder; an absent note draws no line; a failed read
 * draws a dash with its message in the tile's title. A text note draws in the note's type; any other node draws as it
 * is, so a tile can carry markup of its own, such as a bar or a list, in its own type.
 */
export function Tile({ error, label, note, tone, value }: { error?: string | null; label: string; note?: ReactNode | null; tone?: 'critical' | 'warning'; value: string | null }) {
  return (
    <div className="stat stat--compact" title={error ?? undefined}>
      <span className="stat__label">{label}</span>
      <span className={`stat__value ${tone ? `is-${tone}` : ''}`}>{error ? '-' : (value ?? <Skeleton width={60} height={22} />)}</span>
      {error ? (
        <span className="stat__note">could not load</span>
      ) : note === undefined ? null : note === null ? (
        <span className="stat__note">
          <Skeleton width={90} />
        </span>
      ) : typeof note === 'string' || typeof note === 'number' ? (
        <span className="stat__note">{note}</span>
      ) : (
        note
      )}
    </div>
  );
}
