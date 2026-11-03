import { IconChevronLeft, IconChevronRight, IconChevronsLeft } from './icons';

/** A log's First, Previous and Next page buttons, the first two off on the first page and Next off on the last. */
export function PagerNav({ atFirst, atLast, onFirst, onNext, onPrevious }: { atFirst: boolean; atLast: boolean; onFirst: () => void; onNext: () => void; onPrevious: () => void }) {
  return (
    <div className="pager__nav">
      <button type="button" onClick={onFirst} disabled={atFirst} aria-label="First page">
        <IconChevronsLeft size={15} />
      </button>
      <button type="button" onClick={onPrevious} disabled={atFirst} aria-label="Previous page">
        <IconChevronLeft size={15} />
      </button>
      <button type="button" onClick={onNext} disabled={atLast} aria-label="Next page">
        <IconChevronRight size={15} />
      </button>
    </div>
  );
}
