import { useId, useState } from 'react';
import { Popover } from '@/components/ui/Popover';
import { Button, Field, Select } from '@/components/ui/primitives';
import { useCommittedText } from '@/lib/hooks';

/** The breakdown route's bounds on a pattern list, so the field holds no more than the route accepts. */
export const MAX_PATTERNS = 32;
export const MAX_PATTERN_CHARS = 256;
export const PATTERN_BOUND_HINT = '32 patterns of 256 characters at most, as the route accepts';

/** Lines past the count are dropped and each line cut at the length, counted in characters as the route counts them. */
export function boundPatterns(text: string): { cut: boolean; text: string } {
  const bounded = text
    .split('\n')
    .slice(0, MAX_PATTERNS)
    .map((line) => ([...line].length > MAX_PATTERN_CHARS ? [...line].slice(0, MAX_PATTERN_CHARS).join('') : line))
    .join('\n');
  return { cut: bounded !== text, text: bounded };
}

export interface GroupByProps<G extends string> {
  /** Greys the select out with this as its reason, for a view state whose figures no grouping splits. */
  disabledTitle?: string;
  /** The options in the page's order, the label the select prints, and a title where an option needs a word on itself. */
  options: readonly { label: string; title?: string; value: G }[];
  onChange: (next: G) => void;
  /** The select's title while it stands enabled: what a grouping splits. */
  title?: string;
  value: G;
}

/**
 * `label.row` holding `span.muted` "Group by" and a 32 px Select (aria-label "Group by", class groupby-select, 160 px).
 * `disabledTitle` greys the select out under that title, and `title` is its title otherwise.
 */
export function GroupBy<G extends string>({ disabledTitle, onChange, options, title, value }: GroupByProps<G>): JSX.Element {
  return (
    <label className="row">
      <span className="muted">Group by</span>
      <Select aria-label="Group by" value={value} onChange={(e) => onChange(e.target.value as G)} className="groupby-select" disabled={disabledTitle !== undefined} title={disabledTitle ?? title}>
        {options.map((o) => (
          <option key={o.value} value={o.value} title={o.title}>
            {o.label}
          </option>
        ))}
      </Select>
    </label>
  );
}

export interface GroupsButtonProps {
  /** The newline-separated patterns the address carries. */
  onCommit: (next: string) => void;
  value: string;
}

/** A 32 px secondary button "Groups (N)", N the non-empty lines of `value`, aria-haspopup dialog, aria-expanded, whose Popover holds the textarea under the stacked label "User agent groups" with the page's placeholder, title, maxLength, boundPatterns and PATTERN_BOUND_HINT, committing on blur, on the 400 ms pause and as the popover closes. */
export function GroupsButton({ onCommit, value }: GroupsButtonProps): JSX.Element {
  const boxId = useId();
  const labelId = useId();
  // Held in state through the ref callback, since the hooks lint refuses a ref read while rendering.
  const [anchor, setAnchor] = useState<HTMLButtonElement | null>(null);
  const [open, setOpen] = useState(false);
  // Held by the button rather than the popover's content, so the draft outlives the field. Enter inserts a line, so the
  // field commits on the pause and on blur.
  const text = useCommittedText(value, onCommit);
  // A click outside takes the field away before its blur, and a Group by change inside the pause would then take this
  // button and its pending draft with it, so the draft commits as the popover closes.
  const close = () => {
    text.onBlur();
    setOpen(false);
  };
  const [cut, setCut] = useState(false);
  const count = value.split('\n').filter((line) => line.trim()).length;
  return (
    <>
      <Button ref={setAnchor} size="sm" aria-haspopup="dialog" aria-expanded={open} onClick={() => (open ? close() : setOpen(true))}>
        Groups ({count})
      </Button>
      <Popover anchor={anchor} id={boxId} labelledBy={labelId} onClose={close} open={open}>
        <div className="groups-popover">
          <Field label={<span id={labelId}>User agent groups</span>} hint={cut ? PATTERN_BOUND_HINT : undefined}>
            <textarea
              value={text.draft}
              maxLength={MAX_PATTERNS * (MAX_PATTERN_CHARS + 1)}
              onBlur={text.onBlur}
              onChange={(e) => {
                const next = boundPatterns(e.target.value);
                text.onChange(next.text);
                setCut(next.cut);
              }}
              placeholder={'User agent groups, one pattern per line\nclaude-cli/2.1.*\nclaude-cli/2.0.*'}
              aria-label="User agent groups"
              title="Each pattern names a group, and every other user agent groups under other"
              className="input mono groups-popover__field"
              rows={2}
            />
          </Field>
        </div>
      </Popover>
    </>
  );
}
