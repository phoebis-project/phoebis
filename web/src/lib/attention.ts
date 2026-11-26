import { isoOrNull, list, num, numOrNull, rec, strings } from '@/api/decode';
import type { AttentionItem, AttentionKind } from '@/api/types';
import { accountMeterHref, quotaCardHref } from './address';
import { lookup } from './counts';
import { fmtAbsolute, fmtPct, fmtRelative, instant as clockInstant, joinPresent, listWords, messageOf, noun, percentText, plural } from './format';
import { clearsWords } from './resets';
import { entityHref, formatFact, logHref, sinceCondition, type LogLink } from './routingLog';
import { windowTag } from './windows';

export const ATTENTION_LABEL: Record<AttentionKind, string> = {
  certificate_expiring: 'Certificate expiring',
  certificate_reload_failed: 'Certificate reload failed',
  client_drift: 'Client drift',
  destination_spent: 'Destination spent within the hour',
  expired_between_turns: 'Expired between turns',
  feature_account_unavailable: 'Feature account unavailable',
  funding_required: 'Funding required',
  keepalive_bus_gaps: 'Keepalive bus gaps',
  keepalive_cap: 'Keepalive cap',
  ledger_record_parked: 'Priced record parked',
  limit_reset_ending: 'Reset ending',
  meter_drift: 'Meter drift against the table',
  model_unpriced: 'Model unpriced',
  pairing_expiring: 'Refresh token expiring',
  poll_backoff: 'Usage poll backing off',
  pool_exhausted: 'Pool exhausted',
  reauth_required: 'Re-auth required',
  refresh_failed: 'Credential refresh failed',
  session_cap: 'Session engine cap',
  shed_no_runway: 'Rounds without a destination',
  slowed_long: 'Slowed past twice the stretch',
  unknown: 'Unknown condition',
  wall_reached: 'Wall reached',
  writer_degraded: 'State writer degraded',
};
/** Critical where requests fail now, serious where they fail soon or lose data, warning for the rest, info for a
 *  condition that costs nothing and asks for a table entry. */
export const ATTENTION_TONE: Record<AttentionKind, 'critical' | 'info' | 'serious' | 'warning'> = {
  certificate_expiring: 'warning',
  certificate_reload_failed: 'serious',
  client_drift: 'serious',
  destination_spent: 'warning',
  expired_between_turns: 'warning',
  feature_account_unavailable: 'serious',
  funding_required: 'serious',
  keepalive_bus_gaps: 'serious',
  keepalive_cap: 'warning',
  ledger_record_parked: 'serious',
  limit_reset_ending: 'warning',
  meter_drift: 'warning',
  model_unpriced: 'info',
  pairing_expiring: 'warning',
  poll_backoff: 'warning',
  pool_exhausted: 'critical',
  reauth_required: 'critical',
  refresh_failed: 'serious',
  session_cap: 'warning',
  shed_no_runway: 'warning',
  slowed_long: 'warning',
  unknown: 'warning',
  wall_reached: 'warning',
  writer_degraded: 'serious',
};

/** How often a page with a reader open asks the route again, so an exhaustion or a cleared failure shows unprompted. */
export const ATTENTION_REFRESH_MS = 30_000;

type LinkedItem = Pick<AttentionItem, 'detail' | 'kind' | 'link' | 'since'>;

/** Whether a wall's detail carries a reset that can clear it now, which the route joins at the read. */
const resettable = ({ detail, kind }: LinkedItem): boolean => kind === 'wall_reached' && typeof detail.reset === 'object' && detail.reset !== null;

/**
 * The pool whose exhausted decisions a `pool_exhausted` item opens in the routing log. The pool link alone names the
 * log, so a link to any other page opens none.
 */
const exhaustedPool = ({ kind, link }: LinkedItem): string | null => (kind === 'pool_exhausted' && link?.page === 'pools' ? link.id : null);

/**
 * The routing log a routing condition opens, on the subject's rows of the kinds that make it hold, with the words that
 * name them, null for any other kind, a link that names no subject of the kind's own, or a wall a reset can clear,
 * which opens the account's Quota card.
 */
