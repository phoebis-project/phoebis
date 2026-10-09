import type { PrefixEntry } from '@/api/types';
import { hashTitle, shortHash, warmUntil } from '@/lib/routingLog';

/** A rung entry's hash: its first eight characters, the rest and any refusing gate on hover. */
export function HashChip({ entry }: { entry: PrefixEntry }) {
  return (
    <span className="chip mono decision-card__hash" title={hashTitle(entry)}>
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
