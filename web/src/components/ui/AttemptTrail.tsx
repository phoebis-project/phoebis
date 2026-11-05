import { useLayoutEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import type { AttemptOutcome } from '@/api/types';
import { fmtAbsolute } from '@/lib/format';
import { floatAt } from '@/lib/menu';
import type { BadgeTone } from '@/lib/quota';
import { useLeaveOnMove } from './overlay';
import { Badge, Described } from './primitives';

/** The shape of a ledger attempt as the trail draws it. */
export interface TrailAttempt {
  accountId: string;
  cooldownSet: { reason: string; until: string } | null;
  index: number;
  outcome: AttemptOutcome;
  upstreamStatus: number | null;
}

const OUTCOME_LABEL: Record<AttemptOutcome, string> = {
  auth_failed: 'auth failed',
  canceled: 'canceled',
  rate_limited: 'rate limited',
  succeeded: 'succeeded',
  transport_error: 'transport error',
  upstream_error: 'upstream error',
};

const statusTone = (status: number | null): BadgeTone => (status === null ? 'neutral' : status === 200 ? 'good' : status === 429 ? 'warning' : status === 529 ? 'serious' : 'critical');

export function StatusBadge({ status, attempts = 1 }: { attempts?: number; status: number | null }) {
  return (
    <Badge tone={statusTone(status)} title={attempts > 1 ? `${attempts} attempts` : undefined}>
      {status ?? '-'}
      {attempts > 1 && <span className="badge__extra">×{attempts}</span>}
    </Badge>
  );
}

const lineFor = (a: TrailAttempt) =>
  [`${a.index + 1}. ${a.accountId}: ${OUTCOME_LABEL[a.outcome]}`, a.upstreamStatus === null ? 'no status' : `HTTP ${a.upstreamStatus}`, a.cooldownSet ? `cooldown set, ${a.cooldownSet.reason}, until ${fmtAbsolute(a.cooldownSet.until)}` : 'no cooldown set'].join(', ');

/** The trail's lines, one per attempt, the text the count's title and description carry. */
export const trailLines = (attempts: TrailAttempt[]): string => attempts.map(lineFor).join('\n');

/** One badge per attempt in the order they were tried, so a failover reads as "a 429, b 200". */
export function AttemptTrail({ attempts }: { attempts: TrailAttempt[] }) {
  const title = trailLines(attempts);
  if (attempts.length === 0) return <span className="muted">none</span>;
  return (
    <Described text={title}>
      {(describedBy) => (
        <span className="trail" title={title} aria-describedby={describedBy}>
          {attempts.map((a) => (
            <span key={a.index} className={`trail__step badge badge--${statusTone(a.upstreamStatus)}`}>
              {a.accountId} <span className="num">{a.upstreamStatus ?? '…'}</span>
            </span>
          ))}
        </span>
      )}
    </Described>
  );
}

/**
 * Nothing for a request of one attempt or none, "N attempts" for more, with the trail of badges in a card on hover
 * and on focus, and the trail's lines as the count's title and description. The card is drawn under the body: the
 * table's wrap scrolls sideways, which clips a card positioned inside it for the last rows however high its layer. It
 * is placed fixed from the count's box, below the count when the viewport has the room and above it otherwise, and
 * moved left when its right edge would pass the viewport. A fixed card stays where it was drawn while the page moves
 * under it, so it leaves rather than float beside another row: on a scroll that moved the count, on a resize, and on
 * a render that finds the count elsewhere (a refresh that re-sorts the rows, the columns changing under a drawer).
 */
export function AttemptCount({ attempts }: { attempts: TrailAttempt[] }) {
  const many = attempts.length >= 2;
  // Under two attempts nothing registers, so the host's entries are the counts drawn and no more.
  const lines = many ? trailLines(attempts) : undefined;
  const [at, setAt] = useState<{ left: number; top: number } | null>(null);
  const open = at !== null;
  const countRef = useRef<HTMLSpanElement>(null);
  const cardRef = useRef<HTMLSpanElement>(null);
  // Where the count stood when the card opened, which every move check below reads against.
  const anchor = useRef<{ left: number; top: number } | null>(null);
  const show = () => {
    const rect = countRef.current?.getBoundingClientRect();
    if (!rect) return;
    anchor.current = { left: rect.left, top: rect.top };
    setAt({ left: rect.left, top: rect.bottom + 4 });
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
  useLeaveOnMove(open, countRef, anchor, hide);
  if (!many) return null;
  return (
    <>
      <Described text={lines}>
        {(describedBy) => (
          <span ref={countRef} className="trail-count" tabIndex={0} title={lines} aria-describedby={describedBy} onMouseEnter={show} onMouseLeave={hide} onFocus={show} onBlur={hide}>
            {attempts.length} attempts
          </span>
        )}
      </Described>
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
