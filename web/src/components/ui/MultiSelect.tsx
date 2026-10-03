import { useEffect, useId, useMemo, useRef, useState, type FocusEvent, type KeyboardEvent } from 'react';
import { IconCheck, IconChevron, IconX } from './icons';

export interface Option {
  label: string;
  value: string;
}

/**
 * `disabled` holds the trigger shut, for a filter whose options did not load; `description` says why, in the trigger's
 * title for a pointer and in a hidden node the trigger points at for a screen reader.
 */
export function MultiSelect({ label, options, value, onChange, searchable = false, mono = false, disabled = false, description }: { description?: string; disabled?: boolean; label: string; mono?: boolean; onChange: (v: string[]) => void; options: Option[]; searchable?: boolean; value: string[] }) {
  const [open, setOpen] = useState(false);
  const descriptionId = useId();
  const [query, setQuery] = useState('');
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
  // Focus enters the list as it opens, so the list's keys reach it.
  useEffect(() => {
    if (open) listRef.current?.querySelector<HTMLElement>('.mselect__search, [role="option"]')?.focus();
  }, [open]);
  const visible = useMemo(() => options.filter((o) => o.label.toLowerCase().includes(query.toLowerCase())), [options, query]);
  const toggle = (v: string) => onChange(value.includes(v) ? value.filter((x) => x !== v) : [...value, v]);
  const summary = value.length === 0 ? label : `${label} (${value.length})`;
  // The keys act only while the list is open, so an Escape on a closed filter still reaches the page, whose drawer
  // closes on it. Home and End stay with the search field, where they move the caret.
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
    const search = list.querySelector<HTMLInputElement>('.mselect__search');
    const choices = [...list.querySelectorAll<HTMLButtonElement>('[role="option"]')];
    const stops: HTMLElement[] = search ? [search, ...choices] : choices;
    const at = stops.indexOf(e.target as HTMLElement);
    const onChoice = choices.includes(e.target as HTMLButtonElement);
    const to =
      e.key === 'ArrowDown'
        ? stops[(at + 1) % stops.length]
        : e.key === 'ArrowUp'
          ? stops[(at <= 0 ? stops.length : at) - 1]
          : e.key === 'Home' && onChoice
            ? choices[0]
            : e.key === 'End' && onChoice
              ? choices[choices.length - 1]
              : undefined;
    if (!to) return;
    e.preventDefault();
    to.focus();
  };
  // Safari, and Firefox on macOS, give a clicked button no focus, so a click on an option blurs the list naming no
  // element; a blur closes the list only when it names an element outside the control.
  const closeOnLeave = (e: FocusEvent<HTMLDivElement>) => {
    if (e.relatedTarget && !ref.current?.contains(e.relatedTarget)) setOpen(false);
  };
  return (
    <div className={`mselect ${value.length ? 'has-value' : ''}`} ref={ref} onBlur={closeOnLeave} onKeyDown={onKeys}>
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
        <span>{summary}</span>
        {value.length === 0 && <IconChevron size={14} />}
      </button>
      {value.length > 0 && (
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
        <div className="mselect__list" role="listbox" aria-multiselectable ref={listRef}>
          {searchable && <input className="input mselect__search" placeholder={`Search ${label.toLowerCase()}`} value={query} onChange={(e) => setQuery(e.target.value)} />}
          {visible.length === 0 && <div className="mselect__empty">No matches</div>}
          {visible.map((o) => {
            const on = value.includes(o.value);
            return (
              <button key={o.value} type="button" role="option" aria-selected={on} className={`mselect__option ${on ? 'is-on' : ''} ${mono ? 'mono' : ''}`} onClick={() => toggle(o.value)}>
                <span className="mselect__check">{on && <IconCheck size={14} />}</span>
                {o.label}
              </button>
            );
          })}
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
