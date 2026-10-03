import type { AttentionItem } from '@/api/types';
import { QUOTA_SNAPSHOTS, accountById } from './accounts';
import { KEYS, POOLS } from './pools';
import { ATTENTION_ITEM_SAMPLE } from './samples';
import { DAY, HOUR, MINUTE, ago } from './time';

const ms = (at: string | null | undefined) => (at ? Date.parse(at) : null);

/** The account as the route names it, with the pools it is a member of, which a pool card filters by. */
const ofAccount = (id: string): Pick<AttentionItem, 'link' | 'pools' | 'subject'> => ({
  link: { id, page: 'accounts' },
  pools: POOLS.filter((p) => p.members.some((m) => m.accountId === id)).map((p) => p.id),
  subject: { id, label: accountById.get(id)?.label ?? id, type: 'account' },
});

const mailPoll = QUOTA_SNAPSHOTS.find((s) => s.accountId === 'mail')?.poll ?? null;

/**
 * One item per kind on the story's accounts, pools and keys, in the route's order: codex exhausted since the runner's
 * key last asked it, a client build refused on the laptop key, team's lapsed credential, personal out of funding as
 * its account reads, team's refreshes failing, and the backoff mail's poll is in.
 */
export const ATTENTION: AttentionItem[] = [
  {
    ...ATTENTION_ITEM_SAMPLE,
    detail: { key: 'ci', pool: 'codex' },
    kind: 'pool_exhausted',
    link: { id: 'codex', page: 'pools' },
    pools: ['codex'],
    since: ago(2 * DAY),
    subject: { id: 'codex', label: 'codex', type: 'pool' },
  },
  {
    ...ATTENTION_ITEM_SAMPLE,
    detail: { client_version: '2.1.271', key: 'laptop', trigger: 'hash' },
    kind: 'client_drift',
    link: { id: 'laptop', page: 'keys' },
    pools: KEYS.filter((k) => k.id === 'laptop').map((k) => k.poolId),
    since: ago(25 * MINUTE),
    subject: { id: '2.1.271', label: '2.1.271', type: 'client' },
  },
  {
    ...ATTENTION_ITEM_SAMPLE,
    ...ofAccount('team'),
    detail: { account: 'team', label: 'team' },
    kind: 'reauth_required',
    since: ago(2 * DAY + 2 * HOUR),
  },
  {
    ...ATTENTION_ITEM_SAMPLE,
    ...ofAccount('personal'),
    detail: { account: 'personal', label: 'personal' },
    kind: 'funding_required',
    since: ago(3 * HOUR),
  },
  {
    ...ATTENTION_ITEM_SAMPLE,
    ...ofAccount('team'),
    detail: { account: 'team', failures: 5, last_error: 'refresh_token rejected: invalid_grant', outcome: 'failed' },
    kind: 'refresh_failed',
    since: ago(2 * DAY + 2 * HOUR + 50 * MINUTE),
  },
  {
    ...ATTENTION_ITEM_SAMPLE,
    ...ofAccount('mail'),
    detail: { account: 'mail', backoff_since: ms(mailPoll?.backoffSince), next_attempt_at: ms(mailPoll?.nextAttemptAt), rate_limited_in_a_row: mailPoll?.rateLimitedInARow ?? 0 },
    kind: 'poll_backoff',
    since: mailPoll?.backoffSince ?? ago(42 * MINUTE),
  },
];
