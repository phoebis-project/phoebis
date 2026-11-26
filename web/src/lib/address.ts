import { iso } from '@/api/decode';
import type { Account, BurnGroup, KeyStatus, RequestKind, RequestOrder, RequestOutcome, SortDir } from '@/api/types';
import { REQUEST_KIND_VALUES, REQUEST_OUTCOME_VALUES } from '@/api/types';
import { CACHE_MISS_REASONS, missShort } from './cache';
import { clock } from './clock';
import { lookup } from './counts';
import { sameMembers } from './forms';
import type { SessionFilters } from './keepalive';
import { ALL_KINDS } from './kinds';
import { ACCOUNT_STATUSES } from './quota';
import { DAY, HOUR, MINUTE } from './time';
import type { UnitPosition } from './unit';

/** One preset of the range control's one table: the name the address carries, the width and the label. */
export interface RangePreset {
  label: string;
  ms: number;
  name: string;
}

/** The one preset table of every ranged view: the name the address carries, the width and the label for the titles. */
export const RANGE_PRESETS = [
  { label: 'Last hour', ms: HOUR, name: '1h' },
  { label: 'Last 6 hours', ms: 6 * HOUR, name: '6h' },
  { label: 'Last 24 hours', ms: DAY, name: '24h' },
  { label: 'Last 7 days', ms: 7 * DAY, name: '7d' },
  { label: 'Last 30 days', ms: 30 * DAY, name: '30d' },
] as const satisfies readonly RangePreset[];

/** The Active within values, each read against a row's last turn: its newest inference request, or its pin's last use
 *  before the first one. */
export const ACTIVE_WITHIN = [
  { id: '15m', label: '15 min', ms: 15 * MINUTE },
  { id: '1h', label: '1 h', ms: HOUR },
  { id: '6h', label: '6 h', ms: 6 * HOUR },
  { id: '24h', label: '24 h', ms: DAY },
] as const;

/** A view's window as the address carries it: a preset by name with the instants empty, or `custom` with both instants. */
export interface RangeValue {
  from: string;
  preset: string;
  to: string;
}

export type KeySpec =
  /** A value outside `values` reads as the default. */
  | { kind: 'choice'; default: string; name: string; values: readonly string[] }
  /** A number among `values`, written as its digits. A value outside them reads as the default. */
  | { kind: 'count'; default: number; name: string; values: readonly number[] }
  /** Written as `1`, read as true on `1`. */
  | { kind: 'flag'; name: string }
  /**
   * Comma-joined ids. An entry `accepts` refuses leaves the list, and `normalize` rewrites the list on read and
   * write.
   */
  | { accepts?: (value: string) => boolean; kind: 'list'; name: string; normalize?: (values: string[]) => string[] }
  /** Patterns joined by a newline, as the wire joins them, so a comma inside one rides. */
  | { kind: 'patterns'; name: string }
  /** The keys `range`, `from` and `to`. */
  | { kind: 'range'; default: string }
  | { kind: 'text'; name: string };

export type { SortDir } from '@/api/types';
export const SORT_DIRS = ['asc', 'desc'] as const;
export type SortDefault = SortDir | 'none';

/** One key of a view's sort control: its address value, its label and the direction a pick of it starts in. */
export interface SortKey<K extends string = string> {
  /** The direction a pick starts in, or `none` for a key with no direction, which draws no arrow and writes no
   *  `dir`. */
  defaultDir: SortDefault;
  id: K;
  label: string;
}

export interface SortFields<K extends string> {
  /** `asc`, `desc` or empty for the sort key's own default direction, which the address omits. */
  dir: '' | SortDir;
  sort: K;
}

/**
 * A view's `sort` and `dir` keys: `choice` keys with the view's default key as `sort`'s default and the empty value
 * as `dir`'s, which stands for the picked key's own default direction, so the address carries `dir` only where it
 * differs from that direction and `?sort=usage` alone keeps its meaning. `sort` comes first, since `codecOf` writes
 * the keys in the specification's order, so a spread writes `sort=...&dir=...`.
 */
export const sortSpecs = <K extends string>(keys: readonly SortKey<K>[], defaultKey: K): { sort: KeySpec; dir: KeySpec } => ({
  sort: { default: defaultKey, kind: 'choice', name: 'sort', values: keys.map((k) => k.id) },
  dir: { default: '', kind: 'choice', name: 'dir', values: SORT_DIRS },
});

