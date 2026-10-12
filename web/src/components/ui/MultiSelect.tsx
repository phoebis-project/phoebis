import { useEffect, useId, useRef, useState, type FocusEvent, type KeyboardEvent } from 'react';
import { IconChevron, IconX } from './icons';
import { type Option, OptionList, walkRows } from './OptionList';

export type { Option } from './OptionList';

/**
 * `disabled` holds the trigger shut, for a filter whose options did not load; `description` says why, in the trigger's
 * title for a pointer and in a hidden node the trigger points at for a screen reader. A filter whose default checks
 * some options passes `active` false at that default, so the trigger reads unset and offers no Clear, and `summary` in
 * place of the label and its count.
 */
export function MultiSelect({ label, options, value, onChange, searchable = false, mono = false, disabled = false, description, active = value.length > 0, summary }: { active?: boolean; description?: string; disabled?: boolean; label: string; mono?: boolean; onChange: (v: string[]) => void; options: Option[]; searchable?: boolean; summary?: string; value: string[] }) {
  const [open, setOpen] = useState(false);
  const descriptionId = useId();
  const ref = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener('mousedown', onDoc);
    return () => document.removeEventListener('mousedown', onDoc);
  }, [open]);
  const shown = summary ?? (value.length === 0 ? label : `${label} (${value.length})`);
  // The keys act only while the list is open, so an Escape on a closed filter still reaches the page, whose drawer
  // closes on it.
  const onKeys = (e: KeyboardEvent<HTMLDivElement>) => {
    const list = listRef.current;
    if (!open || !list) return;
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      triggerRef.current?.focus();
      setOpen(false);
      return;
    }
    // The list walks the keys pressed inside it, and an arrow on the trigger or the clear button steps into it.
    if (!list.contains(e.target as Node)) walkRows(e, list);
  };
  // Safari, and Firefox on macOS, give a clicked button no focus, so a click on an option blurs the list naming no
  // element; a blur closes the list only when it names an element outside the control.
  const closeOnLeave = (e: FocusEvent<HTMLDivElement>) => {
    if (e.relatedTarget && !ref.current?.contains(e.relatedTarget)) setOpen(false);
  };
  return (
    <div className={`mselect ${active ? 'has-value' : ''}`} ref={ref} onBlur={closeOnLeave} onKeyDown={onKeys}>
      <button
        type="button"
        ref={triggerRef}
        className="mselect__trigger"
        aria-haspopup="listbox"
        aria-expanded={open}
        disabled={disabled}
        title={description}
        aria-describedby={description ? descriptionId : undefined}
        onClick={() => setOpen((v) => !v)}
      >
        <span>{shown}</span>
        {!active && <IconChevron size={14} />}
      </button>
      {active && (
        <button
          type="button"
          className="mselect__clear"
          aria-label={`Clear ${label}`}
          onClick={() => {
            // The button leaves as the value empties, which would drop the focus on it to the body.
            triggerRef.current?.focus();
            onChange([]);
          }}
        >
          <IconX size={12} />
        </button>
      )}
      {open && (
        <div className="mselect__list" ref={listRef}>
          <OptionList autoFocus label={label} mono={mono} onChange={onChange} options={options} searchable={searchable} value={value} />
        </div>
      )}
      {/* Last, so the clear button stays the trigger's next sibling. */}
      {description && (
        <span id={descriptionId} className="sr-only">
          {description}
        </span>
      )}
    </div>
  );
}
