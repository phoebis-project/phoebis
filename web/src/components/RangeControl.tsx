import { useId, useState } from 'react';
import { iso } from '@/api/decode';
import { Popover } from '@/components/ui/Popover';
import { Button, Field, Input } from '@/components/ui/primitives';
import { RANGE_PRESETS, type RangeValue } from '@/lib/address';
import { fmtAbsolute, spanText, toLocalInput } from '@/lib/format';

export interface RangeControlProps {
  /** The clock, read while the popover is open and at Apply, so a typed end is capped at the instant the span is written rather than at the page's load. */
  now: () => number;
  onChange: (next: RangeValue) => void;
  value: RangeValue;
  /** The window the view shows: the end the group's title names under a preset, and the span the Custom popover opens on. */
  window: { from: number; to: number };
}

type Side = 'from' | 'to';

/**
 * Six 32 px pills, 1h, 6h, 24h, 7d, 30d and Custom, the pressed one the address's preset or Custom while a span stands,
 * printing the span. The group's title reads "Last 24 hours, ends {fmtAbsolute}" under a preset and "{from} to {to}"
 * in fmtAbsolute under a span. It draws `Segmented`'s markup itself, since the Custom pill anchors a popover and the
 * primitive's buttons take no ref.
 */
export function RangeControl({ now, onChange, value, window: shown }: RangeControlProps): JSX.Element {
  const boxId = useId();
  const titleId = useId();
  // Held in state through the ref callback, since the hooks lint refuses a ref read while rendering.
  const [anchor, setAnchor] = useState<HTMLButtonElement | null>(null);
  // The window the popover opened on, so the view's window moving on while it is open changes no untouched field.
  const [edit, setEdit] = useState<{ base: { from: number; to: number }; from: string; to: string } | null>(null);
  const custom = value.preset === 'custom';
  const pair = `${fmtAbsolute(iso(shown.from))} to ${fmtAbsolute(iso(shown.to))}`;
  const label = RANGE_PRESETS.find((p) => p.name === value.preset)?.label ?? value.preset;

  // A field's minute drops the window's seconds and milliseconds, so an untouched field keeps the window's own instant.
  const bound = (side: Side) => (edit === null ? Number.NaN : edit[side] === toLocalInput(edit.base[side]) ? edit.base[side] : Date.parse(edit[side]));
  // The end is capped before the pair is judged, so two dates after the clock read as a pair that ends before it starts.
  const capped = (at: number) => ({ from: bound('from'), to: Math.min(bound('to'), at) });
  const shownPair = edit === null ? null : capped(now());
  const invalid = shownPair !== null && !(shownPair.from < shownPair.to);

  const choose = (name: string) => {
    setEdit(null);
    if (name !== value.preset) onChange({ from: '', preset: name, to: '' });
  };
  const toggle = () => setEdit(edit !== null ? null : { base: { from: shown.from, to: shown.to }, from: toLocalInput(shown.from), to: toLocalInput(shown.to) });
  const close = () => {
    anchor?.focus({ preventScroll: true });
    setEdit(null);
  };
  const apply = () => {
    const next = capped(now());
    if (!(next.from < next.to)) return;
    close();
    onChange({ from: iso(next.from), preset: 'custom', to: iso(next.to) });
  };
  const type = (side: Side, text: string) => setEdit((e) => (e === null ? e : { ...e, [side]: text }));

  return (
    <>
      <div className="segmented" role="group" aria-label="Range" title={custom ? pair : `${label}, ends ${fmtAbsolute(iso(shown.to))}`}>
        {RANGE_PRESETS.map((p) => (
          <button key={p.name} type="button" className={p.name === value.preset ? 'is-active' : ''} aria-label={p.label} aria-pressed={p.name === value.preset} onClick={() => choose(p.name)}>
            {p.name}
          </button>
        ))}
        <button ref={setAnchor} type="button" className={custom ? 'is-active' : ''} aria-pressed={custom} aria-haspopup="dialog" aria-expanded={edit !== null} title={custom ? pair : undefined} onClick={toggle}>
          {custom ? spanText(shown.from, shown.to) : 'Custom'}
        </button>
      </div>
      <Popover anchor={anchor} id={boxId} labelledBy={titleId} onClose={() => setEdit(null)} open={edit !== null}>
        <div className="popover__head">
          <span className="popover__title" id={titleId}>
            Custom range
          </span>
        </div>
        <div className="range-control__fields">
          <Field label="From">
            <Input type="datetime-local" aria-label="From" value={edit?.from ?? ''} onChange={(e) => type('from', e.target.value)} />
          </Field>
          <Field label="To">
            <Input type="datetime-local" aria-label="To" max={toLocalInput(now())} value={edit?.to ?? ''} onChange={(e) => type('to', e.target.value)} />
          </Field>
          {invalid && <span className="secondary">End must be after start.</span>}
        </div>
        <div className="popover__foot">
          <Button size="sm" onClick={close}>
            Cancel
          </Button>
          <Button variant="primary" size="sm" disabled={invalid} onClick={apply}>
            Apply
          </Button>
        </div>
      </Popover>
    </>
  );
}