/**
 * The direction an address means: its `dir` where it names one, else the sort key's own default. A key with no
 * direction reads `asc` whatever the address says, since nothing orders by the figure and a typed `dir` on it is inert.
 */
export const sortDirOf = <K extends string>(keys: readonly SortKey<K>[], address: SortFields<K>): SortDir => {
  const own = keys.find((k) => k.id === address.sort)?.defaultDir;
  if (own === 'none') return 'asc';
  return address.dir !== '' ? address.dir : (own ?? 'asc');
};

/** The address fields of a pick: `dir` empty where it is the key's own default, so the address omits it, and always
 *  empty for a key with no direction. */
export const sortAddressOf = <K extends string>(keys: readonly SortKey<K>[], next: SortFields<K>): SortFields<K> => {
  const own = keys.find((k) => k.id === next.sort)?.defaultDir;
  return { dir: own === 'none' || next.dir === own ? '' : next.dir, sort: next.sort };
};

export type Codec<F> = {
  /** The address with every field at its default, for a link that names one field and leaves the rest as the view's. */
  defaults: F;
  fromParams(params: URLSearchParams, now?: number): F;
  toParams(filters: F): URLSearchParams;
};

/** The instant a window key carries, or empty: any RFC 3339 instant parses, a bare integer reads as milliseconds. */
export const instantOf = (value: string | null): string => {
  if (!value) return '';
  const at = new Date(/^\d+$/.test(value) ? Number(value) : Date.parse(value));
  // An integer past the dates a `Date` holds reads as empty, since its ISO form would throw.
  return Number.isFinite(at.getTime()) ? at.toISOString() : '';
};

function readKey(key: KeySpec, params: URLSearchParams, now: number): unknown {
  switch (key.kind) {
    case 'choice': {
      const value = params.get(key.name);
      return value !== null && key.values.includes(value) ? value : key.default;
    }
    case 'count': {
      const value = params.get(key.name);
      return key.values.find((v) => String(v) === value) ?? key.default;
    }
    case 'flag':
      return params.get(key.name) === '1';
    case 'list': {
      const values = (params.get(key.name) ?? '').split(',').filter((v) => v !== '' && (key.accepts?.(v) ?? true));
      return key.normalize?.(values) ?? values;
    }
    case 'patterns':
      return (params.get(key.name) ?? '').split('\n').filter(Boolean);
    case 'range': {
      const from = instantOf(params.get('from'));
      const asked = instantOf(params.get('to'));
      // The end is capped before the pair is judged, as the range control caps it, so a pair wholly after now reads
      // as one that ends before it starts.
      const to = asked !== '' && Date.parse(asked) > now ? iso(now) : asked;
      // A pair is a span only while `from` precedes `to`, so a half, backward or unreadable pair reads as the preset.
      if (from !== '' && to !== '' && Date.parse(from) < Date.parse(to)) return { from, preset: 'custom', to } satisfies RangeValue;
      const name = params.get('range');
      return { from: '', preset: RANGE_PRESETS.some((p) => p.name === name) && name !== null ? name : key.default, to: '' } satisfies RangeValue;
    }
    case 'text':
      return params.get(key.name) ?? '';
  }
}

function writeKey(key: KeySpec, value: unknown, params: URLSearchParams): void {
  switch (key.kind) {
    case 'choice':
      if (value !== key.default) params.set(key.name, value as string);
      return;
    case 'count':
      if (value !== key.default) params.set(key.name, String(value));
      return;
    case 'flag':
      if (value === true) params.set(key.name, '1');
      return;
    case 'list': {
      const values = key.normalize?.(value as string[]) ?? (value as string[]);
      if (values.length > 0) params.set(key.name, values.join(','));
      return;
    }
    case 'patterns':
      if ((value as string[]).length > 0) params.set(key.name, (value as string[]).join('\n'));
      return;
    case 'range': {
      const range = value as RangeValue;
      if (range.preset === 'custom') {
        if (range.from) params.set('from', range.from);
        if (range.to) params.set('to', range.to);
      } else if (range.preset !== key.default) params.set('range', range.preset);
      return;
    }
    case 'text':
      if (value) params.set(key.name, value as string);
  }
}

