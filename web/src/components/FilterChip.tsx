import { IconX } from '@/components/ui/icons';
import type { Chip } from '@/lib/filterBar';

/**
 * `span.chip.chip--filter.chip--removable[title]` holding `button.chip__body` (the word, a colon, the values, 240 px at
 * most with an ellipsis) and a `button[aria-label="Remove {label} filter"]` with IconX at 12. A button holds no other
 * button, so the two stand side by side in the pill. `hidden` keeps a chip the line folds drawn but out of view.
 */
export function FilterChip({ chip, hidden, onOpen, onRemove }: { chip: Chip; hidden?: boolean; onOpen: (anchor: HTMLElement) => void; onRemove: () => void }): JSX.Element {
  const { values } = chip;
  return (
    <span className="chip chip--filter chip--removable" title={chip.title} hidden={hidden}>
      <button type="button" className="chip__body" onClick={(e) => onOpen(e.currentTarget)}>
        <span className="chip__word">{values === null ? chip.text : `${chip.label}:`}</span>
        {values !== null && (
          <>
            {/* The gap draws the space, and the text keeps it for a reader and a copy. */}{' '}
            <span className={chip.mono ? 'chip__values mono' : 'chip__values'}>{values}</span>
          </>
        )}
      </button>
      <button type="button" className="chip__remove" aria-label={`Remove ${chip.label} filter`} onClick={onRemove}>
        <IconX size={12} />
      </button>
    </span>
  );
}
