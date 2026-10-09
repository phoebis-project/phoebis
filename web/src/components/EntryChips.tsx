import type { PrefixEntry } from '@/api/types';
import { hashTitle, shortHash, warmUntil } from '@/lib/routingLog';

/** The rung's entry: its hash's first eight characters, the rest on hover, and when it stops being warm. */
export function EntryChips({ entry, now }: { entry: PrefixEntry; now: number }) {
  const until = warmUntil(entry, now);
  return (
    <>
      <span className="chip mono decision-card__hash" title={hashTitle(entry)}>
        {shortHash(entry.hash)}
      </span>
      {until !== null && <span className="chip decision-card__warm">{until}</span>}
    </>
  );
}