/**
 * The grammar every filtered view reads its address through. A filter at its default leaves the address, and an
 * unknown key is dropped on the next write. A window's `from` and `to` are RFC 3339 UTC instants at millisecond
 * precision (`toISOString()`), read through `Date.parse`. A bare integer reads as Unix milliseconds, a form no page
 * writes. A `to` past `now`, the console clock's unless given, reads as `now`. The address lists the keys in the
 * specification's order.
 */
export function codecOf<F extends object>(spec: { [K in keyof F]: KeySpec }): Codec<F> {
  const keys = Object.entries(spec) as [string, KeySpec][];
  const fromParams = (params: URLSearchParams, now = clock.now()): F => Object.fromEntries(keys.map(([field, key]) => [field, readKey(key, params, now)])) as F;
  return {
    // An empty address names no instant, so the clock the range is capped at does not enter it.
    defaults: fromParams(new URLSearchParams()),
    fromParams,
    toParams(filters) {
      const params = new URLSearchParams();
      for (const [field, key] of keys) writeKey(key, (filters as Record<string, unknown>)[field], params);
      return params;
    },
  };
}

/** A preset's window from `now`, or a custom span's own instants. Named apart from quota.ts's windowOf, which the test entry also star-exports. */
export function addressWindow(range: RangeValue, fallback: string, now: number): { from: number; to: number } {
  if (range.preset === 'custom') {
    const from = Date.parse(range.from);
    const to = Date.parse(range.to);
    if (from < to) return { from, to };
  }
  const preset = RANGE_PRESETS.find((p) => p.name === range.preset) ?? RANGE_PRESETS.find((p) => p.name === fallback);
  return { from: now - (preset?.ms ?? 0), to: now };
}

const list = (name: string): KeySpec => ({ kind: 'list', name });
const text = (name: string): KeySpec => ({ kind: 'text', name });

export interface RequestsAddress extends SortFields<(typeof REQUEST_SORT_KEYS)[number]['id']> {
  accounts: string[];
  /** The user agent pattern. */
  agent: string;
  agentId: string;
  agentTypes: string[];
  cacheMissReasons: string[];
  /** `client`, `client_off`, `-` for a request the marker does not hold. */
  classifiers: string[];
  client: string;
  efforts: string[];
  /** `true` for the rows that presented a fallback token, `false` for the rest, empty for both. */
  fallback: '' | 'false' | 'true';
  /** Guardrail ids, `-` for a request no guardrail matched. */
  guardrails: string[];
  held: boolean;
  inputTransformations: string[];
  keys: string[];
  kind: '' | RequestKind;
  models: string[];
  outcomes: RequestOutcome[];
  pools: string[];
  range: RangeValue;
  /** The serving attempt's refusal category, `-` for none. */
  refusalCategories: string[];
  request: string;
  requestClasses: string[];
  /** The page size. */
  rows: number;
  search: string;
  session: string;
  /** The list's order, which the read pages by: `received` the default. */
  sort: (typeof REQUEST_SORT_KEYS)[number]['id'];
  speeds: string[];
  statuses: string[];
  /** The serving attempt's stop reason, `-` for none. */
  stopReasons: string[];
  turnOrigins: string[];
}

/** The ledger's words for an outcome, so one the address carries that names none leaves it. */
const isOutcome = (value: string): boolean => REQUEST_OUTCOME_VALUES.some((o) => o === value);
/** A status code is digits alone. */
const isStatus = (value: string): boolean => /^\d+$/.test(value);

const REQUEST_KINDS: readonly ('' | RequestKind)[] = ['', ...REQUEST_KIND_VALUES];
/** The Rows select's sizes and the codec's, one list, so every size the select offers reads back from the address. */
export const REQUEST_PAGE_SIZES = [10, 25, 50, 100] as const;

export const REQUEST_SORT_KEYS = [
  { defaultDir: 'desc', id: 'received', label: 'Time' },
  { defaultDir: 'desc', id: 'attempts', label: 'Attempts' },
  { defaultDir: 'desc', id: 'duration', label: 'Duration' },
  { defaultDir: 'desc', id: 'ttfb', label: 'TTFB' },
  { defaultDir: 'desc', id: 'input', label: 'Uncached input' },
  { defaultDir: 'desc', id: 'output', label: 'Output tokens' },
] as const satisfies readonly SortKey[];
/** The route's order word per key. */
export const REQUEST_ORDER_OF = { attempts: 'attempts', duration: 'duration_ms', input: 'input_tokens', output: 'output_tokens', received: 'received_at', ttfb: 'ttfb_ms' } as const satisfies Record<(typeof REQUEST_SORT_KEYS)[number]['id'], RequestOrder>;
/** The route's order words the console sends, which the poll's comparator reads. */
export type RequestOrderWord = (typeof REQUEST_ORDER_OF)[keyof typeof REQUEST_ORDER_OF];

