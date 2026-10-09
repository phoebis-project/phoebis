import { type FocusEvent, type KeyboardEvent, type ReactNode, useCallback, useEffect, useLayoutEffect, useRef } from 'react';
import { createPortal } from 'react-dom';
import { ChipView, rowValue } from '@/components/DecisionCard';
import { HashChip } from '@/components/EntryChips';
import { Badge, useDescription } from '@/components/ui/primitives';
import type { StripLayout } from '@/lib/decisionStrip';
import type { Chip } from '@/lib/routingLog';
import type { PlanGroup, PlanRow, PlanTable, PopoverPlan, WindowRow } from '@/lib/stripFacts';

/** The popover's widest box, as the stylesheet bounds it too. */
const MAX_WIDTH = 720;
/** The room the popover keeps from the card's edges, the window's top and bottom, and its anchor. */
const GAP = 8;
const TABBABLE = 'a[href], button:not(:disabled), [tabindex]:not([tabindex="-1"])';
const WINDOW_COLUMNS: { key: keyof WindowRow; label: string; numeric: boolean }[] = [
  { key: 'window', label: 'Window', numeric: false },
  { key: 'used', label: 'Used', numeric: true },
  { key: 'left', label: 'Left', numeric: true },
  { key: 'reset', label: 'Reset', numeric: false },
  { key: 'source', label: 'Source', numeric: false },
  { key: 'age', label: 'Age', numeric: true },
];

/**
 * Describes the element `children` draws by `text` for a reader: through the card's description host where one
 * stands, else through a hidden node after the element, as the attempt trail's is.
 */
export function Described({ children, text }: { children: (describedBy: string | undefined) => ReactNode; text: string | undefined }) {
  const described = useDescription(text);
  return (
    <>
      {children(described?.id)}
      {described?.sibling && (
        <span id={described.id} className="sr-only" data-descriptions="">
          {text}
        </span>
      )}
    </>
  );
}

/** A chip's hover described through the host as a `Badge`'s is. Outside one the title alone describes it. */
function DescribedChip({ chip, marked }: { chip: Chip; marked?: boolean }) {
  const described = useDescription(chip.title);
  // A node beside the chip would join the text of the row or the cell that holds it.
  const hosted = described && !described.sibling ? described.id : undefined;
  return <ChipView chip={chip} describedBy={hosted} marked={marked} />;
}

/** A table cell's hover, described through the host. Outside one the title alone describes it, as the Pace cell's. */
function Cell({ children, head = false, numeric, title }: { children: ReactNode; head?: boolean; numeric: boolean; title: string | undefined }) {
  const described = useDescription(title);
  // A node after the cell would stand in the row, where only cells may.
  const hosted = described && !described.sibling ? described.id : undefined;
  const Tag = head ? 'th' : 'td';
  return (
    <Tag className={numeric ? 'right' : undefined} title={title} aria-describedby={hosted}>
      {children}
    </Tag>
  );
}