function conditionLog(item: LinkedItem): { log: Pick<LogLink, 'accounts' | 'kinds' | 'pools'>; words: string } | null {
  const { kind, link } = item;
  if (link === null) return null;
  if (kind === 'shed_no_runway' && link.page === 'pools') return { log: { kinds: ['shedding_round'], pools: [link.id] }, words: `pool ${link.id}, shedding rounds` };
  if (link.page !== 'accounts') return null;
  if (kind === 'destination_spent') return { log: { accounts: [link.id], kinds: ['pin_migrated', 'window_spent'] }, words: `account ${link.id}, moves and spent windows` };
  if (kind === 'wall_reached') return resettable(item) ? null : { log: { accounts: [link.id], kinds: ['window_spent'] }, words: `account ${link.id}, spent windows` };
  if (kind === 'slowed_long') return { log: { accounts: [link.id], kinds: ['slow_down_ended', 'slow_down_started'] }, words: `account ${link.id}, slow-downs` };
  return null;
}

/** The entity each of the route's three link pages opens, in the kind entityHref takes. */
const PAGE_ENTITY = { accounts: 'account', keys: 'key', pools: 'pool' } as const;

const SESSIONS_KINDS: ReadonlySet<AttentionKind> = new Set(['expired_between_turns', 'keepalive_bus_gaps', 'keepalive_cap', 'session_cap']);

/** An item's key in a list. The raw kind keeps two conditions the console does not know apart on one subject. */
export const attentionKey = (item: Pick<AttentionItem, 'kind' | 'rawKind' | 'subject'>): string => `${item.rawKind ?? item.kind}:${item.subject.type}:${item.subject.id}`;

/**
 * A `pool_exhausted` item opens the routing log on the pool's exhausted decisions from the condition's start, and a
 * routing condition on its subject's rounds, moves, walls and slow-downs since then. A reset that ends soon and a wall
 * a reset can clear open the account's Quota card. A meter drift opens the Meter view narrowed to the account, where
 * its reconciled windows are listed. The session engine's and the cache warming's conditions open the Sessions view,
 * though their link names no page. Every other kind opens the account, pool or key where it is fixed. Null when the
 * item names none, and for a kind this console does not know whatever its link, since the console cannot say what the
 * page would fix.
 */
export function attentionHref(item: LinkedItem, now: number): string | null {
  const { link } = item;
  if (SESSIONS_KINDS.has(item.kind)) return '/sessions';
  if (link === null || item.kind === 'unknown') return null;
  const since = sinceCondition(Date.parse(item.since), now);
  const pool = exhaustedPool(item);
  if (pool !== null) return logHref({ pools: [pool], kinds: ['exhausted'], ...since });
  const condition = conditionLog(item);
  if (condition !== null) return logHref({ ...condition.log, ...since });
  // The card's Use reset is where either one is acted on, so both open the card and neither opens the log.
  if ((item.kind === 'limit_reset_ending' || resettable(item)) && link.page === 'accounts') return quotaCardHref(link.id);
  if (item.kind === 'meter_drift' && link.page === 'accounts') return accountMeterHref(link.id);
  return entityHref(PAGE_ENTITY[link.page], link.id);
}

/**
 * The last line of a title whose item opens the routing log, naming what the log opens on, the start with its date
 * once it is a day away, or null for any other item.
 */
