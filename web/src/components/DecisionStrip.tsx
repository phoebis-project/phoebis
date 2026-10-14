import { type FocusEvent, type KeyboardEvent, useCallback, useEffect, useId, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore } from 'react';
import type { DecisionRecord, DecisionSummary } from '@/api/types';
import { ChipView } from '@/components/ChipView';
import { StripPopover } from '@/components/StripPopover';
import { Badge, Described } from '@/components/ui/primitives';
import { clock } from '@/lib/clock';
import { BRACKET_TICK, BRACKET_WORD_RISE, type LabelHeights, layoutStrip, LEAF_HALF, leafName, type OpenPopover, popoverStore, RADIUS, stationModel, stationName, type StripAnchor, TERMINUS_RADIUS, terminusName, type PathState } from '@/lib/decisionStrip';
import { anchorPlan } from '@/lib/stripFacts';

const MIN_WIDTH = 640;
const REDRAW_MS = 120;
// A stub runs 10 px across and 16 px up from its station, or down on the renewal, and its word starts past its end.
const STUB_WORD_X = 12;
const STUB_WORD_ABOVE = 30;
const STUB_WORD_BELOW = 12;
// Under a station whose label sits below the line the word starts past that label's own word ("Renew"), which the
// stub's end would put it across.
const STUB_WORD_PAST_LABEL = 24;
const TERMINUS_WORD_X = 14;
// A terminus word sits level with its node: centered on the badge's 22 px box, or on its own 16 px line when stacked.
const TERMINUS_ROW_HALF = 11;
const TERMINUS_LINE_HALF = 8;
// The idle paths share their first run with the taken one, so the taken path is drawn last to stay on top.
const PAINT_ORDER: Record<PathState, number> = { idle: 0, inferred: 1, taken: 2 };

/** A strip's own popover, as the store holds it less the strip's id. */
type Own = Omit<OpenPopover, 'stripId'>;

const keyOf = (a: StripAnchor) => (a.kind === 'leaf' ? `leaf:${a.index}` : `${a.kind}:${a.id}`);
const sameHeights = (a: LabelHeights, b: LabelHeights) => Object.keys(a).length === Object.keys(b).length && Object.entries(a).every(([id, h]) => b[id] === h);
const classes = (...names: (string | false)[]) => names.filter(Boolean).join(' ');
/** The popover keeps inside the card the strip stands in, or inside the strip where it stands alone. */
const boundsOf = (el: HTMLElement) => el.closest<HTMLElement>('.decision-card') ?? el.closest<HTMLElement>('.decision-strip') ?? el;

/**
 * The walk as a subway strip: the model from the record, the layout from the slot's width (at least 640 px, re-read on
 * a resize debounced at 120 ms), one SVG for the lines and a button per station, leaf and terminus, each opening the
 * popover of its facts. One popover is open on the page: a hover opens it, a click pins it and marks its anchor, and a
 * click outside, a click on another anchor or Escape closes it. Enter or Space pins it and moves focus into it, Escape
 * from there returns focus to the anchor, and focus leaving both closes it.
 */