export const REQUESTS_CODEC = codecOf<RequestsAddress>({
  search: text('search'),
  request: text('request'),
  session: text('session'),
  agentId: text('agent_id'),
  client: text('client'),
  agent: text('agent'),
  agentTypes: list('agent_types'),
  turnOrigins: list('turn_origins'),
  efforts: list('efforts'),
  requestClasses: list('request_classes'),
  classifiers: list('classifiers'),
  speeds: list('speeds'),
  held: { kind: 'flag', name: 'held' },
  fallback: { default: '', kind: 'choice', name: 'fallback', values: ['', 'true', 'false'] },
  keys: list('keys'),
  pools: list('pools'),
  accounts: list('accounts'),
  models: list('models'),
  statuses: { accepts: isStatus, kind: 'list', name: 'statuses' },
  outcomes: { accepts: isOutcome, kind: 'list', name: 'outcomes' },
  cacheMissReasons: list('cache_miss_reasons'),
  inputTransformations: list('input_transformations'),
  guardrails: list('guardrails'),
  refusalCategories: list('refusal_categories'),
  stopReasons: list('stop_reasons'),
  kind: { default: '', kind: 'choice', name: 'kind', values: REQUEST_KINDS },
  ...sortSpecs(REQUEST_SORT_KEYS, 'received'),
  rows: { default: 25, kind: 'count', name: 'rows', values: REQUEST_PAGE_SIZES },
  range: { default: '24h', kind: 'range' },
});
export const requestsHref = (address: RequestsAddress): string => `/requests?${REQUESTS_CODEC.toParams(address)}`;

export const accountHref = (id: string): string => `/accounts/${encodeURIComponent(id)}`;
export const keyHref = (id: string): string => `/keys#${encodeURIComponent(id)}`;
/** One request's own page, where `requestsHref` opens the filtered list. */
export const requestHref = (id: string): string => `/requests/${encodeURIComponent(id)}`;

export interface AnalyticsAddress extends SortFields<string> {
  accounts: string[];
  /** An agent of the session, which narrows the session's view to the agent's requests. */
  agentId: string;
  /** The group-by, the Burn section's own `session` among its values. */
  by: BurnGroup;
  /** The client patterns, any of which a request's client matches. */
  clients: string[];
  /** The user agent groups, one pattern per line. */
  groups: string;
  /** A session's key lifetime, `<key id>@<generation>`. */
  key: string;
  keys: string[];
  models: string[];
  pools: string[];
  probes: boolean;
  range: RangeValue;
  session: string;
  /** The group tables' order: `name`, or the id of a column of some tab's group table, kept while a tab without that
   *  column draws Name. */
  sort: string;
  /** A shared link's unit position, which the page reads once into the console-wide setting and drops. */
  unit: string;
  /** The user agent patterns, any of which a request's user agent matches. */
  userAgents: string[];
}

// A key table rather than a list, so a grouping the burn route gains fails to compile until it is listed here. The
// burn route's groupings are the breakdown's and its own `session`.
const GROUP_BY_KEYS = { account: true, classifier: true, client: true, effort: true, key: true, kind: true, model: true, pool: true, request_class: true, session: true, speed: true, user_agent: true } satisfies Record<BurnGroup, true>;
const GROUP_BYS = Object.keys(GROUP_BY_KEYS) as BurnGroup[];

const NAME_KEY = { defaultDir: 'asc', id: 'name', label: 'Name' } as const satisfies SortKey;
/** Every group table column holds a count, a rate, a cost or a duration, so each key starts descending. */
const column = (id: string, label: string): SortKey => ({ defaultDir: 'desc', id, label });
/** The known reasons alone, since the address's keys are fixed: a reason the console does not know draws its column
 *  with no key. */
const MISS_REASON_KEYS: readonly SortKey[] = CACHE_MISS_REASONS.map((reason) => column(reason, missShort(reason)));
/** The group table's keys per Analytics tab, in the order the Sort control lists them, the Cost tab's per unit. A tab
 *  without a group table has none. */
