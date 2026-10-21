import { type KeyboardEvent, useId, useState } from 'react';
import { walkIndex } from '@/components/ui/OptionList';
import { Popover } from '@/components/ui/Popover';
import { IconCheck, IconChevron, IconRefresh } from '@/components/ui/icons';
import { PAUSE_CAUSE, type PauseCause, RELOAD_INTERVALS } from '@/lib/follow';
import { useDocumentHidden } from '@/lib/hooks';

export interface RefreshControlProps {
  /** An ended custom span: the part reads Off, disabled, with this title. */
  disabled?: string;
  intervalMs: number;
  onIntervalChange: (ms: number) => void;
  onRefresh: () => void;
  /** A phrase the part's title takes after a comma while off (Analytics: "the window ends 14:30"). */
  note?: string;
  /** The page's cause while auto reload waits, or null. */
  paused: PauseCause | null;
  /** The body's title: "Rows up to 14:30. Refresh reads up to now." */
  refreshTitle: string;
}

/** The menu's keys, as the Menu primitive walks its items: the arrows wrap, Home and End jump to either end. */
function walk(e: KeyboardEvent<HTMLButtonElement>) {
  const items = [...(e.currentTarget.parentElement?.querySelectorAll<HTMLElement>('[role="menuitemradio"]') ?? [])];
  const i = walkIndex(e.key, items.length, items.indexOf(e.currentTarget));
  const to = i === null ? undefined : items[i];
  if (!to) return;
  e.preventDefault();
  to.focus();
}

/**
 * One 32 px box: `button.refresh-control__body[aria-label="Refresh"]` with IconRefresh at 14, a hairline,
 * `button.refresh-control__part[aria-haspopup="menu"][aria-label="Auto reload interval"]` reading the interval's word
 * before IconChevron at 12, as wide as "1 min" in every state, its menu a Popover of `button[role="menuitemradio"]`
 * rows. `data-state` on the root reads "off" at 0, "on" at an interval with no pause, and "paused" at an interval with
 * a pause or a hidden tab.
 */
export function RefreshControl({ disabled, intervalMs, note, onIntervalChange, onRefresh, paused, refreshTitle }: RefreshControlProps): JSX.Element {
  const menuId = useId();
  // Held in state through the ref callback, since the hooks lint refuses a ref read while rendering.
  const [anchor, setAnchor] = useState<HTMLButtonElement | null>(null);
  const [open, setOpen] = useState(false);
  // A span that ends while the menu is open leaves nothing to choose, so the menu closes with the part disabled.
  if (open && disabled !== undefined) setOpen(false);
  const hidden = useDocumentHidden();
  const ms = disabled === undefined ? intervalMs : 0;
  const word = RELOAD_INTERVALS.find((i) => i.ms === ms)?.label ?? 'Off';
  const cause = paused ?? (hidden ? 'hidden' : null);
  const state = ms === 0 ? 'off' : cause === null ? 'on' : 'paused';
  const title = disabled ?? (ms === 0 ? (note === undefined ? 'Auto reload off' : `Auto reload off, ${note}`) : cause === null ? `Auto reload every ${word}` : `Auto reload every ${word}, ${PAUSE_CAUSE[cause]}`);
  const choose = (next: number) => {
    anchor?.focus({ preventScroll: true });
    setOpen(false);
    onIntervalChange(next);
  };

  return (
    <div className="refresh-control" data-state={state}>
      <button type="button" className="refresh-control__body" aria-label="Refresh" title={refreshTitle} onClick={onRefresh}>
        <IconRefresh size={14} />
      </button>
      <button ref={setAnchor} type="button" className="refresh-control__part" aria-label="Auto reload interval" aria-haspopup="menu" aria-expanded={open} title={title} disabled={disabled !== undefined} onClick={() => setOpen((o) => !o)}>
        {/* The widest word sits under the shown one in one grid cell, so the part keeps its width as the word changes. */}
        <span className="refresh-control__word">
          <span className="refresh-control__sizer" aria-hidden="true">
            1 min
          </span>
          <span className="refresh-control__shown">{word}</span>
        </span>
        <IconChevron size={12} strokeWidth={1.5} />
      </button>
      <Popover anchor={anchor} id={menuId} label="Auto reload interval" onClose={() => setOpen(false)} open={open} role="menu">
        {RELOAD_INTERVALS.map((i) => (
          <button key={i.ms} type="button" role="menuitemradio" aria-checked={i.ms === ms} className="mselect__option" onClick={() => choose(i.ms)} onKeyDown={walk}>
            <span className="mselect__check">{i.ms === ms && <IconCheck size={14} />}</span>
            {i.label}
          </button>
        ))}
      </Popover>
    </div>
  );
}