export function DecisionStrip({ now, record, row }: { now: number; record: DecisionRecord; row: DecisionSummary }): JSX.Element {
  const ref = useRef<HTMLDivElement>(null);
  const id = useId();
  const popoverId = `${id}-popover`;
  const [width, setWidth] = useState(MIN_WIDTH);
  useLayoutEffect(() => {
    const el = ref.current;
    if (el === null) return undefined;
    const read = () => setWidth(Math.max(MIN_WIDTH, el.clientWidth));
    read();
    let timer: number | null = null;
    const later = () => {
      if (timer !== null) clock.clearTimeout(timer);
      timer = clock.setTimeout(read, REDRAW_MS);
    };
    const cancel = () => {
      if (timer !== null) clock.clearTimeout(timer);
    };
    if (typeof ResizeObserver === 'function') {
      const observer = new ResizeObserver(later);
      observer.observe(el);
      return () => {
        observer.disconnect();
        cancel();
      };
    }
    window.addEventListener('resize', later);
    return () => {
      window.removeEventListener('resize', later);
      cancel();
    };
  }, []);
  const model = useMemo(() => stationModel(record, now), [record, now]);
  const counted = useMemo(() => layoutStrip(model, width), [model, width]);
  const [measured, setMeasured] = useState<LabelHeights>({});
  const layout = useMemo(() => (Object.keys(measured).length > 0 ? layoutStrip(model, width, measured) : counted), [counted, measured, model, width]);
  // A label whose words wrapped stands taller than the layout counts, which only the drawn box tells, so every draw
  // reads the labels and lays the strip out once more with the heights that differ. jsdom's empty boxes read as no
  // height.
  useLayoutEffect(() => {
    const el = ref.current;
    if (el === null) return;
    const next: Record<string, number> = {};
    for (const button of el.querySelectorAll<HTMLElement>('.strip-station')) {
      const id = button.dataset.station;
      const height = Math.round(button.getBoundingClientRect().height);
      if (id !== undefined && height > 0 && height !== counted.labels[id]?.height) next[id] = height;
    }
    // eslint-disable-next-line react-hooks/set-state-in-effect -- the drawn heights exist only once the strip is drawn, so the layout reads them before paint
    if (!sameHeights(next, measured)) setMeasured(next);
  }, [counted, measured]);
  // `focus` is true when a key opened the popover, which then takes focus. A hover or a click leaves focus where it is.
  // The page holds one popover across its strips, so this strip's is the store's while the store names this strip, and
  // a hover or a pin here replaces another strip's whatever its state.
  const all = useSyncExternalStore(popoverStore.subscribe, popoverStore.get);
  const open = all !== null && all.stripId === id ? all : null;
  const setOpen = useCallback(
    (update: Own | null | ((o: Own | null) => Own | null)) =>
      popoverStore.set((o) => {
        const mine = o !== null && o.stripId === id ? o : null;
        const next = typeof update === 'function' ? update(mine) : update;
        // A close here leaves another strip's popover open, and an unchanged one keeps the store as it is.
        if (next === mine) return o;
        if (next === null) return null;
        // A hover and the click that pins it name one anchor by two objects, so the store keeps the first, and the
        // plan below is read again for a new anchor alone.
        const anchor = mine !== null && keyOf(mine.anchor) === keyOf(next.anchor) ? mine.anchor : next.anchor;
        return { ...next, anchor, stripId: id };
      }),
    [id],
  );
  useEffect(() => () => popoverStore.set((o) => (o !== null && o.stripId === id ? null : o)), [id]);
  const buttons = useRef(new Map<string, HTMLButtonElement>());
  // The anchor's button from the last draw: one the new draw leaves out closes its popover as it leaves the document.
  // eslint-disable-next-line react-hooks/refs -- the buttons register through their ref callbacks, so the last draw's map is the one place that holds them
  const anchorEl = open === null ? undefined : buttons.current.get(keyOf(open.anchor));
  // The button is looked up when the close runs, so a pin that keeps the anchor keeps this callback and the popover's
  // placement.
  const close = useCallback(
    (returnFocus: boolean) => {
      const own = popoverStore.get();
      if (returnFocus && own !== null && own.stripId === id) buttons.current.get(keyOf(own.anchor))?.focus({ preventScroll: true });
      setOpen(null);
    },
    [id, setOpen],
  );
  const anchorRef = (anchor: StripAnchor) => (el: HTMLButtonElement | null) => {
    if (el === null) buttons.current.delete(keyOf(anchor));
    else buttons.current.set(keyOf(anchor), el);
  };
  // A hover opens a popover while none is pinned. A click pins it, or closes the one its anchor already pinned.
  const enter = (anchor: StripAnchor) => () => setOpen((o) => (o?.pinned ? o : { anchor, focus: false, pinned: false }));
  const leave = (anchor: StripAnchor) => () => setOpen((o) => (o !== null && !o.pinned && keyOf(o.anchor) === keyOf(anchor) ? null : o));
  const pin = (anchor: StripAnchor, focus: boolean) => setOpen((o) => (o?.pinned && keyOf(o.anchor) === keyOf(anchor) ? null : { anchor, focus, pinned: true }));
  // The key's own click is cancelled, or it would pin a second time and close the popover again.
  const keyDown = (anchor: StripAnchor) => (e: KeyboardEvent<HTMLButtonElement>) => {
    if (e.key !== 'Enter' && e.key !== ' ') return;
    e.preventDefault();
    pin(anchor, true);
  };
  // Engines differ on which of Space's two events fires a button's click, so its release is cancelled too.
  const keyUp = (e: KeyboardEvent<HTMLButtonElement>) => {
    if (e.key === ' ') e.preventDefault();
  };
  // Focus leaving an open popover's anchor for anything but the popover closes it, as a menu closes on a blur outside
  // its list and its trigger. A blur naming no element (a click on a button Safari gives no focus) closes nothing.
  const blurred = (anchor: StripAnchor) => (e: FocusEvent<HTMLButtonElement>) => {
    const to = e.relatedTarget;
    if (to === null || e.currentTarget.contains(to) || document.getElementById(popoverId)?.contains(to)) return;
    setOpen((o) => (o !== null && keyOf(o.anchor) === keyOf(anchor) ? null : o));
  };
  const handlers = (anchor: StripAnchor) => ({ onBlur: blurred(anchor), onClick: () => pin(anchor, false), onKeyDown: keyDown(anchor), onKeyUp: keyUp, onMouseEnter: enter(anchor), onMouseLeave: leave(anchor), ref: anchorRef(anchor) });
  const pinned = open?.pinned ? open.anchor : null;
  useEffect(() => {
    if (pinned === null) return undefined;
    const onOutside = (e: MouseEvent) => {
      const target = e.target as Node;
      const anchors = [...buttons.current.values()];
      if (!document.getElementById(popoverId)?.contains(target) && !anchors.some((b) => b.contains(target))) setOpen(null);
    };
    document.addEventListener('mousedown', onOutside);
    return () => document.removeEventListener('mousedown', onOutside);
  }, [pinned, popoverId, setOpen]);
  // Captured on the document ahead of every other handler, so a key from the strip or its popover closes the popover
  // alone: a dialog around the card never takes it too. A key from a control outside both is that control's, so the
  // popover a hover opened over it closes and the key runs on, and a filter's open list closes on the same press. Focus
  // returns to the anchor from the popover or the anchor alone: elsewhere it stays where it was.
  useEffect(() => {
    if (open === null) return undefined;
    const onEscape = (e: globalThis.KeyboardEvent) => {
      if (e.key !== 'Escape') return;
      const at = e.target as Node;
      const inPopover = Boolean(document.getElementById(popoverId)?.contains(at));
      if (inPopover || ref.current?.contains(at)) {
        e.preventDefault();
        e.stopPropagation();
      }
      close(inPopover || Boolean(buttons.current.get(keyOf(open.anchor))?.contains(at)));
    };
    document.addEventListener('keydown', onEscape, true);
    return () => document.removeEventListener('keydown', onEscape, true);
  }, [close, open, popoverId]);
  const picked = pinned === null ? null : keyOf(pinned);
  // Only a pinned popover marks its anchor expanded: one a hover opened closes as the pointer leaves, out of a reader's
  // reach.
  const expanded = (anchor: StripAnchor) => (picked === keyOf(anchor) ? { 'aria-controls': popoverId, 'aria-expanded': true } : {});
  // The line's stations left to right, then the track's, so Tab walks the strip in the record's order.
  const stations = [...model.stations.filter((s) => s.level === 'line'), ...model.stations.filter((s) => s.level === 'track')];
  const termini = model.termini.flatMap((t) => {
    const p = layout.termini[t.kind];
    return t.drawn && p !== undefined ? [{ p, t }] : [];
  });
  // A move's continuation leaves its terminus a narrower margin than a line does, so the word stacks over the badge.
  const stacked = model.branch?.kind === 'move';
  const opened = open?.anchor;
  const openedKey = opened === undefined ? null : keyOf(opened);
  // The popover places itself again for a new plan or a redrawn strip alone.
  const plan = useMemo(() => (opened === undefined ? null : anchorPlan(opened, model, record, row, now)), [model, now, opened, record, row]);
  const popover = open === null || anchorEl === undefined || plan === null ? null : { anchor: anchorEl, focus: open.focus, pinned: open.pinned, plan };
  return (
    <div className="decision-strip" ref={ref}>
      <div className="decision-strip__surface" style={{ height: layout.height, minWidth: MIN_WIDTH }} data-y0={layout.y0} data-leaves={model.leaves.length} data-stations={model.stations.length}>
        <svg aria-hidden="true" width={layout.width} height={layout.height}>
          {layout.brackets.map((b, i) => (
            <path key={`bracket-${i}`} className="strip-bracket" d={`M${b.from} ${b.y + BRACKET_TICK} V${b.y} H${b.to} V${b.y + BRACKET_TICK}`} />
          ))}
          {layout.paths
            .map((p, i) => ({ ...p, i }))
            .sort((a, b) => PAINT_ORDER[a.state] - PAINT_ORDER[b.state] || a.i - b.i)
            .map((p) => (
              <path key={p.i} className={`strip-path is-${p.state} strip-path--${p.kind}`} d={p.d} />
            ))}
          {/* Each names its button, so a check pairs them by name and a failure names the station. */}
          {stations.map((s) => (
            <circle key={s.id} className={classes('strip-node', `is-${s.state}`, s.decisive && 'is-decisive')} cx={layout.stations[s.id].x} cy={layout.stations[s.id].y} r={RADIUS} data-station={s.id} />
          ))}
          {termini.map(({ p, t }) => (
            <circle key={t.kind} className={classes('strip-node', `is-${t.state}`, 'strip-node--terminus', t.state === 'taken' && `strip-node--${t.tone}`)} cx={p.x} cy={p.y} r={TERMINUS_RADIUS} data-terminus={t.kind} />
          ))}
        </svg>
        {stations.map((s, i) => {
          const p = layout.stations[s.id];
          const label = layout.labels[s.id];
          const anchor: StripAnchor = { kind: 'station', id: s.id };
          return (
            <button
              key={s.id}
              type="button"
              className={classes('strip-station', `is-${s.state}`, s.decisive && 'is-decisive', picked === keyOf(anchor) && 'is-selected', s.level === 'track' && 'strip-station--track', label.above && 'strip-station--above')}
              data-station={s.id}
              data-step={s.step}
              data-index={i}
              aria-label={stationName(s)}
              {...expanded(anchor)}
              style={{ left: p.x - label.width / 2, top: label.top, width: label.width }}
              {...handlers(anchor)}
            >
              <span className="strip-station__word">{s.word}</span>
              {s.badge !== '' && <Badge tone={s.tone}>{s.badge}</Badge>}
              {s.second.length > 0 && (
                <span className="strip-station__second">
                  {/* Each part keeps its value whole, but the Move station's one part names the rule that fired, whose
                      words ("return to plan rule") run wider than a label on a move's tight track, so they wrap a word
                      at a time. */}
                  {s.second.flatMap((part, j) => (s.id === 'move' ? part.split(' ') : [part]).map((words, k) => <span key={`${j}/${k}`}>{words}</span>))}
                </span>
              )}
            </button>
          );
        })}
        {layout.brackets.map((b, i) => (
          <span key={`group-${i}`} className={classes('strip-group', !model.brackets[i]?.recorded && 'is-unrecorded')} style={{ left: b.from, top: b.y - BRACKET_WORD_RISE, width: b.to - b.from }}>
            {b.word}
          </span>
        ))}
        {stations
          .filter((s) => s.stub !== null)
          .map((s) => {
            const p = layout.stations[s.id];
            const down = s.stub === 'released';
            const past = down && !layout.labels[s.id].above;
            return (
              <span key={s.id} className="strip-stub is-idle" style={{ left: p.x + (past ? STUB_WORD_PAST_LABEL : STUB_WORD_X), top: down ? p.y + STUB_WORD_BELOW : p.y - STUB_WORD_ABOVE }}>
                {s.stub}
              </span>
            );
          })}
        {model.leaves.map((l, i) => {
          const p = layout.leaves[i];
          if (p === undefined) return null;
          const anchor: StripAnchor = { kind: 'leaf', index: i };
          // The obstacle chip's hover describes the button a reader lands on. Its node stands after the button, whose
          // own content reads as the button and not apart from it.
          return (
            <Described key={i} text={l.chip?.title}>
              {(describedBy) => (
                <button
                  type="button"
                  className={classes('strip-candidate', l.chosen && 'is-chosen', l.more > 0 && 'is-more', picked === keyOf(anchor) && 'is-selected')}
                  data-leaf={i}
                  aria-label={leafName(l)}
                  aria-describedby={describedBy}
                  {...expanded(anchor)}
                  style={{ left: p.x, maxWidth: p.maxWidth, top: p.y - LEAF_HALF }}
                  {...handlers(anchor)}
                >
                  {l.accountId !== null && <span className="strip-candidate__account">{l.accountId}</span>}
                  <Badge tone={l.tone}>{l.badge}</Badge>
                  {l.chip !== null && <ChipView chip={l.chip} />}
                </button>
              )}
            </Described>
          );
        })}
        {termini.map(({ p, t }) => {
          const anchor: StripAnchor = { kind: 'terminus', id: t.kind };
          const left = p.x + TERMINUS_WORD_X;
          return (
            <button
              key={t.kind}
              type="button"
              className={classes('strip-terminus', `is-${t.state}`, picked === keyOf(anchor) && 'is-selected', stacked && 'strip-terminus--stacked')}
              data-terminus={t.kind}
              aria-label={terminusName(t)}
              {...expanded(anchor)}
              style={{ left, maxWidth: layout.width - left, top: p.y - (stacked ? TERMINUS_LINE_HALF : TERMINUS_ROW_HALF) }}
              {...handlers(anchor)}
            >
              <span className="strip-terminus__word">{t.word}</span>
              {t.state === 'taken' && t.badge !== null && <Badge tone={t.tone}>{t.badge}</Badge>}
            </button>
          );
        })}
      </div>
      {/* A new anchor's box mounts hidden and waits for its own placement, where a box handed the new anchor would be
          placed again in view. A placement again for the same anchor (a new plan, a redrawn strip, a resize, a scroll)
          keeps its box shown, and the focus a keyboard user holds in it. */}
      {popover !== null && <StripPopover key={openedKey} anchor={popover.anchor} bounds={boundsOf(popover.anchor)} focus={popover.focus} id={popoverId} layout={layout} onClose={close} pinned={popover.pinned} plan={popover.plan} />}
    </div>
  );
}
