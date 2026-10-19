import { Fragment, type KeyboardEvent, useEffect, useMemo, useRef, useState } from 'react';
import { IconCheck } from './icons';

export interface Option {
  /** The heading the option is listed under. The options of one group are adjacent. */
  group?: string;
  label: string;
  value: string;
}

/** A named set of options a single row turns on or off together. */
export interface Preset {
  label: string;
  members: readonly string[];
}

export interface OptionListProps {
  /** Focus the search field, else the first row, on mount. */
  autoFocus?: boolean;
  /** The value is every option because none is chosen, so a preset row's click replaces it with the preset's members. */
  everyByDefault?: boolean;
  /** The search field's placeholder reads `Search ${label.toLowerCase()}`. */
  label: string;
  mono?: boolean;
  /**
   * `via` reads `all` on Select all's click alone. A caller reads it to tell the uncheck that empties the list from a
   * row's click.
   */
  onChange: (next: string[], via?: 'all') => void;
  options: Option[];
  /** Rows under a "Presets" heading after "Select all" and before the first group: checked while every member is on, mixed while some are, a click adds the members of an unchecked or mixed row and removes a checked row's. The search filters them by label. */
  presets?: readonly Preset[];
  searchable?: boolean;
  value: string[];
}

// A disabled row is a stop for the keys, so its reason reaches a reader who walks the list, and the row's own handler
// keeps it inert.
const ROWS = '[role="option"], [role="radio"]';

/**
 * The list's keys: ArrowDown and ArrowUp walk the list's field and its rows, wrapping at either end, and Home and End
 * jump to the first and last row from a row. Home and End stay with the field, where they move the caret.
 */
export function walkRows(e: KeyboardEvent<HTMLElement>, list: HTMLElement): void {
  const search = list.querySelector<HTMLInputElement>('input');
  const choices = [...list.querySelectorAll<HTMLElement>(ROWS)];
  const stops: HTMLElement[] = search ? [search, ...choices] : choices;
  const at = stops.indexOf(e.target as HTMLElement);
  const onChoice = choices.includes(e.target as HTMLElement);
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
}

/** The multi-select's body: `div.option-list[role=listbox][aria-multiselectable]`, the `.mselect__search` field, "No matches", the `.mselect__option--all` row with its three states, the `.mselect__group` headings and the `.mselect__option` rows, ArrowDown, ArrowUp, Home and End walking the field and the rows. Escape is the opener's. */
export function OptionList({ autoFocus = false, everyByDefault = false, label, mono = false, onChange, options, presets = [], searchable = false, value }: OptionListProps): JSX.Element {
  const [query, setQuery] = useState('');
  const ref = useRef<HTMLDivElement>(null);
  // Focus enters the list as it opens, so the list's keys reach it.
  useEffect(() => {
    if (autoFocus) ref.current?.querySelector<HTMLElement>('.mselect__search, [role="option"]')?.focus();
  }, [autoFocus]);
  const needle = query.toLowerCase();
  const visible = useMemo(() => options.filter((o) => o.label.toLowerCase().includes(needle)), [options, needle]);
  const shownPresets = presets.filter((p) => p.label.toLowerCase().includes(needle));
  const toggle = (v: string) => onChange(value.includes(v) ? value.filter((x) => x !== v) : [...value, v]);
  const visibleValues = visible.map((o) => o.value);
  const allOn = visible.length > 0 && visibleValues.every((v) => value.includes(v));
  const someOn = visibleValues.some((v) => value.includes(v));
  // The row reaches the options the search shows, so a query narrows what it selects and the hidden ones stay as
  // they are.
  const toggleAll = () => onChange(allOn ? value.filter((v) => !visibleValues.includes(v)) : [...value, ...visibleValues.filter((v) => !value.includes(v))], 'all');
  const togglePreset = (members: readonly string[]) =>
    onChange(everyByDefault ? [...members] : members.every((m) => value.includes(m)) ? value.filter((v) => !members.includes(v)) : [...value, ...members.filter((m) => !value.includes(m))]);
  return (
    <div className="option-list" role="listbox" aria-multiselectable ref={ref} onKeyDown={(e) => walkRows(e, e.currentTarget)}>
      {searchable && <input className="input mselect__search" placeholder={`Search ${label.toLowerCase()}`} value={query} onChange={(e) => setQuery(e.target.value)} />}
      {visible.length === 0 && shownPresets.length === 0 && <div className="mselect__empty">No matches</div>}
      {visible.length > 0 && (
        <button
          type="button"
          role="option"
          aria-selected={allOn}
          aria-checked={someOn && !allOn ? 'mixed' : undefined}
          className={`mselect__option mselect__option--all ${allOn ? 'is-on' : someOn ? 'is-mixed' : ''}`}
          onClick={toggleAll}
        >
          <span className="mselect__check">{allOn ? <IconCheck size={14} /> : someOn ? <span className="mselect__mixed" aria-hidden="true" /> : null}</span>
          Select all
        </button>
      )}
      {shownPresets.length > 0 && (
        <div className="mselect__group" role="presentation">
          Presets
        </div>
      )}
      {shownPresets.map((p) => {
        const on = p.members.every((m) => value.includes(m));
        const some = !on && p.members.some((m) => value.includes(m));
        return (
          <button
            key={p.label}
            type="button"
            role="option"
            aria-selected={on}
            aria-checked={some ? 'mixed' : undefined}
            className={`mselect__option mselect__option--preset ${on ? 'is-on' : some ? 'is-mixed' : ''}`}
            onClick={() => togglePreset(p.members)}
          >
            <span className="mselect__check">{on ? <IconCheck size={14} /> : some ? <span className="mselect__mixed" aria-hidden="true" /> : null}</span>
            {p.label}
          </button>
        );
      })}
      {visible.map((o, i) => {
        const on = value.includes(o.value);
        return (
          <Fragment key={o.value}>
            {o.group !== undefined && o.group !== visible[i - 1]?.group && (
              <div className="mselect__group" role="presentation">
                {o.group}
              </div>
            )}
            <button type="button" role="option" aria-selected={on} className={`mselect__option ${on ? 'is-on' : ''} ${mono ? 'mono' : ''}`} onClick={() => toggle(o.value)}>
              <span className="mselect__check">{on && <IconCheck size={14} />}</span>
              {o.label}
            </button>
          </Fragment>
        );
      })}
    </div>
  );
}
