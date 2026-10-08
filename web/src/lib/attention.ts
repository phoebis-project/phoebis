import type { AttentionItem, AttentionKind } from '@/api/types';
import { fmtAbsolute, fmtRelative } from './format';
import { instant as clockInstant, logHref, sinceCondition } from './routingLog';

export const ATTENTION_LABEL: Record<AttentionKind, string> = {
  certificate_expiring: 'Certificate expiring',
  certificate_reload_failed: 'Certificate reload failed',
  client_drift: 'Client drift',
  funding_required: 'Funding required',
  ledger_record_parked: 'Priced record parked',
  poll_backoff: 'Usage poll backing off',
  pool_exhausted: 'Pool exhausted',
  reauth_required: 'Re-auth required',
  refresh_failed: 'Credential refresh failed',
  writer_degraded: 'State writer degraded',
};
export const ATTENTION_TONE: Record<AttentionKind, 'critical' | 'serious' | 'warning'> = {
  certificate_expiring: 'warning',
  certificate_reload_failed: 'serious',
  client_drift: 'serious',
  funding_required: 'serious',
  ledger_record_parked: 'serious',
  poll_backoff: 'warning',
  pool_exhausted: 'critical',
  reauth_required: 'critical',
  refresh_failed: 'critical',
  writer_degraded: 'serious',
};

/** How often a page with a reader open asks the route again, so an exhaustion or a cleared failure shows unprompted. */
export const ATTENTION_REFRESH_MS = 30_000;

type LinkedItem = Pick<AttentionItem, 'kind' | 'link' | 'since'>;

/**
 * The pool whose exhausted decisions the item opens in the routing log. The decoder reads a kind this build does not
 * know as `pool_exhausted`, so the link must name a pool as well, or an account's id would be read as one.
 */
const exhaustedPool = ({ kind, link }: LinkedItem): string | null => (kind === 'pool_exhausted' && link?.page === 'pools' ? link.id : null);

/**
 * A `pool_exhausted` item opens the routing log on the pool's exhausted decisions from the condition's start. Every
 * other kind opens the account, pool or key where it is fixed. Null when the item names none.
 */
export function attentionHref(item: LinkedItem, now: number): string | null {
  const { link } = item;
  if (link === null) return null;
  const pool = exhaustedPool(item);
  if (pool !== null) return logHref({ pools: [pool], kinds: ['exhausted'], ...sinceCondition(Date.parse(item.since), now) });
  const id = encodeURIComponent(link.id);
  return link.page === 'accounts' ? `/accounts/${id}` : `/${link.page}#${id}`;
}

/**
 * The last line of a title whose item opens the routing log, naming what the log opens on, the start with its date
 * once it is a day away, or null for any other item.
 */
export function attentionLogLine(item: LinkedItem, now: number): string | null {
  const pool = exhaustedPool(item);
  return pool === null ? null : `Opens the Routing log: pool ${pool}, exhausted, since ${clockInstant(Date.parse(item.since), now)}`;
}

/** The id an address's hash names, the way `attentionHref` wrote it; null for no hash or one that does not decode. */
export function anchorOf(hash: string): string | null {
  if (hash.length < 2) return null;
  try {
    return decodeURIComponent(hash.slice(1));
  } catch {
    return null;
  }
}

const text = (v: unknown): string | null => (typeof v === 'string' && v !== '' ? v : null);
const count = (v: unknown): number => (typeof v === 'number' && Number.isFinite(v) ? v : 0);
const instant = (v: unknown): string | null => (typeof v === 'number' && Number.isFinite(v) ? new Date(v).toISOString() : null);
const days = (n: number): string => `${n} ${n === 1 ? 'day' : 'days'}`;
/** The listener a certificate item's subject names: `proxy.tls` is the proxy listener's pair. */
const listenerOf = ({ subject }: AttentionItem): string => subject.id.replace(/\.tls$/, '');

