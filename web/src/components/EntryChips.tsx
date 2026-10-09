import type { PrefixEntry } from '@/api/types';
import { useDescription } from '@/components/ui/primitives';
import { hashTitle, shortHash, warmUntil } from '@/lib/routingLog';

/**
 * A rung entry's hash: its first eight characters, the rest and any refusing gate on hover. The hover is a description
 * too, through the page's host, or in a node after the chip where no host stands.
 */
export function HashChip({ entry }: { entry: PrefixEntry }) {
  const title = hashTitle(entry);
  const described = useDescription(title);
  return (
    <>
      <span className="chip mono decision-card__hash" title={title} aria-describedby={described?.id}>
        {shortHash(entry.hash)}
      </span>
      {described?.sibling && (
        <span id={described.id} className="sr-only" data-descriptions="">
          {title}
        </span>
      )}
    </>
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
