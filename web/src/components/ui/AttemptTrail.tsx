import { useLayoutEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import type { AttemptOutcome } from '@/api/types';
import { fmtAbsolute } from '@/lib/format';
import { floatAt } from '@/lib/menu';
import type { BadgeTone } from '@/lib/quota';
import { useDescription } from './primitives';

/** The shape shared by a ledger attempt and a live one; the live one has no status until its head lands. */
export interface TrailAttempt {
  accountId: string;
  cooldownSet: { reason: string; until: string } | null;
  index: number;
  outcome: AttemptOutcome | 'in_flight';
  upstreamStatus: number | null;
}

const OUTCOME_LABEL: Record<AttemptOutcome | 'in_flight', string> = {
  auth_failed: 'auth failed',
  canceled: 'canceled',
  in_flight: 'in flight',
  rate_limited: 'rate limited',
  succeeded: 'succeeded',
  transport_error: 'transport error',
  upstream_error: 'upstream error',
};

const toneOf = (status: number | null): BadgeTone => (status === null ? 'neutral' : status === 200 ? 'good' : status === 429 ? 'warning' : status === 529 ? 'serious' : 'critical');

const lineFor = (a: TrailAttempt) =>
  [`${a.index + 1}. ${a.accountId}: ${OUTCOME_LABEL[a.outcome]}`, a.upstreamStatus === null ? 'no status yet' : `HTTP ${a.upstreamStatus}`, a.cooldownSet ? `cooldown set, ${a.cooldownSet.reason}, until ${fmtAbsolute(a.cooldownSet.until)}` : 'no cooldown set'].join(', ');

/** The trail's lines, one per attempt, the text the count's title and description carry. */
export const trailLines = (attempts: TrailAttempt[]): string => attempts.map(lineFor).join('\n');

/** One badge per attempt in the order they were tried, so a failover reads as "mail 429, business 200". */
export function AttemptTrail({ attempts }: { attempts: TrailAttempt[] }) {
  const title = trailLines(attempts);
  const described = useDescription(title);
  if (attempts.length === 0) return <span className="muted">none</span>;
  return (
    <>
      <span className="trail" title={title} aria-describedby={described?.id}>
        {attempts.map((a) => (
          <span key={a.index} className={`trail__step badge badge--${toneOf(a.upstreamStatus)}`}>
            {a.accountId} <span className="num">{a.upstreamStatus ?? '…'}</span>
          </span>
        ))}
      </span>
      {described?.sibling && (
        <span id={described.id} className="sr-only" data-descriptions="">
          {title}
        </span>
      )}
    </>
  );
}

/**
 * Nothing for a request of one attempt or none, "N attempts" for more, with the trail of badges in a card on hover
 * and on focus, and the trail's lines as the count's title and description. The card is drawn under the body: the
 * table's wrap scrolls sideways, which clips a card positioned inside it for the last rows however high its layer. It
 * is placed fixed from the count's box, below the count when the viewport has the room and above it otherwise, and
 * moved left when its right edge would pass the viewport.
 */
export function AttemptCount({ attempts }: { attempts: TrailAttempt[] }) {
  const many = attempts.length >= 2;
  // Under two attempts nothing registers, so the host's entries are the counts drawn and no more.
  const lines = many ? trailLines(attempts) : undefined;
  const described = useDescription(lines);
  const [at, setAt] = useState<{ left: number; top: number } | null>(null);
  const countRef = useRef<HTMLSpanElement>(null);
  const cardRef = useRef<HTMLSpanElement>(null);
  const show = () => {
    const rect = countRef.current?.getBoundingClientRect();
    if (rect) setAt({ left: rect.left, top: rect.bottom + 4 });
  };
  const hide = () => setAt(null);
  // The card's size is known once it is drawn, so it opens at the count and moves inside the viewport here.
  useLayoutEffect(() => {
    const count = countRef.current;
    const card = cardRef.current;
    if (at === null || !count || !card) return;
    const box = card.getBoundingClientRect();
    const { left, top } = floatAt(count.getBoundingClientRect(), { height: box.height, width: box.width }, window.innerHeight, 'left');
    const inside = { left: Math.max(0, Math.min(left, window.innerWidth - box.width)), top };
    if (inside.left !== at.left || inside.top !== at.top) setAt(inside);
  }, [at]);
  if (!many) return null;
  return (
    <>
      <span ref={countRef} className="trail-count" tabIndex={0} title={lines} aria-describedby={described?.id} onMouseEnter={show} onMouseLeave={hide} onFocus={show} onBlur={hide}>
        {attempts.length} attempts
      </span>
      {described?.sibling && (
        <span id={described.id} className="sr-only" data-descriptions="">
          {lines}
        </span>
      )}
      {at !== null &&
        createPortal(
          <span ref={cardRef} className="trail-pop" style={{ left: at.left, top: at.top }}>
            <AttemptTrail attempts={attempts} />
          </span>,
          document.body,
        )}
    </>
  );
}
