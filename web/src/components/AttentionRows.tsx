import { Link } from 'react-router-dom';
import type { AttentionItem } from '@/api/types';
import { Badge } from '@/components/ui/primitives';
import { ATTENTION_LABEL, ATTENTION_TONE, attentionDetail, attentionHref } from '@/lib/attention';
import { fmtRelative } from '@/lib/format';

/**
 * A pool card's conditions in the route's order, which is the ranking; a subject links to where it is fixed, and the
 * pool's own row stays put, since the card it sits on is that place.
 */
export function AttentionRows({ items, now }: { items: AttentionItem[]; now: number }) {
  if (items.length === 0) return null;
  return (
    <ul className="attention">
      {items.map((item) => {
        const { subject } = item;
        const to = item.link?.page === 'pools' ? null : attentionHref(item);
        const detail = attentionDetail(item, now);
        return (
          <li key={`${item.kind}:${subject.type}:${subject.id}`} className="attention__row" title={`${detail} (${fmtRelative(item.since, now)})`}>
            <Badge tone={ATTENTION_TONE[item.kind]}>{ATTENTION_LABEL[item.kind]}</Badge>
            {to ? <Link to={to}>{subject.label}</Link> : <span>{subject.label}</span>}
            <span className="attention__detail">{detail}</span>
          </li>
        );
      })}
    </ul>
  );
}