const ANALYTICS_SORT_KEYS: Readonly<Record<'cache' | 'cost_dollars' | 'cost_native' | 'latency' | 'traffic', readonly SortKey[]>> = {
  cache: [...MISS_REASON_KEYS, column('expired_between_turns', 'Expired between turns')],
  cost_dollars: [
    column('requests', 'Requests'),
    column('cost', 'Cost'),
    column('input', 'Input'),
    column('cache_read', 'Cache read'),
    column('cache_write', 'Cache write'),
    column('output', 'Output'),
    column('per_request', 'Per request'),
    column('per_1k_output', 'Per 1k output'),
  ],
  cost_native: [column('requests', 'Requests'), column('cost', 'Cost'), column('per_request', 'Per request'), column('per_1k_output', 'Per 1k output')],
  latency: [
    column('requests', 'Requests'),
    column('ttfb_p50', 'TTFB p50'),
    column('ttfb_p99', 'TTFB p99'),
    column('ttft_p99', 'TTFT p99'),
    column('duration_p99', 'Duration p99'),
    column('tps_p50', 'tok/s p50'),
  ],
  traffic: [
    column('requests', 'Requests'),
    column('ok', 'OK'),
    column('rate_limited', 'Rate limited'),
    column('failed', 'Failed'),
    column('incomplete', 'Incomplete'),
    column('client_canceled', 'Client canceled'),
    column('exhausted', 'Exhausted'),
    column('rejected', 'Rejected'),
    column('refused', 'Refused'),
    column('error_rate', 'Error rate'),
    column('failovers', 'Failovers'),
    column('search', 'Search'),
    column('fetch', 'Fetch'),
  ],
};
/** Every key an address may name, once per id, since the tabs share columns such as Requests. */
const ANALYTICS_ADDRESS_KEYS = [NAME_KEY, ...Object.values(ANALYTICS_SORT_KEYS).flat()].filter((k, i, all) => all.findIndex((o) => o.id === k.id) === i);
/**
 * The Sort control's keys on a tab under a unit: Name, then the tab's group table columns (the Cost tab's by the unit),
 * Name alone where the tab has no group table. A null tab names one whose group table the view leaves out.
 */
export const analyticsKeysOf = (tab: string | null, unit: UnitPosition): readonly SortKey[] => {
  if (tab === null) return [NAME_KEY];
  const own = lookup(ANALYTICS_SORT_KEYS, tab === 'cost' ? (unit === 'native' ? 'cost_native' : 'cost_dollars') : tab);
  return [NAME_KEY, ...(own ?? [])];
};

export const ANALYTICS_CODEC = codecOf<AnalyticsAddress>({
  session: text('session'),
  key: text('key'),
  agentId: text('agent_id'),
  accounts: list('accounts'),
  by: { default: 'key', kind: 'choice', name: 'by', values: GROUP_BYS },
  clients: { kind: 'patterns', name: 'clients' },
  groups: text('groups'),
  keys: list('keys'),
  models: list('models'),
  pools: list('pools'),
  probes: { kind: 'flag', name: 'probes' },
  unit: text('unit'),
  userAgents: { kind: 'patterns', name: 'user_agents' },
  ...sortSpecs(ANALYTICS_ADDRESS_KEYS, 'name'),
  range: { default: '24h', kind: 'range' },
});

/** The routing log's filters as the address holds them. */
export interface LogFilters extends SortFields<(typeof ROUTING_LOG_SORT_KEYS)[number]['id']> {
  accounts: string[];
  agentId: string;
  /** `asc`, `desc` or empty for the time's own newest first, which the address omits. */
  dir: '' | SortDir;
  keys: string[];
  kinds: string[];
  models: string[];
  pools: string[];
  range: RangeValue;
  request: string;
  session: string;
}

export const ROUTING_LOG_SORT_KEYS = [{ defaultDir: 'desc', id: 'time', label: 'Time' }] as const satisfies readonly SortKey[];

/**
 * The routing log's filters to and from its address. A kind this build does not know stays, so the route's refusal
 * reaches the page where a link from a newer router would otherwise read as every kind.
 */
