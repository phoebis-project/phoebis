import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
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

/** Anything but printable ASCII, a tab included: the lines whose columns the estimate counts by grapheme. */
const NOT_PLAIN = /[^\x20-\x7e]/;
/** The East Asian wide and fullwidth ranges the estimate counts as two columns. */
const WIDE =
  /[\u{1100}-\u{115f}\u{2e80}-\u{303e}\u{3041}-\u{33ff}\u{3400}-\u{4dbf}\u{4e00}-\u{9fff}\u{a000}-\u{a4cf}\u{a960}-\u{a97f}\u{ac00}-\u{d7a3}\u{f900}-\u{faff}\u{fe10}-\u{fe19}\u{fe30}-\u{fe6f}\u{ff00}-\u{ff60}\u{ffe0}-\u{ffe6}\u{1f300}-\u{1f64f}\u{1f900}-\u{1f9ff}\u{20000}-\u{2fffd}\u{30000}-\u{3fffd}]/u;

/**
 * `Intl.Segmenter` where the runtime has one, typed here since the console's TypeScript lib is ES2020's, which predates
 * it.
 */
const Segmenter = (Intl as unknown as { Segmenter?: new (locales: undefined, options: { granularity: 'grapheme' }) => { segment: (text: string) => Iterable<{ segment: string }> } }).Segmenter;
const GRAPHEMES = Segmenter === undefined ? null : new Segmenter(undefined, { granularity: 'grapheme' });

/**
 * A line's columns as the estimate counts them: a tab to the next 8-column stop, a grapheme holding a `WIDE` code
 * point 2, any other grapheme 1, so a letter and its combining marks take one column. Without a segmenter each code
 * unit counts 1.
 */
function columnsOf(line: string): number {
  if (GRAPHEMES === null || !NOT_PLAIN.test(line)) return line.length;
  let columns = 0;
  for (const { segment } of GRAPHEMES.segment(line)) columns = segment === '\t' ? columns + 8 - (columns % 8) : columns + (WIDE.test(segment) ? 2 : 1);
  return columns;
}

/** The line of the most columns, the first of them on a tie. */
function widestLine(lines: readonly string[]): string {
  let widest = '';
  let most = -1;
  for (const line of lines) {
    const columns = columnsOf(line);
    if (columns > most) {
      most = columns;
      widest = line;
    }
  }
  return widest;
}

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
  // Absolutely placed rows widen the scroll area only while drawn, so the block's floor is the width the widest line
  // draws in the pre's font, read off a hidden copy of it once per text, and a reader scrolled sideways keeps the place
  // when that line leaves the window. A character count would miss a tab's stop, a wide character's second column and
  // the combining marks that take none, so the copy holds the line of the most columns.
  const widest = useMemo(() => widestLine(lines), [lines]);
  const probe = useRef<HTMLDivElement>(null);
  const [floor, setFloor] = useState(0);
  useLayoutEffect(() => {
    const el = probe.current;
    if (!el) return;
    setFloor(el.scrollWidth);
    // The wide dialog lays its children out only once it opens, after they mount, so there the copy reads 0 wide
    // until the observer reports its drawn width.
    if (typeof ResizeObserver !== 'function') return;
    const observer = new ResizeObserver(() => setFloor(el.scrollWidth));
    observer.observe(el);
    return () => observer.disconnect();
  }, [widest]);
  const rows = [];
  for (let i = span.first, end = Math.min(span.last, lines.length); i < end; i += 1) rows.push(<Row key={i} highlight={highlight} line={lines[i]} top={i * ROW_PX} />);
  return (
    <pre ref={box} className="body-block__pre body-block__pre--rows" tabIndex={0} onScroll={onScroll}>
      <div className="body-rows" style={{ height: lines.length * ROW_PX, minWidth: floor }}>
        {/* Before the rows and outside their class, so a row stays the block's last child and the rows count alone. */}
        <div ref={probe} aria-hidden="true" style={{ position: 'absolute', visibility: 'hidden', whiteSpace: 'pre' }}>
          {widest}
        </div>
        {rows}
      </div>
    </pre>
  );
}