export function attentionLogLine(item: LinkedItem, now: number): string | null {
  const start = clockInstant(Date.parse(item.since), now);
  const pool = exhaustedPool(item);
  if (pool !== null) return `Opens the Routing log: pool ${pool}, exhausted, since ${start}`;
  const condition = conditionLog(item);
  return condition === null ? null : `Opens the Routing log: ${condition.words}, since ${start}`;
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
/** The listener a certificate item's subject names: `proxy.tls` is the proxy listener's pair. */
const listenerOf = ({ subject }: AttentionItem): string => subject.id.replace(/\.tls$/, '');
/** The subject the feature CA's certificate item carries. */
const FEATURE_CA = 'features.ca';
/** "a reset ends in 4 days", "the first of 2 resets ends today": the item's subject and its whole days. */
const resetEnds = (d: AttentionItem['detail']): string => {
  const ending = num(d.ending);
  const left = num(d.days_left);
  // The route floors the days left, so a reset at 0 ends within a day, as the certificate's item reads its last.
  return `${ending > 1 ? `the first of ${ending} resets` : 'a reset'} ${left <= 0 ? 'ends today' : `ends in ${plural(left, 'day')}`}`;
};
/** An end by the route's floored days left: 0 ends within a day, and -1 ended within the last day. */
const endWords = (left: number): string => (left === 0 ? 'expires today' : left === -1 ? 'expired today' : left < 0 ? `expired ${plural(-left, 'day')} ago` : `expires in ${plural(left, 'day')}`);

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
      return d.component === 'ledger' ? 'ledger batches are being retried, and budgeted members are held back until a batch lands' : 'routing state is kept in memory until the registry file takes writes again';
    case 'reauth_required':
      return 'serves nothing until its credential is renewed';
    case 'funding_required':
      return 'serves nothing until its organization is funded again';
    case 'feature_account_unavailable': {
      const refused = `the feature calls of ${plural(list(d.selections).length, 'selection')} are refused`;
      return d.obstacle === 'ended' ? `its plan ended, and ${refused}` : `it is signed out, and ${refused}`;
    }
    case 'refresh_failed': {
      const error = text(d.last_error);
      return `${num(d.failures)} refreshes failed in a row${error ? `: ${error}` : ''}`;
    }
    case 'ledger_record_parked': {
      const n = num(d.count);
      const accounts = strings(d.accounts);
      return `${plural(n, 'priced record')} parked, landing at the next start or on the dashboard's Replay${accounts.length ? ` (${accounts.join(', ')})` : ''}`;
    }
    case 'poll_backoff': {
      const n = num(d.rate_limited_in_a_row);
      const next = isoOrNull(d.next_attempt_at);
      return `${plural(n, 'rate-limited answer')} in a row${next ? `, next attempt ${fmtRelative(next, now)}` : ''}`;
    }
    case 'pairing_expiring': {
      const left = num(d.days_left);
      return `${text(d.label) ?? item.subject.label}'s refresh token ${endWords(left)} (${fmtAbsolute(isoOrNull(d.expires))}). ${left < 0 ? 'Re-pair the account' : 'Re-pair the account before then'}.`;
    }
    case 'certificate_expiring': {
      const file = text(d.cert_file);
      const ends = `${file ? ` ${file}` : ''} ${endWords(num(d.days_left))} (${fmtAbsolute(isoOrNull(d.expires))})`;
      // No reload reads the CA, and a new one has a new fingerprint each launcher must be given.
      if (item.subject.id === FEATURE_CA) {
        return `The feature CA${ends}. To replace it, remove feature-ca.pem and feature-ca-key.pem from the data directory, start the router again, and give each user the new fingerprint from the Settings view.`;
      }
      return `The ${listenerOf(item)} listener's certificate${ends}. Replace the pair and run phoebis tls reload.`;
    }
    case 'certificate_reload_failed':
      return `The ${listenerOf(item)} listener's pair did not load: ${text(d.error) ?? ''}. The router keeps serving the pair it loaded at ${fmtAbsolute(isoOrNull(d.serving_since))}.`;
    case 'limit_reset_ending': {
      const words = clearsWords(strings(d.clears));
      return `${resetEnds(d)} (${fmtAbsolute(isoOrNull(d.ends_at))})${words === null ? '' : ` and clears ${words}`}: ${text(d.grant_label) ?? ''}`;
    }
    case 'model_unpriced': {
      const n = num(d.requests);
      const model = text(d.model) ?? item.subject.id;
      if (n === 0) return `${model} has no weight in the cost model, so a plan-funded request on it will price no window share.`;
      return `${model} has no weight in the cost model, so its ${plural(n, 'plan-funded request')} since ${fmtAbsolute(item.since)} priced no window share.`;
    }
    case 'wall_reached':
    case 'destination_spent':
    case 'shed_no_runway':
      return conditionWords(item, now);
    case 'slowed_long':
      return slowedWords(item, now);
    case 'meter_drift':
      return driftWords(item, now);
    case 'keepalive_bus_gaps': {
      const last = isoOrNull(d.last_at);
      return `The keepalive subscriber fell behind the router's in-process request bus and discarded the observations it had open${last ? `, the last time ${fmtRelative(last, now)}` : ''}. Opens the Sessions view.`;
    }
    case 'keepalive_cap':
      return 'The cache warming is near its retained-bytes cap and evicts the oldest idle recipe at it. Opens the Sessions view.';
    case 'session_cap':
      return 'The session engine is near its cap and will refuse new records at it. Opens the Sessions view.';
    case 'expired_between_turns':
      return 'Prompt caches expired between two turns on a pool whose keepalive policy is on, under a key that holds the keepalive permission, so a probe came too late or not at all. Opens the Sessions view.';
    case 'unknown':
      return `a condition this console does not know (${item.rawKind ?? 'unknown'})`;
  }
}

