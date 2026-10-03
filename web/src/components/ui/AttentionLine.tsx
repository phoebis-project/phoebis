import { Link } from 'react-router-dom';
import type { AttentionItem } from '@/api/types';
import { ATTENTION_LABEL, ATTENTION_TONE, attentionHref, attentionTitle } from '@/lib/attention';
import { fmtRelative } from '@/lib/format';
import { Badge, Skeleton } from './primitives';

/**
 * The dashboard's line above the tiles: what needs the operator, worst first, each a badge that opens where it is
 * fixed. Loading, quiet and failed, it keeps the one line's height, so the tiles below never move for the state. A
 * failed read never draws the quiet line, which would claim a health the read did not see.
 */
export function AttentionLine({ items, error, loadedAt, now }: { error: string | null; items: AttentionItem[] | null; loadedAt: number | null; now: number }) {
  if (error !== null) {
    const title = loadedAt === null ? error : `${error}\nLast read: ${fmtRelative(new Date(loadedAt).toISOString(), now)}`;
    return (
      <div className="attention-line">
        <Badge tone="warning" title={title}>
          <span className="attention-line__cut">Attention unavailable: {error}</span>
        </Badge>
      </div>
    );
  }
  if (items === null) {
    return (
      <div className="attention-line">
        <Skeleton width={320} height={22} />
      </div>
    );
  }
  if (items.length === 0) {
    return (
      <div className="attention-line">
        <Badge tone="good">Nothing needs attention</Badge>
      </div>
    );
  }
  return (
    <div className="attention-line">
      <span className="attention-line__lead">
        {items.length} {items.length === 1 ? 'needs' : 'need'} attention
      </span>
      {items.map((item) => {
        const text = `${ATTENTION_LABEL[item.kind]}: ${item.subject.label}`;
        const title = attentionTitle(item, now);
        const href = attentionHref(item);
        const key = `${item.kind}:${item.subject.type}:${item.subject.id}`;
        return href ? (
          <Link key={key} to={href} className={`badge badge--${ATTENTION_TONE[item.kind]}`} title={title}>
            {text}
          </Link>
        ) : (
          <Badge key={key} tone={ATTENTION_TONE[item.kind]} title={title}>
            {text}
          </Badge>
        );
      })}
    </div>
  );
}
