import { useId, useLayoutEffect, useRef, useState } from 'react';
import { api } from '@/api';
import type { AttentionItem } from '@/api/types';
import { ATTENTION_LABEL, ATTENTION_TONE, attentionHref, attentionTitle, attentionWords } from '@/lib/attention';
import { fmtInt, fmtRelative } from '@/lib/format';
import { useLatest, useResizeObserver } from '@/lib/hooks';
import { Popover } from './Popover';
import { Badge, BadgeLink, Pill, Skeleton } from './primitives';

const widthOf = (el: Element | null): number => el?.getBoundingClientRect().width ?? 0;

/**
 * The dashboard's line above the tiles: what needs the operator, worst first, each a badge that opens where it is
 * fixed. The line keeps one row of items in every state, the loading, quiet and failed ones included, and folds the
 * badges past the row behind "+N more", whose popover lists every item, so the tiles hold still as the router's state
 * changes. Only where the lead, the fold badge and the Replay button with its answer alone pass the row's width do the
 * button and its answer take a second row. A failed read never draws the quiet line, which would claim a health the
 * read did not see.
 *
 * While a parked record shows, the line carries the Replay button after its badges. Its answer, or the route's
 * refusal, reads in the slot beside it rather than a toast, and an answer reads the list again through `refresh`, so
 * the parked badge clears without waiting for the next tick.
 */