/** The spent windows as the record names them, `5h`, `7d` or `Fable`, a key the console does not know as sent. */
const windowsOf = (windows: string[]): string => {
  const words = clearsWords(windows);
  return words === null ? '' : ` the ${words} ${noun(windows.length, 'window')}`;
};

/** The settings a condition names, each with the pool's value where the route sends one, printed by `value`. */
const settingWords = (d: AttentionItem['detail'], value: (n: number) => string): string => {
  const values = rec(d.setting_values);
  return listWords(
    strings(d.setting).map((name) => {
      const v = values[name];
      return typeof v === 'number' ? `${name} ${value(v)}` : name;
    }),
  );
};
const sinceWords = (item: AttentionItem, now: number): string => `since ${clockInstant(Date.parse(item.since), now)}`;

/**
 * A routing condition's count, the settings it names with the pool's values where the route sends them, and its start,
 * without the subject.
 */
function conditionWords(item: AttentionItem, now: number): string {
  const d = item.detail;
  const settings = settingWords(d, percentText);
  const what = (() => {
    switch (item.kind) {
      case 'shed_no_runway':
        return `${plural(num(d.rounds), 'round')} marked nothing, no destination had runway`;
      case 'destination_spent': {
        const spentWindows = strings(d.windows);
        const spent = windowsOf(spentWindows);
        return `${plural(num(d.moves), 'move')} landed${spent ? ` and${spent} ${noun(spentWindows.length, 'was', 'were')} spent` : ''} within the hour`;
      }
      default: {
        const projectionError = numOrNull(d.projection_error);
        const error = projectionError === null ? '' : `, the last round projected ${percentText(1 - projectionError)}`;
        // A slow-down round paces the window to its reset, so its wall reads how early it came instead.
        const earlyMs = numOrNull(d.early_ms);
        const early = earlyMs === null ? null : earlyMs > 0 ? `, the wall came ${formatFact(earlyMs, 'ms', now)} before the reset` : ', the wall came at the reset';
        const on = windowsOf(strings(d.windows));
        return `${plural(num(d.walls), 'wall')} reached${on ? ` on${on}` : ''}${early ?? error}${resettable(item) ? ', a reset can clear it' : ''}`;
      }
    }
  })();
  return joinPresent([what, settings, sinceWords(item, now)], ', ');
}

/**
 * A slowed account's held time of the day over the bar it passed, the held count and the stretch it is held to,
 * without the subject. The setting's value is a whole number of seconds, which `conditionWords` would print as a
 * percent.
 */
function slowedWords(item: AttentionItem, now: number): string {
  const d = item.detail;
  const settings = settingWords(d, (n) => String(Math.round(n)));
  const barMs = numOrNull(d.bar_ms);
  const held = `held ${formatFact(num(d.held_ms), 'ms', now)} of the last 24 h${barMs === null ? '' : ` over ${formatFact(barMs, 'ms', now)}`}`;
  return joinPresent([held, `${plural(num(d.held), 'request')} held`, settings, sinceWords(item, now)], ', ');
}

/** A ratio as the meter drift words print it: two decimals, one trailing zero dropped (`2.0`, `1.25`, `0.8`). */
const ratioWord = (ratio: number): string => {
  const fixed = ratio.toFixed(2);
  return fixed.endsWith('0') ? fixed.slice(0, -1) : fixed;
};

/**
 * An account's off windows, each with the ratio the meter charged against the table, and the start, without the
 * subject. The first window carries the verb, the later ones their word and ratio alone, so the line stays short.
 */
function driftWords(item: AttentionItem, now: number): string {
  const windows = list(item.detail.windows).map(rec);
  const ratios = windows.map((w, i) => {
    const word = text(w.window) ?? '';
    const name = windowTag(word);
    const ratio = `${ratioWord(num(w.ratio))}x`;
    return i === 0 ? `${name} charged ${ratio} the table` : `${name} ${ratio}`;
  });
  return [...ratios, sinceWords(item, now)].join(', ');
}

