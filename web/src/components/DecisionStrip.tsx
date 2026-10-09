import { useLayoutEffect, useMemo, useRef, useState } from 'react';
import type { DecisionRecord, DecisionSummary } from '@/api/types';
import { ChipView } from '@/components/DecisionCard';
import { Badge } from '@/components/ui/primitives';
import { clock } from '@/lib/clock';
import { layoutStrip, leafName, RADIUS, stationModel, stationName, TERMINUS_RADIUS, terminusName, type PathState, type TerminusKind } from '@/lib/decisionStrip';

/** What a click on the strip names: a station by its model id, a leaf by its index, a terminus by its kind. */
export type StripAnchor = { kind: 'station'; id: string } | { kind: 'leaf'; index: number } | { kind: 'terminus'; id: TerminusKind };

const MIN_WIDTH = 640;
const REDRAW_MS = 120;
// A stub runs 10 px across and 16 px up from its station, or down on the renewal, and its word starts past its end.
const STUB_WORD_X = 12;
const STUB_WORD_ABOVE = 30;
const STUB_WORD_BELOW = 12;
// Under a station whose label sits below the line the word starts past that label's own word ("Renew"), which the
// stub's end would put it across.
const STUB_WORD_PAST_LABEL = 24;
const GROUP_WORD_RISE = 20;
// The ticks reach 6 px under the bracket, which clears the labels a track's stations carry under it by 3 px.
const BRACKET_TICK = 6;
const LEAF_HALF = 12;
const TERMINUS_WORD_X = 14;
// A terminus word sits level with its node: centered on the badge's 22 px box, or on its own 16 px line when stacked.
const TERMINUS_ROW_HALF = 11;
const TERMINUS_LINE_HALF = 8;
// The idle paths share their first run with the taken one, so the taken path is drawn last to stay on top.
const PAINT_ORDER: Record<PathState, number> = { idle: 0, inferred: 1, taken: 2 };

const keyOf = (a: StripAnchor) => (a.kind === 'leaf' ? `leaf:${a.index}` : `${a.kind}:${a.id}`);
const classes = (...names: (string | false)[]) => names.filter(Boolean).join(' ');

/**
 * The walk as a subway strip: the model from the record, the layout from the slot's width (at least 640 px, re-read on
 * a resize debounced at 120 ms), one SVG for the lines and a button per station, leaf and terminus. `onOpen` is the
 * popover's hook. `selected` marks the anchor whose popover is pinned.
 */
export function DecisionStrip({ now, onOpen, record, row, selected }: { now: number; onOpen?: (anchor: StripAnchor) => void; record: DecisionRecord; row: DecisionSummary; selected?: StripAnchor | null }): JSX.Element {
  const ref = useRef<HTMLDivElement>(null);
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
  const model = useMemo(() => stationModel(record, row, now), [record, row, now]);
  const layout = useMemo(() => layoutStrip(model, width), [model, width]);
  const picked = selected ? keyOf(selected) : null;
  const open = (anchor: StripAnchor) => () => onOpen?.(anchor);
  // The line's stations left to right, then the track's, so Tab walks the strip in the record's order.
  const stations = [...model.stations.filter((s) => s.level === 'line'), ...model.stations.filter((s) => s.level === 'track')];
  const termini = model.termini.flatMap((t) => {
    const p = layout.termini[t.kind];
    return t.drawn && p !== undefined ? [{ p, t }] : [];
  });
  // A move's continuation leaves its terminus a narrower margin than a line does, so the word stacks over the badge.
  const stacked = model.branch?.kind === 'move';
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
          {stations.map((s) => (
            <circle key={s.id} className={classes('strip-node', `is-${s.state}`, s.decisive && 'is-decisive')} cx={layout.stations[s.id].x} cy={layout.stations[s.id].y} r={RADIUS} />
          ))}
          {termini.map(({ p, t }) => (
            <circle key={t.kind} className={classes('strip-node', `is-${t.state}`, 'strip-node--terminus', t.state === 'taken' && `strip-node--${t.tone}`)} cx={p.x} cy={p.y} r={TERMINUS_RADIUS} />
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
              style={{ height: label.height, left: p.x - label.width / 2, top: label.top, width: label.width }}
              onClick={open(anchor)}
            >
              <span className="strip-station__word">{s.word}</span>
              {s.badge !== '' && <Badge tone={s.tone}>{s.badge}</Badge>}
              {s.second.length > 0 && (
                <span className="strip-station__second">
                  {s.second.map((part, j) => (
                    <span key={j}>{part}</span>
                  ))}
                </span>
              )}
            </button>
          );
        })}
        {layout.brackets.map((b, i) => (
          <span key={`group-${i}`} className={classes('strip-group', !model.brackets[i]?.recorded && 'is-unrecorded')} style={{ left: b.from, top: b.y - GROUP_WORD_RISE, width: b.to - b.from }}>
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
          return (
            <button
              key={i}
              type="button"
              className={classes('strip-candidate', l.chosen && 'is-chosen', l.more > 0 && 'is-more', picked === keyOf(anchor) && 'is-selected')}
              data-leaf={i}
              aria-label={leafName(l)}
              style={{ left: p.x, top: p.y - LEAF_HALF }}
              onClick={open(anchor)}
            >
              {l.accountId !== null && <span className="strip-candidate__account">{l.accountId}</span>}
              <Badge tone={l.tone}>{l.badge}</Badge>
              {l.chip !== null && <ChipView chip={l.chip} />}
            </button>
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
              style={{ left, maxWidth: layout.width - left, top: p.y - (stacked ? TERMINUS_LINE_HALF : TERMINUS_ROW_HALF) }}
              onClick={open(anchor)}
            >
              <span className="strip-terminus__word">{t.word}</span>
              {t.state === 'taken' && t.badge !== null && <Badge tone={t.tone}>{t.badge}</Badge>}
            </button>
          );
        })}
      </div>
    </div>
  );
}
