import type { ReactNode } from 'react';
import { Link } from 'react-router-dom';
import { type Chip, entityHref } from '@/lib/routingLog';

/**
 * One chip: a value or a key-value pair, the entity it names a link. A popover marks the chip its anchor stands for,
 * and `describedBy` points at the node that carries its hover for a reader.
 */
export function ChipView({ chip, describedBy, marked = false }: { chip: Chip; describedBy?: string; marked?: boolean }) {
  const value = chip.entity ? <Link to={entityHref(chip.entity.kind, chip.entity.id)}>{chip.value}</Link> : chip.href ? <Link to={chip.href}>{chip.value}</Link> : chip.value;
  const label = chip.labelEntity ? <Link to={entityHref(chip.labelEntity.kind, chip.labelEntity.id)}>{chip.label}</Link> : chip.label;
  return (
    <span className={`${chip.mono ? 'chip mono' : 'chip'}${marked ? ' is-marked' : ''}`} title={chip.title} aria-describedby={describedBy}>
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
  if (chips.length === 1 && only.label === undefined && only.title === undefined) {
    if (only.entity) return <Link to={entityHref(only.entity.kind, only.entity.id)}>{only.value}</Link>;
    if (only.href) return <Link to={only.href} className={only.mono ? 'mono' : undefined}>{only.value}</Link>;
    return only.value;
  }
  return <Chips chips={chips} />;
}