/** The cap names in the words a cap item reads, a name the console does not know as sent. */
const CAP_WORD: Record<string, string> = { agents: 'agent records', retained: 'retained prompts', sessions: 'sessions' };

/**
 * A routing condition's or a meter drift's sentence as the dashboard's line draws it, its subject first, that of a
 * condition of the session engine or of the cache warming after its label, or null for any other kind, which keeps its
 * label and subject.
 */
export function attentionWords(item: AttentionItem, now: number): string | null {
  const d = item.detail;
  if (item.kind === 'keepalive_bus_gaps') return `${ATTENTION_LABEL[item.kind]}: ${num(d.gaps)} since the start`;
  if (item.kind === 'keepalive_cap' || item.kind === 'session_cap') {
    const cap = text(d.cap) ?? '';
    return `${ATTENTION_LABEL[item.kind]} at ${fmtPct(num(d.pct))}: ${lookup(CAP_WORD, cap) ?? cap}`;
  }
  if (item.kind === 'expired_between_turns') return `${ATTENTION_LABEL[item.kind]}: ${num(d.expired)} in 24h`;
  if (item.kind === 'slowed_long') return `${item.subject.label}: ${slowedWords(item, now)}`;
  if (item.kind === 'meter_drift') return `${item.subject.label}: ${driftWords(item, now)}`;
  if (item.kind === 'limit_reset_ending') return `${item.subject.label}: ${resetEnds(d)}`;
  if (item.kind !== 'wall_reached' && item.kind !== 'destination_spent' && item.kind !== 'shed_no_runway') return null;
  return `${item.subject.label}: ${conditionWords(item, now)}`;
}

/**
 * The detail's members as `name: value` lines in name order, an instant the route sends in milliseconds read as one,
 * then the age: the title of a badge on the dashboard's line. A member the route left null says nothing and is left
 * out. An item that carries no pool leads with its sentence, since no pool card's row draws it. An item that opens the
 * routing log ends with the line that says so. A kind this console does not know opens with the kind the route named,
 * pooled or not, since its badge's label cannot name it.
 */
export function attentionTitle(item: AttentionItem, now: number): string {
  const members = Object.entries(item.detail)
    .filter(([, v]) => v !== null && v !== undefined)
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([name, v]) => {
      const at = /_(at|since)$|^expires$/.test(name) ? isoOrNull(v) : null;
      // A window's end inside an object member is an instant too, dated in place so the object keeps its key order.
      const dated = (key: string, x: unknown): unknown => {
        const end = key === 'window_end' ? isoOrNull(x) : null;
        return end ? fmtAbsolute(end) : x;
      };
      return `${name}: ${at ? fmtAbsolute(at) : typeof v === 'object' ? JSON.stringify(v, dated) : String(v)}`;
    });
  const kind = item.kind === 'unknown' ? [`kind: ${item.rawKind}`] : [];
  const sentence = item.pools.length === 0 ? [attentionDetail(item, now)] : [];
  const log = attentionLogLine(item, now);
  return [...kind, ...sentence, ...members, `started ${fmtRelative(item.since, now)}`, ...(log === null ? [] : [log])].join('\n');
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
 * regaining focus reads again, and the last one leaving stops both. A tick or a focus that finds a read still out is
 * skipped, so a read slower than the period lands and the tick after it reads again. Nothing runs before a
 * subscription, since the node test bundle loads this module with no window and no reader. `refresh` reads once now,
 * for a write that changed what the route answers, even beside a read still out, whose older answer then lands nothing.
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
  let out = 0;
  const publish = (next: AttentionRead) => {
    state = next;
    for (const listener of listeners) listener();
  };
  const refresh = async () => {
    const mine = (started += 1);
    out += 1;
    try {
      const items = await read();
      // A slow read that lands after a newer one started would put an older list back.
      if (mine === started) publish({ error: null, items, loadedAt: Date.now() });
    } catch (e) {
      if (mine === started) publish({ ...state, error: messageOf(e) });
    } finally {
      out -= 1;
    }
  };
  const tick = () => {
    if (out === 0) void refresh();
  };
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
