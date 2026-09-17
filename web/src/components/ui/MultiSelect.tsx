import { useEffect, useMemo, useRef, useState } from 'react';
import { IconCheck, IconChevron, IconX } from './icons';

export interface Option {
  label: string;
  value: string;
}

export function MultiSelect({ label, options, value, onChange, searchable = false, mono = false }: { label: string; mono?: boolean; onChange: (v: string[]) => void; options: Option[]; searchable?: boolean; value: string[] }) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState('');
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener('mousedown', onDoc);
    return () => document.removeEventListener('mousedown', onDoc);
  }, [open]);
  const visible = useMemo(() => options.filter((o) => o.label.toLowerCase().includes(query.toLowerCase())), [options, query]);
  const toggle = (v: string) => onChange(value.includes(v) ? value.filter((x) => x !== v) : [...value, v]);
  const summary = value.length === 0 ? label : `${label} (${value.length})`;
  return (
    <div className={`mselect ${value.length ? 'has-value' : ''}`} ref={ref}>
      <button type="button" className="mselect__trigger" aria-haspopup="listbox" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
        <span>{summary}</span>
        {value.length > 0 ? (
          <span
            role="button"
            tabIndex={0}
            className="mselect__clear"
            aria-label={`Clear ${label}`}
            onClick={(e) => {
              e.stopPropagation();
              onChange([]);
            }}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.stopPropagation();
                onChange([]);
              }
            }}
          >
            <IconX size={12} />
          </span>
        ) : (
          <IconChevron size={14} />
        )}
      </button>
      {open && (
        <div className="mselect__list" role="listbox" aria-multiselectable>
          {searchable && <input className="input mselect__search" placeholder={`Search ${label.toLowerCase()}`} value={query} onChange={(e) => setQuery(e.target.value)} autoFocus />}
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
    </div>
  );
}
