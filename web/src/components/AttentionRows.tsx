import { Link } from 'react-router-dom';
import type { AttentionItem } from '@/api/types';
import { Badge } from '@/components/ui/primitives';
import { ATTENTION_LABEL, ATTENTION_TONE, attentionDetail, attentionHref, attentionLogLine } from '@/lib/attention';
import { fmtRelative } from '@/lib/format';

/**
 * A pool card's conditions in the route's order, which is the ranking; a subject links to where it is fixed, and a
 * row pointing at the pool's own card stays put, since the card it sits on is that place. The pool's exhaustion is
 * the exception: it opens the routing log, as the dashboard's item does.
 */
export function AttentionRows({ items, now }: { items: AttentionItem[]; now: number }) {
  if (items.length === 0) return null;
  return (
    <ul className="attention">
      {items.map((item) => {
        const { subject } = item;
        const to = item.link?.page === 'pools' && item.kind !== 'pool_exhausted' ? null : attentionHref(item, now);
        const detail = attentionDetail(item, now);
        const log = attentionLogLine(item, now);
        return (
          <li key={`${item.kind}:${subject.type}:${subject.id}`} className="attention__row" title={`${detail} (${fmtRelative(item.since, now)})${log === null ? '' : `\n${log}`}`}>
            <Badge tone={ATTENTION_TONE[item.kind]}>{ATTENTION_LABEL[item.kind]}</Badge>
            {to ? <Link to={to}>{subject.label}</Link> : <span>{subject.label}</span>}
            <span className="attention__detail">{detail}</span>
          </li>
        );
      })}
    </ul>
  );
}
