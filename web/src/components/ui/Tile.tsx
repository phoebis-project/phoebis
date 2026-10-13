import type { ReactNode } from 'react';
import { Skeleton } from './primitives';

/**
 * A compact stat tile. A null value or a null note draws a placeholder; an absent note draws no line; a failed read
 * draws a dash with its message in the tile's title. A text note draws in the note's type; any other node draws as it
 * is, so a tile can carry markup of its own, such as a bar or a list, in its own type. `title` is the tile's hover
 * text, such as the estimated sentence of a marked figure, and a failed read's message takes its place.
 */
export function Tile({ error, label, note, title, tone, value }: { error?: string | null; label: string; note?: ReactNode | null; title?: string; tone?: 'critical' | 'warning'; value: ReactNode }) {
  return (
    <div className="stat stat--compact" title={error ?? title}>
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

/**
 * Two figures stacked in a tile's value, each followed by its word, for a site that sums rows of both fundings. A site
 * with one side's rows draws that line alone, so its word still says which funding the figure is.
 */
export function PairValue({ first, firstLabel, second, secondLabel = '' }: { first: ReactNode; firstLabel: string; second?: ReactNode; secondLabel?: string }) {
  return (
    <span className="stat__pair">
      <span className="stat__pair-line">
        {first}
        <span className="stat__pair-word">{firstLabel}</span>
      </span>
      {second !== undefined && (
        <span className="stat__pair-line">
          {second}
          <span className="stat__pair-word">{secondLabel}</span>
        </span>
      )}
    </span>
  );
}