function Rows({ rows }: { rows: PlanRow[] }) {
  return (
    <dl className="kv">
      {rows.map((r, i) => (
        <div key={`${r.label}/${i}`} className={r.marked ? 'kv__row is-marked' : 'kv__row'}>
          <dt>{r.label}</dt>
          {/* The node follows the dd inside the row, so the dd's own text stays its chips alone. */}
          <Described text={r.title}>
            {(describedBy) => (
              <dd title={r.title} aria-describedby={describedBy}>
                <span className="chips">
                  {r.entry !== undefined && <HashChip entry={r.entry} />}
                  {r.chips.map((c, j) => (
                    <DescribedChip key={`${c.label ?? ''}/${c.value}/${j}`} chip={c} />
                  ))}
                </span>
              </dd>
            )}
          </Described>
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
              <Cell key={c.key} head numeric={c.numeric} title={c.title}>
                {c.label}
              </Cell>
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
                  <Cell key={c.key} numeric={c.numeric} title={cell?.title}>
                    {cell?.text ?? ''}
                  </Cell>
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
              <DescribedChip key={`${c.label ?? ''}/${i}`} chip={c} />
            ))}
          </span>
          {b.windows.length > 0 && <Windows windows={b.windows} />}
        </>
      );
    case 'chips':
      return (
        <span className="chips">
          {b.chips.map((c, i) => (
            <DescribedChip key={`${c.label ?? ''}/${c.value}/${i}`} chip={c} marked={i === b.marked} />
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
 * anything absolute inside it: no taller than the window less 8 px above and below, scrolling what runs past that,
 * measured at the window's corner, then centered under its anchor 8 px below, kept 8 px inside the part of the card's
 * box on screen, moved above the anchor where it would pass the window's bottom, and kept 8 px inside the window, and
 * hidden until that placement is done. A strip redrawn for a record that no longer holds the anchor takes the anchor's
 * button away, and the popover closes with it. A key that opened it moves focus into it, and focus leaving it for
 * anything but its anchor closes it. Tab past its end moves on to the strip's next button and Shift+Tab before its
 * start back to the anchor.
 */
export function StripPopover({ anchor, bounds, focus, id, layout, onClose, pinned, plan }: { anchor: HTMLElement; bounds: HTMLElement; focus: boolean; id: string; layout: StripLayout; onClose: (returnFocus: boolean) => void; pinned: boolean; plan: PopoverPlan }): JSX.Element {
  const ref = useRef<HTMLDivElement>(null);
  const place = useCallback(() => {
    const el = ref.current;
    if (el === null) return;
    if (!anchor.isConnected) {
      onClose(false);
      return;
    }
    const card = bounds.getBoundingClientRect();
    // A classic scrollbar stands inside the window's inner box but outside the fixed box's containing block.
    const view = document.documentElement.clientWidth;
    const tall = document.documentElement.clientHeight;
    // The log's table scrolls a card wider than the window sideways, so the box keeps to the part of the card on
    // screen.
    const from = Math.max(card.left, 0) + GAP;
    const to = Math.min(card.right, view) - GAP;
    // The bounds are set before the box is read, so a card narrower than the window's bound narrows the box it
    // measures, and a shed round's chip per mark scrolls inside a box no taller than the window.
    el.style.maxWidth = `${Math.min(MAX_WIDTH, Math.max(0, view - 2 * GAP), Math.max(0, card.width - 2 * GAP), Math.max(0, to - from))}px`;
    el.style.maxHeight = `${Math.max(0, tall - 2 * GAP)}px`;
    // A fixed box shrinks to the room right of where it last stood, so it is measured from the window's corner.
    el.style.left = '0px';
    el.style.top = '0px';
    const box = el.getBoundingClientRect();
    const at = anchor.getBoundingClientRect();
    el.style.left = `${Math.max(from, Math.min(at.left + at.width / 2 - box.width / 2, to - box.width))}px`;
    const below = at.bottom + GAP;
    const top = below + box.height > tall - GAP ? at.top - GAP - box.height : below;
    el.style.top = `${Math.max(GAP, Math.min(top, tall - GAP - box.height))}px`;
    el.style.visibility = 'visible';
  }, [anchor, bounds, onClose]);
  // A new plan places it again, and so does a strip redrawn for a new width or a measured label, which moves the
  // anchor it stands under.
  useLayoutEffect(place, [layout, place, plan]);
  // After the placement, so the box takes focus where it stands. A redraw for the same anchor leaves focus where Tab
  // has since put it.
  useEffect(() => {
    if (focus) ref.current?.focus({ preventScroll: true });
  }, [anchor, focus]);
  // A blur naming no element (a click on a button Safari gives no focus, the window losing focus) closes nothing, as a
  // menu's does.
  const leave = (e: FocusEvent<HTMLDivElement>) => {
    const to = e.relatedTarget;
    if (to !== null && !e.currentTarget.contains(to) && !anchor.contains(to)) onClose(false);
  };
  // The box stands at the body's end, after every other stop, so Tab off either end would leave the page or land at
  // its far end. The keys keep the walk in the strip: on past the anchor, or back onto it.
  const tab = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key !== 'Tab') return;
    const root = e.currentTarget;
    const stops = [...root.querySelectorAll<HTMLElement>(TABBABLE)];
    if (e.shiftKey) {
      if (e.target !== root && e.target !== stops[0]) return;
      e.preventDefault();
      anchor.focus({ preventScroll: true });
      return;
    }
    if (e.target !== (stops.length > 0 ? stops[stops.length - 1] : root)) return;
    e.preventDefault();
    let next = anchor.nextElementSibling;
    while (next !== null && next.tagName !== 'BUTTON') next = next.nextElementSibling;
    // The anchor stands for the next button when it is the strip's last.
    ((next as HTMLElement | null) ?? anchor).focus();
    onClose(false);
  };
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
    // Hidden until `place` has put it under its anchor, so a frame painted before then shows nothing. React writes a
    // style key only when its value changes between renders, so the reveal `place` writes on the element outlives a
    // re-render.
    <div ref={ref} className={`strip-popover strip-popover--${plan.renderer}${pinned ? ' is-pinned' : ''}`} id={id} role="dialog" aria-modal="false" aria-labelledby={headId} tabIndex={-1} style={{ position: 'fixed', visibility: 'hidden' }} onBlur={leave} onKeyDown={tab}>
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
