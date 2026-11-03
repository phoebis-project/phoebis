import type { ReactNode } from 'react';
import { Link } from 'react-router-dom';
import { type Chip, entityHref } from '@/lib/routingLog';

/** Where a chip's value links: the entity it names, else the address a session or a request chip carries. */
export const chipHref = (c: Chip): string | undefined => (c.entity ? entityHref(c.entity.kind, c.entity.id) : c.href);

/**
 * The value, a link where the chip resolves one. `face` is the class of a value drawn outside a chip's own span: inside
 * one the span carries the face, and `.mono` inside `.mono` would size the text down twice.
 */
function linked(c: Chip, face?: string): ReactNode {
  const href = chipHref(c);
  if (href) return <Link to={href} className={face}>{c.value}</Link>;
  return face === undefined ? c.value : <span className={face}>{c.value}</span>;
}

/** A chip's value drawn on its own, as a row's one value or a word of a sentence, in the mono face where it asks. */
export const chipValue = (c: Chip): ReactNode => linked(c, c.mono ? 'mono' : undefined);

/**
 * One chip: a value or a key-value pair, the entity it names a link. A popover marks the chip its anchor stands for,
 * and `describedBy` points at the node that carries its hover for a reader.
 */
export function ChipView({ chip, className, describedBy, marked = false }: { chip: Chip; className?: string; describedBy?: string; marked?: boolean }) {
  const value = linked(chip);
  const label = chip.labelEntity ? <Link to={entityHref(chip.labelEntity.kind, chip.labelEntity.id)}>{chip.label}</Link> : chip.label;
  return (
    <span className={`${chip.mono ? 'chip mono' : 'chip'}${marked ? ' is-marked' : ''}${className ? ` ${className}` : ''}`} title={chip.title} aria-describedby={describedBy}>
      {label !== undefined && <span className="chip__label">{label}</span>}
      {value}
    </span>
  );
}

export function Chips({ chips }: { chips: Chip[] }) {
  return (
    <span className="chips">
      {chips.map((c, i) => (
        <ChipView key={`${c.label ?? ''}/${c.value}/${i}`} chip={c} />
      ))}
    </span>
  );
}

/** A row's value: one plain value or one entity reads as itself, anything more as a row of chips. */
export function rowValue(chips: Chip[]): ReactNode {
  const [only] = chips;
  if (chips.length === 1 && only.label === undefined && only.title === undefined) return chipValue(only);
  return <Chips chips={chips} />;
}
