import { useEffect, useId, useLayoutEffect, useRef, useState } from 'react';
import { api } from '@/api';
import type { AttentionItem } from '@/api/types';
import { ATTENTION_LABEL, ATTENTION_TONE, attentionHref, attentionTitle, attentionWords } from '@/lib/attention';
import { fmtInt, fmtRelative } from '@/lib/format';
import { Popover } from './Popover';
import { Badge, BadgeLink, Pill, Skeleton } from './primitives';

const widthOf = (el: Element | null): number => el?.getBoundingClientRect().width ?? 0;

/**
 * The dashboard's line above the tiles: what needs the operator, worst first, each a badge that opens where it is
 * fixed. The line keeps one row in every state, the loading, quiet and failed ones included, and folds the badges past
 * the row behind "+N more", whose popover lists every item, so the tiles never move with the router's state. A failed
 * read never draws the quiet line, which would claim a health the read did not see.
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
  const [open, setOpen] = useState(false);
  // Held in state through the ref callback, since the hooks lint refuses a ref read while rendering.
  const [fold, setFold] = useState<HTMLButtonElement | null>(null);
  const listId = useId();
  const drawn = error === null && items !== null && items.length > 0;
  const badges = (drawn ? items : []).map((item) => ({
    href: attentionHref(item, now),
    key: `${item.kind}:${item.subject.type}:${item.subject.id}`,
    text: attentionWords(item, now) ?? `${ATTENTION_LABEL[item.kind]}: ${item.subject.label}`,
    title: attentionTitle(item, now),
    tone: ATTENTION_TONE[item.kind],
  }));
  const folded = badges.slice(fit);
  // The list goes with the badge it hangs from, so a fold drawn again later opens nothing on its own.
  if (open && folded.length === 0) setOpen(false);

  // How many badges end inside the line with room left for the lead, the fold badge, the Replay button and its slot.
  // The badges past the last fit are drawn hidden, so each measure shows them for the reading and hides them again
  // before paint.
  const measure = () => {
    const line = lineRef.current;
    const row = rowRef.current;
    if (line === null || row === null) return;
    const width = line.clientWidth;
    // A line no layout measured (jsdom, the smoke) shows every badge, so the checks read them all on the row.
    if (width === 0) {
      setFit(Number.POSITIVE_INFINITY);
      return;
    }
    const wraps = [...row.children].filter((el): el is HTMLElement => el instanceof HTMLElement);
    const hidden = wraps.map((el) => el.hidden);
    for (const el of wraps) el.hidden = false;
    const widths = wraps.map(widthOf);
    wraps.forEach((el, i) => {
      el.hidden = hidden[i];
    });
    const more = widthOf(line.querySelector(':scope > .attention-line__more'));
    const beside = [...line.children].filter((el) => el !== row && !el.classList.contains('attention-line__more')).map(widthOf);
    const gap = parseFloat(getComputedStyle(line).columnGap) || 0;
    const rowGap = parseFloat(getComputedStyle(row).columnGap) || 0;
    const fits = (count: number) => {
      const parts = [widths.slice(0, count).reduce((sum, w) => sum + w, 0) + rowGap * Math.max(0, count - 1), ...beside, ...(count < widths.length ? [more] : [])];
      return parts.reduce((sum, w) => sum + w, 0) + gap * (parts.length - 1) <= width;
    };
    let count = widths.length;
    while (count > 0 && !fits(count)) count -= 1;
    setFit(count);
  };
  const measureRef = useRef(measure);
  // Every draw can change a part's width (a refresh's items, the replay's answer in the slot), so each one measures
  // again.
  useLayoutEffect(() => {
    measureRef.current = measure;
    measure();
  });
  useEffect(() => {
    const line = lineRef.current;
    if (!drawn || line === null || typeof ResizeObserver !== 'function') return;
    const observer = new ResizeObserver(() => measureRef.current());
    observer.observe(line);
    return () => observer.disconnect();
  }, [drawn]);

  if (error !== null) {
    const title = loadedAt === null ? error : `${error}\nLast read: ${fmtRelative(new Date(loadedAt).toISOString(), now)}`;
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
    <div ref={lineRef} className="attention-line">
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
      {folded.length > 0 && (
        <button ref={setFold} type="button" className="badge badge--neutral attention-line__more" title={`Folded: ${folded.map((b) => b.text).join(', ')}`} aria-haspopup="dialog" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
          {`+${folded.length} more`}
        </button>
      )}
      {items.some((item) => item.kind === 'ledger_record_parked') && (
        <Pill onClick={() => void replay()} disabled={replaying}>
          Replay parked records
        </Pill>
      )}
      {slot}
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
