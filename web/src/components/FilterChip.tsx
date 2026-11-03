import { useState } from 'react';
import { IconX } from '@/components/ui/icons';
import type { Chip } from '@/lib/filterBar';

/**
 * `span.chip.chip--filter.chip--removable[title]` holding `button.chip__body` (the word, a colon, the values, 240 px at
 * most with an ellipsis, `aria-haspopup="dialog"` with `aria-expanded` while the body is `editing`, the element the Add
 * filter box opened from) and a `button[aria-label="Remove {label} filter"]` with IconX at 12. A button holds no other
 * button, so the two stand side by side in the pill. `hidden` keeps a chip the line folds drawn but out of view.
 */
export function FilterChip({ chip, editing = null, hidden, onOpen, onRemove }: { chip: Chip; editing?: HTMLElement | null; hidden?: boolean; onOpen: (anchor: HTMLElement) => void; onRemove: () => void }): JSX.Element {
  const { values } = chip;
  // Held in state through the ref callback, since the hooks lint refuses a ref read while rendering.
  const [body, setBody] = useState<HTMLButtonElement | null>(null);
  return (
    <span className="chip chip--filter chip--removable" title={chip.title} hidden={hidden}>
      <button ref={setBody} type="button" className="chip__body" aria-haspopup="dialog" aria-expanded={body !== null && editing === body} onClick={(e) => onOpen(e.currentTarget)}>
        <span className="chip__word">{values === null ? chip.text : `${chip.label}:`}</span>
        {values !== null && (
          <>
            {/* The gap draws the space, and the text keeps it for a reader and a copy. */}{' '}
            <span className={chip.mono ? 'chip__values ellipsis mono' : 'chip__values ellipsis'}>{values}</span>
          </>
        )}
      </button>
      <button type="button" className="chip__remove" aria-label={`Remove ${chip.label} filter`} onClick={onRemove}>
        <IconX size={12} />
      </button>
    </span>
  );
}
