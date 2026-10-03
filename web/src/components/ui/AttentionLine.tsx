import type { AttentionItem } from '@/api/types';
import { ATTENTION_LABEL, ATTENTION_TONE, attentionHref, attentionTitle } from '@/lib/attention';
import { fmtRelative } from '@/lib/format';
import { Badge, BadgeLink, Skeleton } from './primitives';

/**
 * The dashboard's line above the tiles: what needs the operator, worst first, each a badge that opens where it is
 * fixed. The loading, quiet and failed states keep the one-row height, so the tiles hold still between them; a
 * populated line wraps to a second row once its badges outgrow the first. A failed read never draws the quiet line,
 * which would claim a health the read did not see.
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
          <BadgeLink key={key} to={href} tone={ATTENTION_TONE[item.kind]} title={title}>
            {text}
          </BadgeLink>
        ) : (
          <Badge key={key} tone={ATTENTION_TONE[item.kind]} title={title}>
            {text}
          </Badge>
        );
      })}
    </div>
  );
}