export const ROUTING_LOG_CODEC = codecOf<LogFilters>({
  accounts: list('accounts'),
  agentId: text('agent_id'),
  keys: list('keys'),
  // An absent `kinds` reads every kind, so a list naming every kind is the empty one.
  kinds: { kind: 'list', name: 'kinds', normalize: (v) => (sameMembers(v, ALL_KINDS) ? [] : v) },
  models: list('models'),
  pools: list('pools'),
  request: text('request'),
  session: text('session'),
  ...sortSpecs(ROUTING_LOG_SORT_KEYS, 'time'),
  range: { default: '1h', kind: 'range' },
});

/** The Meter view's one filter. */
export interface MeterAddress {
  accounts: string[];
}

export const METER_CODEC = codecOf<MeterAddress>({ accounts: list('accounts') });
/** The Meter view on one account's groups. */
export const accountMeterHref = (accountId: string): string => `/meter?${METER_CODEC.toParams({ accounts: [accountId] })}`;
/** The Quota view on one account's card. */
export const quotaCardHref = (accountId: string): string => `/quota#${encodeURIComponent(accountId)}`;

export interface KeysAddress extends SortFields<(typeof KEY_SORT_KEYS)[number]['id']> {
  pools: string[];
  search: string;
  status: 'all' | KeyStatus;
}

const KEY_STATUSES: readonly ('all' | KeyStatus)[] = ['all', 'active', 'disabled', 'expired', 'revoked'];

export const KEY_SORT_KEYS = [
  NAME_KEY,
  { defaultDir: 'desc', id: 'created', label: 'Created' },
  { defaultDir: 'desc', id: 'lastUsed', label: 'Last used' },
  { defaultDir: 'desc', id: 'requests', label: 'Requests, 7d' },
  { defaultDir: 'desc', id: 'tokens', label: 'Tokens, 7d' },
  { defaultDir: 'desc', id: 'cost', label: 'Cost, 7d' },
  { defaultDir: 'asc', id: 'expiry', label: 'Expiry' },
  { defaultDir: 'asc', id: 'status', label: 'Status' },
] as const satisfies readonly SortKey[];

export const KEYS_CODEC = codecOf<KeysAddress>({
  pools: list('pools'),
  search: text('search'),
  ...sortSpecs(KEY_SORT_KEYS, 'name'),
  status: { default: 'all', kind: 'choice', name: 'status', values: KEY_STATUSES },
});
/** The Keys view on one pool's keys, every other filter at its default. */
export const poolKeysHref = (poolId: string): string => `/keys?${KEYS_CODEC.toParams({ ...KEYS_CODEC.defaults, pools: [poolId] })}`;

export type QuotaAddress = SortFields<(typeof QUOTA_SORT_KEYS)[number]['id']>;

export const QUOTA_SORT_KEYS = [
  { defaultDir: 'none', id: 'default', label: 'Default order' },
  { defaultDir: 'desc', id: 'pace5h', label: '5-hour pace' },
  { defaultDir: 'desc', id: 'pace7d', label: '7-day pace' },
  { defaultDir: 'desc', id: 'paceFable', label: '7-day Fable pace' },
  { defaultDir: 'asc', id: 'reset5h', label: '5-hour recovery' },
  { defaultDir: 'asc', id: 'reset7d', label: '7-day recovery' },
  { defaultDir: 'asc', id: 'resetFable', label: '7-day Fable recovery' },
] as const satisfies readonly SortKey[];

export const QUOTA_CODEC = codecOf<QuotaAddress>(sortSpecs(QUOTA_SORT_KEYS, 'default'));

export const POOL_SORT_KEYS = [
  NAME_KEY,
  { defaultDir: 'asc', id: 'status', label: 'Status' },
  { defaultDir: 'desc', id: 'members', label: 'Members' },
  { defaultDir: 'desc', id: 'requests', label: 'Requests, 24h' },
  { defaultDir: 'desc', id: 'cost', label: 'Cost, 24h' },
  { defaultDir: 'desc', id: 'pins', label: 'Active pins' },
] as const satisfies readonly SortKey[];

export type PoolsAddress = SortFields<(typeof POOL_SORT_KEYS)[number]['id']>;

export const POOLS_CODEC = codecOf<PoolsAddress>(sortSpecs(POOL_SORT_KEYS, 'name'));

export const GUARDRAIL_SORT_KEYS = [
  NAME_KEY,
  { defaultDir: 'asc', id: 'mode', label: 'Mode' },
  { defaultDir: 'desc', id: 'matches', label: 'Matches, 24h' },
  { defaultDir: 'desc', id: 'created', label: 'Created' },
  { defaultDir: 'desc', id: 'updated', label: 'Last updated' },
] as const satisfies readonly SortKey[];

