import type { PrefixEntry } from '@/api/types';
import { useDescription } from '@/components/ui/primitives';
import { hashTitle, shortHash, warmUntil } from '@/lib/routingLog';

/**
 * A rung entry's hash: its first eight characters, the rest and any refusing gate on hover. The hover is a description
 * too, through the page's host as a `Badge`'s is.
 */
export function HashChip({ entry }: { entry: PrefixEntry }) {
  const title = hashTitle(entry);
  const described = useDescription(title);
  // Outside every host the title alone describes the chip: a node beside it would join the cell's text.
  const hosted = described && !described.sibling ? described.id : undefined;
  return (
    <span className="chip mono decision-card__hash" title={title} aria-describedby={hosted}>
      {shortHash(entry.hash)}
    </span>
  );
}

/** The rung's entry: its hash's first eight characters, the rest on hover, and when it stops being warm. */
export function EntryChips({ entry, now }: { entry: PrefixEntry; now: number }) {
  const until = warmUntil(entry, now);
  return (
    <>
      <HashChip entry={entry} />
      {until !== null && <span className="chip decision-card__warm">{until}</span>}
    </>
  );
}
