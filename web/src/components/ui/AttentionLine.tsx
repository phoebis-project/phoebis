import { useState } from 'react';
import { api } from '@/api';
import type { AttentionItem } from '@/api/types';
import { ATTENTION_LABEL, ATTENTION_TONE, attentionHref, attentionTitle, attentionWords } from '@/lib/attention';
import { fmtInt, fmtRelative } from '@/lib/format';
import { Badge, BadgeLink, Pill, Skeleton } from './primitives';

/**
 * The dashboard's line above the tiles: what needs the operator, worst first, each a badge that opens where it is
 * fixed. The loading, quiet and failed states keep the one-row height, so the tiles hold still between them; a
 * populated line wraps to a second row once its badges outgrow the first. A failed read never draws the quiet line,
 * which would claim a health the read did not see.
 *
 * While a parked record shows, the line carries the Replay button after its badges. Its answer, or the route's
 * refusal, reads in the slot beside it rather than a toast, and an answer reads the list again through `refresh`, so
 * the parked badge clears without waiting for the next tick.
 */
export function AttentionLine({ error, items, loadedAt, now, refresh }: { error: string | null; items: AttentionItem[] | null; loadedAt: number | null; now: number; refresh: () => Promise<void> }) {
  const [replaying, setReplaying] = useState(false);
  const [replayed, setReplayed] = useState<string | null>(null);
  const replay = async () => {
    setReplaying(true);
    try {
      const { landed, remaining } = await api.replayParked();
      setReplayed(`Landed ${fmtInt(landed)}, ${fmtInt(remaining)} still parked`);
      await refresh();
    } catch (e) {
      setReplayed(e instanceof Error ? e.message : String(e));
    } finally {
      setReplaying(false);
    }
  };
  const slot = replayed === null ? null : <span className="attention-line__replay">{replayed}</span>;
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
        {slot}
      </div>
    );
  }
  return (
    <div className="attention-line">
      <span className="attention-line__lead">
        {items.length} {items.length === 1 ? 'needs' : 'need'} attention
      </span>
      {items.map((item) => {
        const text = attentionWords(item, now) ?? `${ATTENTION_LABEL[item.kind]}: ${item.subject.label}`;
        const title = attentionTitle(item, now);
        const href = attentionHref(item, now);
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
      {items.some((item) => item.kind === 'ledger_record_parked') && (
        <Pill onClick={() => void replay()} disabled={replaying}>
          Replay parked records
        </Pill>
      )}
      {slot}
    </div>
  );
}
