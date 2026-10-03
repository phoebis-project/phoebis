export type AttentionKind = 'client_drift' | 'funding_required' | 'poll_backoff' | 'pool_exhausted' | 'reauth_required' | 'refresh_failed';
export interface AttentionItem {
  detail: string;
  kind: AttentionKind;
  /** When the condition started, ISO. */
  since: string;
  subject: { id: string; kind: 'account' | 'client' | 'key' | 'pool'; label: string };
}
export const ATTENTION_LABEL: Record<AttentionKind, string> = {
  client_drift: 'Client drift',
  funding_required: 'Funding required',
  poll_backoff: 'Usage poll backing off',
  pool_exhausted: 'Pool exhausted',
  reauth_required: 'Re-auth required',
  refresh_failed: 'Refresh failed',
};
export const ATTENTION_TONE: Record<AttentionKind, 'critical' | 'serious' | 'warning'> = {
  client_drift: 'warning',
  funding_required: 'serious',
  poll_backoff: 'warning',
  pool_exhausted: 'critical',
  reauth_required: 'critical',
  refresh_failed: 'critical',
};
// Worst first, so not alphabetical.
const ORDER: AttentionKind[] = ['reauth_required', 'refresh_failed', 'pool_exhausted', 'funding_required', 'poll_backoff', 'client_drift'];
/** Worst first (the three critical kinds, the serious one, the two warnings), the oldest first within a kind. */
export const sortAttention = (items: AttentionItem[]): AttentionItem[] => [...items].sort((a, b) => ORDER.indexOf(a.kind) - ORDER.indexOf(b.kind) || Date.parse(a.since) - Date.parse(b.since));
