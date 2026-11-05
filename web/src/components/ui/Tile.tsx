import type { ReactNode } from 'react';
import { dollarsUnder, nativeFiguresOf, nativeSidesOf, UNPRICED_TITLE, type CostFigure, type FundedSum, type WeekReading } from '@/lib/analytics';
import { fmtUsd } from '@/lib/format';
import type { UnitPosition } from '@/lib/unit';
import { Skeleton } from './primitives';

/**
 * A compact stat tile. A null value or a null note draws a placeholder, an absent note draws no line, and a failure
 * draws a dash with its text in the tile's title. A text note draws in the note's type, and any other node draws as it
 * is, so a tile can carry markup of its own, such as a bar or a list, in its own type. `title` is the tile's hover
 * text, such as the unpriced title where a plan row priced nothing, and a failure's text takes its place. A failure
 * that is a week reading's unpriced sentence, which no retry clears, notes the dash unpriced, not could not load.
 */
export function Tile({ failure = null, label, note, title, tone, value }: { failure?: WeekReading['failure']; label: string; note?: ReactNode | null; title?: string; tone?: 'critical' | 'warning'; value: ReactNode }) {
  return (
    <div className="stat stat--compact" title={failure?.text ?? title}>
      <span className="stat__label">{label}</span>
      <span className={`stat__value ${tone ? `is-${tone}` : ''}`}>{failure ? '-' : (value ?? <Skeleton width={60} height={22} />)}</span>
      {failure ? (
        <span className="stat__note">{failure.unpriced ? 'unpriced' : 'could not load'}</span>
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

/**
 * A tile's figures: two stacked as a pair, each with its funding's word, and a side alone on its one line with its
 * word, so the tile still says which funding it reads. A figure that carries no word, a dollar position's, reads alone
 * in the tile's own type, and no figure reads a dash.
 */
export function figuresValue(figures: CostFigure[]): ReactNode {
  const figure = (f: CostFigure) => <span className="cost-figure">{f.text}</span>;
  if (figures.length === 2) return <PairValue first={figure(figures[0])} firstLabel={figures[0].word ?? ''} second={figure(figures[1])} secondLabel={figures[1].word ?? ''} />;
  const [only] = figures;
  if (only === undefined) return '-';
  if (only.word === undefined) return only.text;
  return <PairValue first={figure(only)} firstLabel={only.word} />;
}

/**
 * A tile over a sum under a position. Native stacks the two sides with their words in the chosen week, the
 * subscription side a dash under `UNPRICED_TITLE` where a plan row priced nothing, and an empty sum reads its zero in
 * each unit rather than nothing. A null value is the skeleton native draws while the scale is out, and `failure` the
 * reading's failure there, which a tile draws as its dash.
 */
export function sumTileOf(unit: UnitPosition, s: FundedSum, reading: WeekReading): { failure: WeekReading['failure']; title?: string; value: ReactNode | null } {
  if (unit !== 'native') return { failure: null, value: fmtUsd(dollarsUnder(unit, s)) };
  if (reading.scale === null) return { failure: reading.failure, value: null };
  const native = nativeFiguresOf(nativeSidesOf(s), reading.week, reading.scale);
  return { failure: null, title: native.unpriced ? UNPRICED_TITLE : undefined, value: figuresValue(native.figures) };
}
