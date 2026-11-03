import { Fragment, type RefObject, useEffect, useId, useLayoutEffect, useRef, useState } from 'react';
import { OptionList, type Option, type Preset, RadioRow, walkRows } from '@/components/ui/OptionList';
import { Popover } from '@/components/ui/Popover';
import { IconChevronLeft, IconPlus } from '@/components/ui/icons';
import { Button, Input } from '@/components/ui/primitives';
import { chipText, type Dimension, emptyValue, hasValue } from '@/lib/filterBar';

export interface AddFilterProps {
  /** The "Add filter" button, for the chip line's focus fallback. */
  buttonRef: RefObject<HTMLButtonElement | null>;
  dimensions: Dimension[];
  /** A dimension whose options did not load, by field, with the failure's words for its title and its description line. */
  disabled?: Record<string, string>;
  /** A chip's click: step two for `field` anchored to the chip. Null while nothing is edited. */
  edit: { anchor: HTMLElement; field: string } | null;
  filters: Record<string, unknown>;
  onEditDone: () => void;
  options: Record<string, Option[]>;
  presets?: Record<string, readonly Preset[]>;
  update: (patch: Record<string, unknown>) => void;
}

/** A list longer than this opens with its search field. */
const SEARCHED = 12;

export const PATTERN_GRAMMAR = '* matches any run of characters, ? one, and a backslash quotes either';

/**
 * A typed list's rows: each checked value no option carries under "Patterns", ahead of the range's own under
 * "In range", so a typed pattern stands as a row that unchecks like any other and reads apart from the values the
 * range holds.
 */
function typedOptions(known: Option[], listed: string[]): Option[] {
  const typed = [...new Set(listed)].filter((v) => !known.some((o) => o.value === v));
  return [...typed.map((v) => ({ group: 'Patterns', label: v, value: v })), ...known.map((o) => ({ ...o, group: 'In range' }))];
}

/** A dimension's value in the chip's words without the dimension, "on" for a flag, and nothing while it holds none. */
function valueWords(dimension: Dimension, value: unknown, options: Option[]): string {
  if (!hasValue(dimension, value)) return '';
  const { values } = dimension.text ? dimension.text(value, options) : chipText(dimension, value, options);
  return values ?? 'on';
}