export type GuardrailsAddress = SortFields<(typeof GUARDRAIL_SORT_KEYS)[number]['id']>;

export const GUARDRAILS_CODEC = codecOf<GuardrailsAddress>(sortSpecs(GUARDRAIL_SORT_KEYS, 'name'));

export const EDIT_PARAM = 'edit';
export const GROUP_PARAM = 'group';
export const EDIT_PARAMS = [EDIT_PARAM, GROUP_PARAM] as const;
/**
 * The view at `path` with the dialog of item `id` opened once on `group`, which `useAddressEdit` reads. `search` is the
 * view's current query string, so its order and filters ride along and the dialog's close returns to the same list.
 */
export const editHref = (path: string, id: string, group: string, search: string): string => {
  const params = new URLSearchParams(search);
  params.set(EDIT_PARAM, id);
  params.set(GROUP_PARAM, group);
  return `${path}?${params}`;
};

export interface AccountsAddress extends SortFields<(typeof ACCOUNT_SORT_KEYS)[number]['id']> {
  search: string;
  /** The list's order: `name` the default, `reset5h`, `reset7d` and `resetFable` a window's reset, `usage` the most
   *  used, `status`. */
  sort: (typeof ACCOUNT_SORT_KEYS)[number]['id'];
  /** `all` the default, else an account status. */
  status: 'all' | Account['status'];
}

export const ACCOUNT_SORT_KEYS = [
  NAME_KEY,
  { defaultDir: 'asc', id: 'reset5h', label: '5-hour reset' },
  { defaultDir: 'asc', id: 'reset7d', label: '7-day reset' },
  { defaultDir: 'asc', id: 'resetFable', label: '7-day Fable reset' },
  { defaultDir: 'desc', id: 'usage', label: 'Most used' },
  { defaultDir: 'asc', id: 'status', label: 'Status' },
] as const satisfies readonly SortKey[];

export const ACCOUNTS_CODEC = codecOf<AccountsAddress>({
  search: text('search'),
  ...sortSpecs(ACCOUNT_SORT_KEYS, 'name'),
  status: { default: 'all', kind: 'choice', name: 'status', values: ['all', ...ACCOUNT_STATUSES] },
});

const SESSION_GROUP_BYS = ['none', 'project', 'repository'] as const;
export type SessionGroupBy = (typeof SESSION_GROUP_BYS)[number];

export interface SessionsAddress extends SessionFilters, SortFields<(typeof SESSION_SORT_KEYS)[number]['id']> {
  /** How the rows are grouped: `none` the default, `project` or `repository`. */
  by: SessionGroupBy;
  /** `created` the default, `activity`, `cost` (the dollars of the session's requests) or `requests`. */
  sort: (typeof SESSION_SORT_KEYS)[number]['id'];
}

export const SESSION_SORT_KEYS = [
  { defaultDir: 'desc', id: 'created', label: 'Created' },
  { defaultDir: 'desc', id: 'activity', label: 'Last activity' },
  { defaultDir: 'desc', id: 'cost', label: 'Cost' },
  { defaultDir: 'desc', id: 'requests', label: 'Requests' },
] as const satisfies readonly SortKey[];

// The codec's literal keeps the bar's order: codecOf writes the address's keys in its specification's order.
export const SESSIONS_CODEC = codecOf<SessionsAddress>({
  search: text('search'),
  active: { default: '', kind: 'choice', name: 'active', values: ['', ...ACTIVE_WITHIN.map((a) => a.id)] },
  client: list('client'),
  companion: list('companion'),
  finished: { kind: 'flag', name: 'finished' },
  forked: { kind: 'flag', name: 'forked' },
  key: list('key'),
  pool: list('pool'),
  session: text('session'),
  slowed: { kind: 'flag', name: 'slowed' },
  account: list('account'),
  // A type the companion reports is free text and can hold a comma, which a comma-joined list would split.
  agentType: { kind: 'patterns', name: 'agent_type' },
  cache: list('cache'),
  effort: list('effort'),
  model: list('model'),
  released: { kind: 'flag', name: 'released' },
  status: list('status'),
  by: { default: 'none', kind: 'choice', name: 'by', values: SESSION_GROUP_BYS },
  ...sortSpecs(SESSION_SORT_KEYS, 'created'),
});
