import { useState } from 'react';
import { IconColumns } from './icons';
import { Button } from './primitives';

/**
 * The `Columns (N)` button and its checkbox popover. It draws no box of its own, so the popover anchors to the page's
 * container, which needs `position: relative` as `.filters__cols` has.
 */
export function ColumnsControl<C extends string>({ columns, onChange, visible }: { columns: readonly { id: C; label: string }[]; onChange: (next: Set<C>) => void; visible: ReadonlySet<C> }) {
  const [open, setOpen] = useState(false);
  return (
    <>
      <Button size="sm" icon={<IconColumns size={14} />} onClick={() => setOpen((v) => !v)} aria-expanded={open}>
        Columns ({visible.size})
      </Button>
      {open && (
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
      )}
    </>
  );
}
