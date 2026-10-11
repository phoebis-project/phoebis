import { memo, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { highlightJson } from '@/lib/bodies';

/** One row of the mono font's line height, 12 px at 1.5. `--body-row` in pages.css carries the same figure. */
export const ROW_PX = 18;
/** The rows drawn beyond the viewport on each side, so a scroll of a few rows meets drawn rows. */
export const WINDOW_ROWS = 20;

/**
 * The rows the window draws, `last` exclusive. The line count caps `last` as the rows draw, so new lines need no new
 * read of the box.
 */
interface Span {
  first: number;
  last: number;
}

const spanOf = (top: number, height: number): Span => ({
  first: Math.max(0, Math.floor(top / ROW_PX) - WINDOW_ROWS),
  last: Math.ceil((top + height) / ROW_PX) + WINDOW_ROWS,
});

/**
 * One line, tokenized alone. Memoized on its own props, so a row tokenizes once while the window keeps it, and its
 * tokens go when it leaves.
 */
const Row = memo(function Row({ highlight, line, top }: { highlight: boolean; line: string; top: number }) {
  return (
    <div className="body-row" style={{ top }}>
      {highlight ? highlightJson(line).map((t, i) => (t.cls === null ? t.text : <span key={i} className={t.cls}>{t.text}</span>)) : line}
    </div>
  );
});

/**
 * A body as rows: a scroll box (the `pre` itself, `overflow: auto`) holding a block of `lines.length` rows' height,
 * with only the rows inside the viewport plus WINDOW_ROWS on each side drawn at their offsets, each tokenized alone
 * when `highlight` is on and memoized while it stays drawn. `onScroll` moves the window through one
 * requestAnimationFrame, and a ResizeObserver on the box re-reads the viewport's height.
 */
export function BodyViewer({ highlight, lines }: { highlight: boolean; lines: readonly string[] }) {
  const box = useRef<HTMLPreElement>(null);
  const [span, setSpan] = useState<Span>(() => spanOf(0, 0));
  const read = useCallback(() => {
    const el = box.current;
    if (!el) return;
    const next = spanOf(el.scrollTop, el.clientHeight);
    setSpan((was) => (was.first === next.first && was.last === next.last ? was : next));
  }, []);
  const frame = useRef<number | null>(null);
  const onScroll = () => {
    if (frame.current !== null) return;
    frame.current = requestAnimationFrame(() => {
      frame.current = null;
      read();
    });
  };
  // Read once as the box mounts, where no observer exists or one never reports. The wide dialog opens after its
  // children mount, so its height arrives through the observer.
  useEffect(() => {
    const el = box.current;
    read();
    const cancel = () => {
      if (frame.current !== null) cancelAnimationFrame(frame.current);
      frame.current = null;
    };
    if (!el || typeof ResizeObserver !== 'function') return cancel;
    const observer = new ResizeObserver(read);
    observer.observe(el);
    return () => {
      observer.disconnect();
      cancel();
    };
  }, [read]);
  // Absolutely placed rows widen the scroll area only while drawn, so the longest line sets the block's width in the
  // mono font's ch, and a reader scrolled sideways keeps the place when a long line leaves the window.
  const widest = useMemo(() => lines.reduce((most, line) => Math.max(most, line.length), 0), [lines]);
  const rows = [];
  for (let i = span.first, end = Math.min(span.last, lines.length); i < end; i += 1) rows.push(<Row key={i} highlight={highlight} line={lines[i]} top={i * ROW_PX} />);
  return (
    <pre ref={box} className="body-block__pre body-block__pre--rows" tabIndex={0} onScroll={onScroll}>
      <div className="body-rows" style={{ height: lines.length * ROW_PX, minWidth: `${widest}ch` }}>
        {rows}
      </div>
    </pre>
  );
}
