import { Fragment, type KeyboardEvent, type ReactNode, useEffect, useMemo, useRef, useState } from 'react';
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
 * The one key map of the menus and the lists: the index a key moves to from `at` among `count` rows, the arrows
 * wrapping at either end and Home and End jumping to the first and the last row. Null for a key the walk leaves alone.
 */
function walkIndex(key: string, count: number, at: number): number | null {
  switch (key) {
    case 'ArrowDown':
      return (at + 1) % count;
    case 'ArrowUp':
      return (at <= 0 ? count : at) - 1;
    case 'Home':
      return 0;
    case 'End':
      return count - 1;
    default:
      return null;
  }
}

/** Moves the focus where `walkIndex` sends the key from the event's target among `items`, claiming the key, and does
 *  nothing for a key it does not map. */
export function walkFocus(e: KeyboardEvent<HTMLElement>, items: readonly HTMLElement[]): void {
  const i = walkIndex(e.key, items.length, items.indexOf(e.target as HTMLElement));
  const to = i === null ? undefined : items[i];
  if (!to) return;
  e.preventDefault();
  to.focus();
}

/**
 * The list's keys: ArrowDown and ArrowUp walk the list's field and its rows, wrapping at either end, and Home and End
 * jump to the first and last row from a row. Home and End stay with the field, where they move the caret.
 */
export function walkRows(e: KeyboardEvent<HTMLElement>, list: HTMLElement): void {
  const search = list.querySelector<HTMLInputElement>('input');
  const choices = [...list.querySelectorAll<HTMLElement>(ROWS)];
  const stops: HTMLElement[] = search ? [search, ...choices] : choices;
  const onChoice = choices.includes(e.target as HTMLElement);
  // Home and End move the caret in the field, so off a row they are the field's.
  if ((e.key === 'Home' || e.key === 'End') && !onChoice) return;
  const walked = e.key === 'Home' || e.key === 'End' ? choices : stops;
  walkFocus(e, walked);
}

/** A row that selects a set of options: on with every one, mixed with some, its mark a check or a dash. */
function TriStateRow({ children, mixed, modifier, on, onClick }: { children: ReactNode; mixed: boolean; modifier: 'all' | 'preset'; on: boolean; onClick: () => void }) {
  return (
    <button
      type="button"
      role="option"
      aria-selected={on}
      aria-checked={mixed ? 'mixed' : undefined}
      className={`mselect__option mselect__option--${modifier} ${on ? 'is-on' : mixed ? 'is-mixed' : ''}`}
      onClick={onClick}
    >
      <span className="mselect__check">{on ? <IconCheck size={14} /> : mixed ? <span className="mselect__mixed" aria-hidden="true" /> : null}</span>
      {children}
    </button>
  );
}

/** A one-choice row with the lists' check mark: a radio row `ROWS` walks, or a menu's `menuitemradio` row. */
export function RadioRow({ checked, children, className = 'add-filter__row', onClick, onKeyDown, role = 'radio' }: { checked: boolean; children: ReactNode; className?: string; onClick: () => void; onKeyDown?: (e: KeyboardEvent<HTMLButtonElement>) => void; role?: 'menuitemradio' | 'radio' }) {
  return (
    <button type="button" role={role} aria-checked={checked} className={className} onClick={onClick} onKeyDown={onKeyDown}>
      <span className="mselect__check">{checked && <IconCheck size={14} />}</span>
      {children}
    </button>
  );
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
        <TriStateRow mixed={someOn && !allOn} modifier="all" on={allOn} onClick={toggleAll}>
          Select all
        </TriStateRow>
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
          <TriStateRow key={p.label} mixed={some} modifier="preset" on={on} onClick={() => togglePreset(p.members)}>
            {p.label}
          </TriStateRow>
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