/** The 32 px secondary button (IconPlus at 14, "Add filter", aria-haspopup dialog, aria-expanded) and its Popover: step one the "Filter by" field over the grouped rows, step two by kind. */
export function AddFilter({ buttonRef, dimensions, disabled = {}, edit, filters, onEditDone, options, presets = {}, update }: AddFilterProps): JSX.Element {
  const boxId = useId();
  const titleId = useId();
  const bodyRef = useRef<HTMLDivElement>(null);
  const [open, setOpen] = useState(false);
  const [anchor, setAnchor] = useState<HTMLElement | null>(null);
  // A box opened from a chip hangs from it, and a write that takes the chip away leaves the focus to the chip line.
  const [fromChip, setFromChip] = useState(false);
  /** The field whose values the box shows, or null for the list of dimensions. */
  const [step, setStep] = useState<string | null>(null);
  const [query, setQuery] = useState('');
  // A pattern or an id is written on Enter or Apply alone, so Escape can drop what was typed.
  const [draft, setDraft] = useState('');
  /**
   * The field whose Select all was unchecked while its empty value means every option. It draws with no check until the
   * next write of that field or the popover's close.
   */
  const [cleared, setCleared] = useState<string | null>(null);
  const textOf = (field: string) => {
    const value = filters[field];
    return typeof value === 'string' ? value : '';
  };
  // A chip's click arrives as a new `edit`, and the box opens on its step in the render that brings it.
  const [seen, setSeen] = useState(edit);
  if (edit !== seen) {
    setSeen(edit);
    if (edit !== null) {
      setOpen(true);
      setAnchor(edit.anchor);
      setFromChip(true);
      setStep(edit.field);
      setQuery('');
      setDraft(textOf(edit.field));
    }
  }
  const dimension = step === null ? undefined : dimensions.find((d) => d.field === step);
  const kind = dimension?.kind ?? null;
  // The box stays open across a step switch, so the new step's own control takes the focus the control that left held.
  // A list with nothing to show falls back to Back, so Escape and the keys still reach the box.
  useEffect(() => {
    const body = bodyRef.current;
    if (!open || body === null) return;
    const own = kind === null ? '.add-filter__search' : kind === 'list' ? '.mselect__search, [role="option"], .add-filter__typed' : kind === 'choice' ? '[role="radio"]' : 'input';
    (body.querySelector<HTMLElement>(own) ?? body.querySelector<HTMLElement>('button'))?.focus({ preventScroll: true });
  }, [kind, open, step]);

  // A list writes on each click with the box open, and the chip it hangs from can leave the layout under it: with its
  // last value, or folded away as the write resets the line's expansion. The box then moves to Add filter and stays
  // open, rather than hang from a chip no longer drawn. A write arrives as new `filters`, so they are a dependency
  // though the body never reads them.
  useLayoutEffect(() => {
    if (!open || !fromChip || anchor === null || (anchor.isConnected && anchor.closest('[hidden]') === null)) return;
    setAnchor(buttonRef.current);
    setFromChip(false);
  }, [anchor, buttonRef, filters, fromChip, open]);

  const close = (to: HTMLElement | null) => {
    to?.focus({ preventScroll: true });
    setOpen(false);
    setQuery('');
    setCleared(null);
    if (edit !== null) onEditDone();
  };
  // A write that empties the dimension takes away the chip the box hangs from, so the chip line moves the focus on.
  // Closing first draws both changes in one commit, where the line finds the focus dropped to the body.
  const write = (d: Dimension, next: unknown) => {
    close(fromChip && !hasValue(d, next) ? null : anchor);
    update({ [d.field]: next });
  };
  const toggle = () => {
    if (open && !fromChip) {
      close(buttonRef.current);
      return;
    }
    setOpen(true);
    setAnchor(buttonRef.current);
    setFromChip(false);
    setStep(null);
    setQuery('');
  };
  const choose = (d: Dimension) => {
    if (disabled[d.field] !== undefined) return;
    if (d.kind === 'flag') {
      write(d, true);
      return;
    }
    setDraft(textOf(d.field));
    setStep(d.field);
  };
  const back = () => {
    setStep(null);
    setQuery('');
  };

  const needle = query.trim().toLowerCase();
  const matching = dimensions.filter((d) => d.label.toLowerCase().includes(needle));
  const headed = new Set(dimensions.map((d) => d.group)).size > 1;
  const list = (
    <div ref={bodyRef} className="add-filter" onKeyDown={(e) => walkRows(e, e.currentTarget)}>
      <Input className="add-filter__search" aria-label="Filter by" placeholder="Filter by" value={query} onChange={(e) => setQuery(e.target.value)} />
      <div role="listbox" aria-label="Filters">
        {matching.length === 0 && <div className="mselect__empty">No matches</div>}
        {matching.map((d, i) => {
          const reason = disabled[d.field];
          const words = valueWords(d, filters[d.field], options[d.field] ?? []);
          return (
            <Fragment key={d.field}>
              {headed && d.group !== matching[i - 1]?.group && (
                <div className="mselect__group" role="presentation">
                  {d.group}
                </div>
              )}
              {/* A disabled row stays a stop for the keys so a reader hears its reason, and the keys that would press it are held. */}
              <button type="button" role="option" className="add-filter__row" aria-disabled={reason !== undefined ? true : undefined} aria-describedby={reason !== undefined ? `${boxId}-${d.field}` : undefined} title={reason} onClick={() => reason === undefined && choose(d)} onKeyDown={(e) => reason !== undefined && (e.key === 'Enter' || e.key === ' ') && e.preventDefault()}>
                <span className="add-filter__label">{d.label}</span>
                {words !== '' && <span className={d.mono ? 'add-filter__value ellipsis mono' : 'add-filter__value ellipsis'}>{words}</span>}
                {reason !== undefined && (
                  <span className="add-filter__description" id={`${boxId}-${d.field}`}>
                    {reason}
                  </span>
                )}
              </button>
            </Fragment>
          );
        })}
      </div>
    </div>
  );
  const values = (d: Dimension) => {
    const value = filters[d.field];
    const known = options[d.field] ?? [];
    const none = d.defaultValue ?? '';
    const listed = Array.isArray(value) ? (value as string[]) : [];
    // An empty value that means every option reads every row checked, so a row's click unchecks one and a preset's
    // narrows to its members. After Select all's uncheck it draws no check, so the uncheck reads as the inverse of the
    // check while the value still means every option.
    const every = d.emptyMeansAll === true && listed.length === 0 && cleared !== d.field;
    const rows = d.typed ? typedOptions(known, listed) : known;
    return (
      <div ref={bodyRef} className="add-filter" onKeyDown={d.kind === 'choice' ? (e) => walkRows(e, e.currentTarget) : undefined}>
        <div className="popover__head">
          <button type="button" className="icon-btn" aria-label="Back to the filter list" onClick={back}>
            <IconChevronLeft size={14} />
          </button>
          <span className="popover__title" id={titleId}>
            {d.label}
          </span>
          {hasValue(d, value) && (
            <button type="button" className="link-btn" onClick={() => write(d, emptyValue(d))}>
              Remove
            </button>
          )}
        </div>
        {d.kind === 'list' && (
          <OptionList
            key={d.field}
            autoFocus
            everyByDefault={every}
            label={d.label}
            mono={d.mono}
            onChange={(next, via) => {
              setCleared((c) => (d.emptyMeansAll === true && via === 'all' && next.length === 0 ? d.field : c === d.field ? null : c));
              update({ [d.field]: next });
            }}
            options={rows}
            presets={presets[d.field]}
            searchable={d.searchable === true || rows.length > SEARCHED}
            value={every ? known.map((o) => o.value) : listed}
          />
        )}
        {d.kind === 'list' && d.typed && (
          <Input
            className="add-filter__typed mono"
            aria-label={`${d.label} pattern`}
            placeholder="Pattern"
            title={PATTERN_GRAMMAR}
            spellCheck={false}
            onKeyDown={(e) => {
              if (e.key !== 'Enter') return;
              e.preventDefault();
              const typed = e.currentTarget.value.trim();
              if (typed === '') return;
              e.currentTarget.value = '';
              if (!listed.includes(typed)) update({ [d.field]: [...listed, typed] });
            }}
          />
        )}
        {(d.kind === 'id' || d.kind === 'pattern') && (
          <>
            <Input
              className="mono"
              aria-labelledby={titleId}
              placeholder={d.label}
              title={d.kind === 'pattern' ? PATTERN_GRAMMAR : undefined}
              spellCheck={false}
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key !== 'Enter') return;
                e.preventDefault();
                write(d, draft.trim());
              }}
            />
            <div className="popover__foot">
              <Button variant="primary" size="sm" onClick={() => write(d, draft.trim())}>
                Apply
              </Button>
            </div>
          </>
        )}
        {d.kind === 'choice' && (
          <div role="radiogroup" aria-labelledby={titleId}>
            {/* An option holding the default value would draw a second any row under the same key. */}
            {[{ label: d.anyLabel ?? '', value: none }, ...known.filter((o) => o.value !== none)].map((o) => {
              const on = o.value === none ? !hasValue(d, value) : value === o.value;
              return (
                <RadioRow key={o.value} checked={on} onClick={() => write(d, o.value)}>
                  {o.label}
                </RadioRow>
              );
            })}
          </div>
        )}
      </div>
    );
  };
  return (
    <>
      {/* React 18's types read a ref to a nullable element as another type, though the two hold the same value. */}
      <Button ref={buttonRef as RefObject<HTMLButtonElement>} size="sm" icon={<IconPlus size={14} />} aria-haspopup="dialog" aria-expanded={open && !fromChip} onClick={toggle}>
        Add filter
      </Button>
      <Popover anchor={anchor} id={boxId} label="Add filter" labelledBy={dimension === undefined ? undefined : titleId} onClose={() => close(null)} open={open} placeKey={step}>
        {dimension === undefined ? list : values(dimension)}
      </Popover>
    </>
  );
}