export function AttentionLine({ error, items, loadedAt, now, refresh }: { error: string | null; items: AttentionItem[] | null; loadedAt: number | null; now: number; refresh: () => Promise<void> }) {
  const [replaying, setReplaying] = useState(false);
  const [replayed, setReplayed] = useState<string | null>(null);
  const replay = async () => {
    setReplaying(true);
    try {
      const { landed, remaining } = await api.replayParked();
      setReplayed(`Landed ${fmtInt(landed)}, ${fmtInt(remaining)} still parked`);
      await refresh();
    } catch (e) {
      setReplayed(e instanceof Error ? e.message : String(e));
    } finally {
      setReplaying(false);
    }
  };
  const slot = replayed === null ? null : <span className="attention-line__replay">{replayed}</span>;
  const lineRef = useRef<HTMLDivElement>(null);
  const rowRef = useRef<HTMLSpanElement>(null);
  const [fit, setFit] = useState(Number.POSITIVE_INFINITY);
  const [wrapped, setWrapped] = useState(false);
  const [open, setOpen] = useState(false);
  // Held in state through the ref callback, since the hooks lint refuses a ref read while rendering.
  const [fold, setFold] = useState<HTMLButtonElement | null>(null);
  const listId = useId();
  const drawn = error === null && items !== null && items.length > 0;
  const badges = (drawn ? items : []).map((item) => ({
    href: attentionHref(item, now),
    // The raw kind keeps two conditions the console does not know apart on one subject.
    key: `${item.rawKind ?? item.kind}:${item.subject.type}:${item.subject.id}`,
    text: attentionWords(item, now) ?? `${ATTENTION_LABEL[item.kind]}: ${item.subject.label}`,
    title: attentionTitle(item, now),
    tone: ATTENTION_TONE[item.kind],
  }));
  const folded = badges.slice(fit);
  // The list goes with the badge it hangs from, so a fold drawn again later opens nothing on its own.
  if (open && folded.length === 0) setOpen(false);
  const parked = drawn && items.some((item) => item.kind === 'ledger_record_parked');

  // How many badges end inside the line, and whether the Replay button and its answer stand beside them or take a
  // second row. Every width is read in one state no earlier answer shapes: the line on one row, every badge shown and
  // the fold badge drawn with the widest count it can carry, each put back before paint. The answer rests on the widths
  // alone, so measuring again finds it again.
  const measure = () => {
    const line = lineRef.current;
    const row = rowRef.current;
    const more = line?.querySelector<HTMLElement>(':scope > .attention-line__more') ?? null;
    if (line === null || row === null || more === null) return;
    const width = line.clientWidth;
    // A line no layout measured (jsdom, the smoke) shows every badge, so the checks read them all on the row.
    if (width === 0) {
      setFit(Number.POSITIVE_INFINITY);
      setWrapped(false);
      return;
    }
    const wraps = [...row.children].filter((el): el is HTMLElement => el instanceof HTMLElement);
    const was = { hidden: wraps.map((el) => el.hidden), line: line.className, more: more.hidden, words: more.textContent };
    line.className = 'attention-line';
    for (const el of wraps) el.hidden = false;
    more.hidden = false;
    more.textContent = `+${wraps.length} more`;
    const widths = wraps.map(widthOf);
    const lead = widthOf(line.querySelector(':scope > .attention-line__lead'));
    const fold = widthOf(more);
    const tail = line.querySelector(':scope > .attention-line__tail');
    const tailParts = tail === null ? [] : [...tail.children].map(widthOf);
    const tailWidth = tailParts.reduce((sum, w) => sum + w, 0) + (tail === null ? 0 : (parseFloat(getComputedStyle(tail).columnGap) || 0) * Math.max(0, tailParts.length - 1));
    const gap = parseFloat(getComputedStyle(line).columnGap) || 0;
    const rowGap = parseFloat(getComputedStyle(row).columnGap) || 0;
    line.className = was.line;
    wraps.forEach((el, i) => {
      el.hidden = was.hidden[i];
    });
    more.hidden = was.more;
    more.textContent = was.words;
    const fits = (count: number, withTail: boolean) => {
      const parts = [lead, widths.slice(0, count).reduce((sum, w) => sum + w, 0) + rowGap * Math.max(0, count - 1), ...(count < widths.length ? [fold] : []), ...(withTail && tail !== null ? [tailWidth] : [])];
      return parts.reduce((sum, w) => sum + w, 0) + gap * (parts.length - 1) <= width;
    };
    // The tail takes a second row only where it fits beside neither every badge nor the lead and the fold badge alone.
    const wrap = tail !== null && !fits(widths.length, true) && !fits(0, true);
    let count = widths.length;
    while (count > 0 && !fits(count, !wrap)) count -= 1;
    setWrapped(wrap);
    setFit(count);
  };
  const measureRef = useLatest(measure);
  // A width moves only with the words the line draws (the badges, the lead's count, the Replay button, its answer) or
  // with the line's own width, which the observer below follows.
  const words = [...badges.map((b) => b.text), parked ? 'parked' : '', replayed ?? ''].join('\n');
  useLayoutEffect(() => measureRef.current(), [drawn, measureRef, words]);
  useResizeObserver(lineRef, measure, drawn);

  if (error !== null) {
    const title = loadedAt === null ? error : `${error}\nLast read: ${fmtRelative(loadedAt, now)}`;
    return (
      <div className="attention-line">
        <Badge tone="warning" title={title}>
          <span className="attention-line__cut">Attention unavailable: {error}</span>
        </Badge>
      </div>
    );
  }
  if (items === null) {
    return (
      <div className="attention-line">
        <Skeleton width={320} height={22} />
      </div>
    );
  }
  if (items.length === 0) {
    return (
      <div className="attention-line">
        <Badge tone="good">Nothing needs attention</Badge>
        {slot}
      </div>
    );
  }
  const badge = (b: (typeof badges)[number]) =>
    b.href ? (
      <BadgeLink to={b.href} tone={b.tone} title={b.title}>
        {b.text}
      </BadgeLink>
    ) : (
      <Badge tone={b.tone} title={b.title}>
        {b.text}
      </Badge>
    );
  return (
    <div ref={lineRef} className={wrapped ? 'attention-line is-wrapped' : 'attention-line'}>
      <span className="attention-line__lead">
        {items.length} {items.length === 1 ? 'needs' : 'need'} attention
      </span>
      <span ref={rowRef} className="attention-line__items">
        {badges.map((b, i) => (
          <span key={b.key} className="attention-line__item" hidden={i >= fit}>
            {badge(b)}
          </span>
        ))}
      </span>
      {/* Drawn while nothing folds as well, hidden, so the measure can read its width before a fold needs it. */}
      <button
        ref={setFold}
        type="button"
        className="badge badge--neutral attention-line__more"
        hidden={folded.length === 0}
        // One item per line, since a routing condition's words carry commas of their own.
        title={folded.length === 0 ? undefined : ['Folded:', ...folded.map((b) => b.text)].join('\n')}
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-controls={open ? listId : undefined}
        onClick={() => setOpen((v) => !v)}
      >
        {`+${folded.length} more`}
      </button>
      {(parked || slot !== null) && (
        <span className="attention-line__tail">
          {parked && (
            <Pill onClick={() => void replay()} disabled={replaying}>
              Replay parked records
            </Pill>
          )}
          {slot}
        </span>
      )}
      <Popover anchor={fold} className="attention-line__list" id={listId} label="Needs attention" onClose={() => setOpen(false)} open={open} role="dialog">
        {badges.map((b) => (
          <span key={b.key} className="attention-line__item" onClick={b.href ? () => setOpen(false) : undefined}>
            {badge(b)}
          </span>
        ))}
      </Popover>
    </div>
  );
}
