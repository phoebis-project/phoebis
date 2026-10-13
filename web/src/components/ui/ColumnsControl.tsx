import { useId, useState } from 'react';
import { IconColumns } from './icons';
import { Popover } from './Popover';
import { Button } from './primitives';

/**
 * A 32 px secondary button with IconColumns at 14 and the count of shown columns as its text, the title "Columns:
 * {shown} of {all} shown" and the aria-label "Columns", its checkbox grid in a Popover. The popover is portalled, so
 * the container needs nothing of its own.
 */
export function ColumnsControl<C extends string>({ columns, onChange, visible }: { columns: readonly { id: C; label: string }[]; onChange: (next: Set<C>) => void; visible: ReadonlySet<C> }): JSX.Element {
  const boxId = useId();
  const headId = useId();
  // Held in state through the ref callback, since the hooks lint refuses a ref read while rendering.
  const [anchor, setAnchor] = useState<HTMLButtonElement | null>(null);
  const [open, setOpen] = useState(false);
  return (
    <>
      <Button ref={setAnchor} size="sm" icon={<IconColumns size={14} />} aria-label="Columns" aria-haspopup="dialog" aria-expanded={open} title={`Columns: ${visible.size} of ${columns.length} shown`} onClick={() => setOpen((v) => !v)}>
        {visible.size}
      </Button>
      <Popover anchor={anchor} id={boxId} labelledBy={headId} onClose={() => setOpen(false)} open={open}>
        <span className="sr-only" id={headId}>
          Columns
        </span>
        <div className="cols-popover">
          {columns.map((c) => (
            <label key={c.id}>
              <input
                type="checkbox"
                checked={visible.has(c.id)}
                onChange={(e) => {
                  const next = new Set(visible);
                  if (e.target.checked) next.add(c.id);
                  else next.delete(c.id);
                  onChange(next);
                }}
              />
              {c.label}
            </label>
          ))}
        </div>
      </Popover>
    </>
  );
}