/** The item's detail as the one line a pool card's row draws. */
export function attentionDetail(item: AttentionItem, now: number): string {
  const d = item.detail;
  switch (item.kind) {
    case 'pool_exhausted': {
      const key = text(d.key);
      return key ? `no member could take a request from key ${key}` : 'no member could take a request';
    }
    case 'client_drift': {
      const what = d.trigger === 'hash' ? 'its billing hash' : d.trigger === 'version_suffix' ? 'its version suffix' : 'its build';
      const key = text(d.key);
      return `${what} differs from the pinned build${key ? `, newest refusal from key ${key}` : ''}`;
    }
    case 'writer_degraded':
      return d.component === 'ledger' ? 'ledger batches are being retried; budgeted members are held back until a batch lands' : 'routing state is kept in memory until the registry file takes writes again';
    case 'reauth_required':
      return 'serves nothing until its credential is renewed';
    case 'funding_required':
      return 'serves nothing until its organization is funded again';
    case 'refresh_failed': {
      const error = text(d.last_error);
      return `${count(d.failures)} refreshes failed in a row${error ? `: ${error}` : ''}`;
    }
    case 'ledger_record_parked': {
      const n = count(d.count);
      const accounts = Array.isArray(d.accounts) ? d.accounts.filter((a): a is string => typeof a === 'string') : [];
      return `${n} priced ${n === 1 ? 'record' : 'records'} parked, landing at the next start or on the dashboard's Replay${accounts.length ? ` (${accounts.join(', ')})` : ''}`;
    }
    case 'poll_backoff': {
      const n = count(d.rate_limited_in_a_row);
      const next = instant(d.next_attempt_at);
      return `${n} rate-limited ${n === 1 ? 'answer' : 'answers'} in a row${next ? `, next attempt ${fmtRelative(next, now)}` : ''}`;
    }
    case 'certificate_expiring': {
      const left = count(d.days_left);
      const file = text(d.cert_file);
      const end = left < 0 ? `expired ${days(-left)} ago` : `expires in ${days(left)}`;
      return `The ${listenerOf(item)} listener's certificate${file ? ` ${file}` : ''} ${end} (${fmtAbsolute(instant(d.expires))}). Replace the pair and run phoebis tls reload.`;
    }
    case 'certificate_reload_failed':
      return `The ${listenerOf(item)} listener's pair did not load: ${text(d.error) ?? ''}. The router keeps serving the pair it loaded at ${fmtAbsolute(instant(d.serving_since))}.`;
  }
}

/**
 * The detail's members as `name: value` lines in name order, an instant the route sends in milliseconds read as one,
 * then the age: the title of a badge on the dashboard's line. A member the route left null says nothing and is left
 * out. An item that carries no pool leads with its sentence, since no pool card's row draws it. An item that opens the
 * routing log ends with the line that says so.
 */
export function attentionTitle(item: AttentionItem, now: number): string {
  const members = Object.entries(item.detail)
    .filter(([, v]) => v !== null && v !== undefined)
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([name, v]) => {
      const at = /_(at|since)$|^expires$/.test(name) ? instant(v) : null;
      return `${name}: ${at ? fmtAbsolute(at) : typeof v === 'object' ? JSON.stringify(v) : String(v)}`;
    });
  const sentence = item.pools.length === 0 ? [attentionDetail(item, now)] : [];
  const log = attentionLogLine(item, now);
  return [...sentence, ...members, `started ${fmtRelative(item.since, now)}`, ...(log === null ? [] : [log])].join('\n');
}

export interface AttentionRead {
  /** The last read's failure; null once a read answers. */
  error: string | null;
  /** The last list a read answered, kept through a failed read; null until the first answer. */
  items: AttentionItem[] | null;
  /** When `items` was read, in epoch milliseconds. */
  loadedAt: number | null;
}

/**
 * One schedule for every reader on the page: the first subscription reads at once and starts the interval, the window
 * regaining focus reads again, and the last one leaving stops both. Nothing runs before a subscription, since the
 * node test bundle loads this module with no window and no reader. `refresh` reads once now, for a write that changed
 * what the route answers.
 */
export function createAttentionPoll(read: () => Promise<AttentionItem[]>): {
  refresh: () => Promise<void>;
  snapshot: () => AttentionRead;
  subscribe: (listener: () => void) => () => void;
} {
  let state: AttentionRead = { error: null, items: null, loadedAt: null };
  const listeners = new Set<() => void>();
  let timer: ReturnType<typeof setInterval> | null = null;
  let started = 0;
  const publish = (next: AttentionRead) => {
    state = next;
    for (const listener of listeners) listener();
  };
  const refresh = async () => {
    const mine = (started += 1);
    try {
      const items = await read();
      // A slow read that lands after a newer one started would put an older list back.
      if (mine === started) publish({ error: null, items, loadedAt: Date.now() });
    } catch (e) {
      if (mine === started) publish({ ...state, error: e instanceof Error ? e.message : String(e) });
    }
  };
  const tick = () => void refresh();
  return {
    refresh,
    snapshot: (): AttentionRead => state,
    subscribe: (listener: () => void): (() => void) => {
      listeners.add(listener);
      if (listeners.size === 1) {
        void refresh();
        timer = setInterval(tick, ATTENTION_REFRESH_MS);
        if (typeof window !== 'undefined') window.addEventListener('focus', tick);
      }
      return () => {
        if (!listeners.delete(listener) || listeners.size > 0) return;
        if (timer !== null) clearInterval(timer);
        timer = null;
        if (typeof window !== 'undefined') window.removeEventListener('focus', tick);
      };
    },
  };
}
