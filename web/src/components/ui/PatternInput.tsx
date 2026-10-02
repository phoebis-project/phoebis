import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { clock } from '@/lib/clock';
import { Input } from './primitives';

export const PATTERN_GRAMMAR = '* matches any run of characters, ? one; a backslash quotes either';

interface Props {
  /** How long after the last keystroke the draft commits. */
  delayMs?: number;
  /** The placeholder and the accessible name. */
  label: string;
  onCommit: (value: string) => void;
  /** The committed value. */
  value: string;
}

/** A pattern filter that commits its text once typing pauses, or at once on Enter or when the field loses focus. */
export function PatternInput({ delayMs = 400, label, onCommit, value }: Props) {
  const [draft, setDraft] = useState(value);
  const committed = useRef(value);
  const timer = useRef<number | null>(null);
  // A pending commit fires after later renders, so it reads the page's latest handler, never the one its keystroke saw.
  const latest = useRef(onCommit);
  useLayoutEffect(() => {
    latest.current = onCommit;
  });
  const clear = () => {
    if (timer.current !== null) clock.clearTimeout(timer.current);
    timer.current = null;
  };
  // An outside change (a reset, the URL) replaces the draft and drops the old draft's pending commit, which would
  // otherwise restore the value the page just replaced; the page echoing a commit back does not, so typing is never
  // undone mid-word. It runs before paint, so the field never shows the replaced draft for a frame.
  useLayoutEffect(() => {
    if (value !== committed.current) {
      clear();
      committed.current = value;
      setDraft(value);
    }
  }, [value]);
  const commit = (next: string) => {
    clear();
    if (next === committed.current) return;
    committed.current = next;
    latest.current(next);
  };
  useEffect(() => clear, []);
  return (
    <Input
      value={draft}
      placeholder={label}
      aria-label={label}
      title={PATTERN_GRAMMAR}
      className="mono"
      style={{ fontSize: 'inherit', width: 180 }}
      onChange={(e) => {
        const next = e.target.value;
        setDraft(next);
        clear();
        timer.current = clock.setTimeout(() => commit(next), delayMs);
      }}
      onBlur={() => commit(draft)}
      onKeyDown={(e) => {
        if (e.key === 'Enter') commit(draft);
      }}
    />
  );
}
