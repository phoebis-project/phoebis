import { Link } from 'react-router-dom';
import { Badge } from '@/components/ui/primitives';
import { ATTENTION_LABEL, ATTENTION_TONE, sortAttention, type AttentionItem } from '@/lib/attention';
import { fmtRelative } from '@/lib/format';

/** The conditions that need attention, worst first; a subject links to where it is fixed, a pool's own row stays put. */
export function AttentionRows({ items, now }: { items: AttentionItem[]; now: number }) {
  if (items.length === 0) return null;
  return (
    <ul className="attention">
      {sortAttention(items).map((item) => {
        const { subject } = item;
        const to = subject.kind === 'account' ? `/accounts/${subject.id}` : subject.kind === 'key' ? `/keys#${subject.id}` : null;
        return (
          <li key={`${item.kind}:${subject.kind}:${subject.id}`} className="attention__row" title={`${item.detail} (${fmtRelative(item.since, now)})`}>
            <Badge tone={ATTENTION_TONE[item.kind]}>{ATTENTION_LABEL[item.kind]}</Badge>
            {to ? <Link to={to}>{subject.label}</Link> : <span>{subject.label}</span>}
            <span className="attention__detail">{item.detail}</span>
          </li>
        );
      })}
    </ul>
  );
}
