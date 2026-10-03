import { useCommittedText } from '@/lib/hooks';
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
  const text = useCommittedText(value, onCommit, delayMs);
  return (
    <Input
      value={text.draft}
      placeholder={label}
      aria-label={label}
      title={PATTERN_GRAMMAR}
      className="mono"
      // `.mono` shrinks the text to 0.92em, which would leave the field shorter than the rest of a filter bar.
      style={{ fontSize: 'inherit', width: 180 }}
      onChange={(e) => text.onChange(e.target.value)}
      onBlur={text.onBlur}
      onKeyDown={text.onKeyDown}
    />
  );
}
