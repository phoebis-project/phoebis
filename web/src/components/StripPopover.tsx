import { useCallback, useEffect, useLayoutEffect, useRef } from 'react';
import { createPortal } from 'react-dom';
import { ChipView, rowValue } from '@/components/DecisionCard';
import { HashChip } from '@/components/EntryChips';
import { Badge } from '@/components/ui/primitives';
import type { PlanGroup, PlanRow, PlanTable, PopoverPlan, WindowRow } from '@/lib/stripFacts';

/** The popover's widest box, as the stylesheet bounds it too. */
const MAX_WIDTH = 720;
/** The room the popover keeps from the card's edges, the window's bottom and its anchor. */
const GAP = 8;
const WINDOW_COLUMNS: { key: keyof WindowRow; label: string; numeric: boolean }[] = [
  { key: 'window', label: 'Window', numeric: false },
  { key: 'used', label: 'Used', numeric: true },
  { key: 'left', label: 'Left', numeric: true },
  { key: 'reset', label: 'Reset', numeric: false },
  { key: 'source', label: 'Source', numeric: false },
  { key: 'age', label: 'Age', numeric: true },
];

function Rows({ rows }: { rows: PlanRow[] }) {
  return (
    <dl className="kv">
      {rows.map((r, i) => (
        <div key={`${r.label}/${i}`} className={r.marked ? 'kv__row is-marked' : 'kv__row'}>
          <dt>{r.label}</dt>
          <dd title={r.title}>
            <span className="chips">
              {r.entry !== undefined && <HashChip entry={r.entry} />}
              {r.chips.map((c, j) => (
                <ChipView key={`${c.label ?? ''}/${c.value}/${j}`} chip={c} />
              ))}
            </span>
          </dd>
        </div>
      ))}
    </dl>
  );
}

function Groups({ groups }: { groups: PlanGroup[] }) {
  return (
    <div className="strip-popover__groups">
      {groups.map((g, i) => (
        <section key={`${g.label ?? ''}/${i}`} className="strip-popover__group">
          {g.label !== null && <h5>{g.label}</h5>}
          <Rows rows={g.rows} />
        </section>
      ))}
    </div>
  );
}

