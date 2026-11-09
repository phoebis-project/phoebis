import { useId, useState } from 'react';
import { RadioRow } from '@/components/ui/OptionList';
import { Popover } from '@/components/ui/Popover';
import { Button } from '@/components/ui/primitives';
import { sortAddressOf, sortDirOf, type SortDir, type SortFields, type SortKey } from '@/lib/address';

export interface SortControlProps<K extends string = string> {
  /** Greys the button out with this as its reason and opens nothing, for a view state with no order to pick. */
  disabledTitle?: string;
  keys: readonly SortKey<K>[];
  onChange: (fields: SortFields<K>) => void;
  value: SortFields<K>;
}

const DIRECTIONS: readonly { dir: SortDir; label: string }[] = [
  { dir: 'asc', label: 'Ascending' },
  { dir: 'desc', label: 'Descending' },
];

/** IconArrowRight's stroke, turned up for ascending and down for descending. */
function DirectionArrow({ dir }: { dir: SortDir }): JSX.Element {
  return (
    <svg aria-hidden fill="none" height={14} stroke="currentColor" strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.75} viewBox="0 0 24 24" width={14}>
      <path d="M5 12h14M13 6l6 6-6 6" transform={dir === 'asc' ? 'rotate(-90 12 12)' : 'rotate(90 12 12)'} />
    </svg>
  );
}

/**
 * A 32 px secondary button reading "Sort: {label}" with the direction's arrow, the aria-label "Sort" and the title
 * "Sorted by {label}, {ascending|descending}", its Popover holding the view's keys as radios and, under a rule,
 * Ascending and Descending. A key's pick sends the key in its own default direction, a direction's pick keeps the key.
 * Two states: `disabledTitle` greys the button out under that title, and a key whose `defaultDir` is `none` has no
 * direction to show, so the button draws no arrow, its title names the key alone and both direction rows stand
 * disabled and unchecked.
 */
export function SortControl<K extends string>({ disabledTitle, keys, onChange, value }: SortControlProps<K>): JSX.Element {
  const boxId = useId();
  const headId = useId();
  // Held in state through the ref callback, since the hooks lint refuses a ref read while rendering.
  const [anchor, setAnchor] = useState<HTMLButtonElement | null>(null);
  const [open, setOpen] = useState(false);
  const key = keys.find((k) => k.id === value.sort);
  const label = key?.label ?? value.sort;
  const none = key?.defaultDir === 'none';
  const dir = sortDirOf(keys, value);
  // The pick unmounts the radio that holds the focus, so the focus goes back to the button first, as Add filter's does.
  const pick = (next: SortFields<K>) => {
    anchor?.focus({ preventScroll: true });
    setOpen(false);
    onChange(next);
  };
  return (
    <>
      <Button ref={setAnchor} size="sm" aria-label="Sort" aria-haspopup="dialog" aria-expanded={open} disabled={disabledTitle !== undefined} title={disabledTitle ?? (none ? `Sorted by ${label}` : `Sorted by ${label}, ${dir === 'asc' ? 'ascending' : 'descending'}`)} onClick={() => setOpen((v) => !v)}>
        {`Sort: ${label}`}
        {!none && <DirectionArrow dir={dir} />}
      </Button>
      <Popover anchor={anchor} id={boxId} labelledBy={headId} onClose={() => setOpen(false)} open={open}>
        <span className="sr-only" id={headId}>
          Sort
        </span>
        <div className="sort-popover">
          <div role="radiogroup" aria-labelledby={headId}>
            {keys.map((k) => (
              <RadioRow key={k.id} checked={k.id === value.sort} onClick={() => pick({ dir: '', sort: k.id })}>
                {k.label}
              </RadioRow>
            ))}
          </div>
          <hr className="sort-popover__rule" />
          <div role="radiogroup" aria-label="Direction">
            {DIRECTIONS.map((d) => (
              <RadioRow key={d.dir} checked={!none && d.dir === dir} disabled={none} onClick={() => pick(sortAddressOf(keys, { dir: d.dir, sort: value.sort }))}>
                {d.label}
              </RadioRow>
            ))}
          </div>
        </div>
      </Popover>
    </>
  );
}