function Table({ table }: { table: PlanTable }) {
  return (
    <div className="table-wrap strip-popover__table">
      <table className="table">
        <thead>
          <tr>
            <th>Account</th>
            {table.columns.map((c) => (
              <th key={c.key} className={c.numeric ? 'right' : undefined} title={c.title}>
                {c.label}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {table.rows.map((r) => (
            <tr key={r.account.value}>
              <td>{rowValue([r.account])}</td>
              {table.columns.map((c) => {
                const cell = r.cells[c.key];
                return (
                  <td key={c.key} className={c.numeric ? 'right' : undefined} title={cell?.title}>
                    {cell?.text ?? ''}
                  </td>
                );
              })}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function Windows({ windows }: { windows: WindowRow[] }) {
  return (
    <div className="table-wrap strip-popover__table">
      <table className="table">
        <thead>
          <tr>
            {WINDOW_COLUMNS.map((c) => (
              <th key={c.key} className={c.numeric ? 'right' : undefined}>
                {c.label}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {windows.map((w) => (
            <tr key={w.window}>
              {WINDOW_COLUMNS.map((c) => (
                <td key={c.key} className={c.numeric ? 'right' : undefined}>
                  {w[c.key]}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function Block({ plan }: { plan: PopoverPlan }) {
  const b = plan.block;
  switch (b.kind) {
    case 'candidate':
      return (
        <>
          <span className="chips">
            {b.chips.map((c, i) => (
              <ChipView key={`${c.label ?? ''}/${i}`} chip={c} />
            ))}
          </span>
          {b.windows.length > 0 && <Windows windows={b.windows} />}
        </>
      );
    case 'chips':
      return (
        <span className="chips">
          {b.chips.map((c, i) => (
            <ChipView key={`${c.label ?? ''}/${c.value}/${i}`} chip={c} marked={i === b.marked} />
          ))}
        </span>
      );
    case 'groups':
      return <Groups groups={b.groups} />;
    case 'sentence':
      return <p className="strip-popover__sentence">{b.text}</p>;
    case 'table':
      return (
        <>
          {b.table.loose.length > 0 && <Rows rows={b.table.loose} />}
          {b.table.rows.length > 0 && <Table table={b.table} />}
        </>
      );
  }
}

/**
 * One station's, leaf's or terminus's facts over the card. A fixed box in a portal, since the log's table wrap clips
 * anything absolute inside it: measured at the window's corner, then centered under its anchor 8 px below, kept 8 px
 * inside the part of the card's box on screen, and moved above the anchor where it would pass the window's bottom. A
 * strip redrawn for a record that no longer holds the anchor takes the anchor's button away, and the popover closes
 * with it.
 */
export function StripPopover({ anchor, bounds, id, onClose, pinned, plan }: { anchor: HTMLElement; bounds: HTMLElement; id: string; onClose: () => void; pinned: boolean; plan: PopoverPlan }): JSX.Element {
  const ref = useRef<HTMLDivElement>(null);
  const place = useCallback(() => {
    const el = ref.current;
    if (el === null) return;
    if (!anchor.isConnected) {
      onClose();
      return;
    }
    const card = bounds.getBoundingClientRect();
    // The log's table scrolls a card wider than the window sideways, so the box keeps to the part of the card on screen.
    const from = Math.max(card.left, 0) + GAP;
    const to = Math.min(card.right, window.innerWidth) - GAP;
    // The bound is set before the box is read, so a card narrower than the window's bound narrows the box it measures.
    el.style.maxWidth = `${Math.min(MAX_WIDTH, window.innerWidth - 2 * GAP, Math.max(0, card.width - 2 * GAP), Math.max(0, to - from))}px`;
    // A fixed box shrinks to the room right of where it last stood, so it is measured from the window's corner.
    el.style.left = '0px';
    el.style.top = '0px';
    const box = el.getBoundingClientRect();
    const at = anchor.getBoundingClientRect();
    el.style.left = `${Math.max(from, Math.min(at.left + at.width / 2 - box.width / 2, to - box.width))}px`;
    const below = at.bottom + GAP;
    el.style.top = `${below + box.height > window.innerHeight - GAP ? Math.max(GAP, at.top - GAP - box.height) : below}px`;
  }, [anchor, bounds, onClose]);
  // Every draw places it again, so a strip redrawn for a new width carries it along with its anchor.
  useLayoutEffect(place, [place, plan]);
  useEffect(() => {
    window.addEventListener('resize', place);
    window.addEventListener('scroll', place, true);
    return () => {
      window.removeEventListener('resize', place);
      window.removeEventListener('scroll', place, true);
    };
  }, [place]);
  const headId = `${id}-head`;
  const { head } = plan;
  return createPortal(
    <div ref={ref} className={`strip-popover strip-popover--${plan.renderer}${pinned ? ' is-pinned' : ''}`} id={id} role="dialog" aria-modal="false" aria-labelledby={headId} tabIndex={-1} style={{ position: 'fixed' }}>
      <div className="strip-popover__head">
        <span className="strip-popover__title" id={headId}>
          {head.account !== null ? rowValue([head.account]) : head.word}
        </span>
        {head.badge !== null && <Badge tone={head.tone}>{head.badge}</Badge>}
        {head.second.length > 0 && (
          <span className="strip-popover__second">
            {head.second.map((part, i) => (
              <span key={i}>{part}</span>
            ))}
          </span>
        )}
        {head.chip !== null && (
          <Badge tone={head.tone} title={head.chip.title}>
            {head.chip.value}
          </Badge>
        )}
      </div>
      <Block plan={plan} />
    </div>,
    anchor.closest('dialog[open]') ?? document.body,
  );
}
