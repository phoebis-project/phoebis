import { REQUEST_KIND_VALUES } from '@/api/decode';
import type { BurnGroup, KeyStatus, RequestKind } from '@/api/types';
import { clock } from './clock';
import { ACCOUNT_STATUSES } from './quota';

const HOUR = 3_600_000;
const DAY = 24 * HOUR;

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

/** A view's window as the address carries it: a preset by name with the instants empty, or `custom` with both instants. */
export interface RangeValue {
  from: string;
  preset: string;
  to: string;
}

export type KeySpec =
  /** A value outside `values` reads as the default. */
  | { kind: 'choice'; default: string; name: string; values: readonly string[] }
  /** Written as `1`, read as true on `1`. */
  | { kind: 'flag'; name: string }
  /** Comma-joined ids. */
  | { kind: 'list'; name: string }
  /** The keys `range`, `from` and `to`. */
  | { kind: 'range'; default: string; presets: readonly RangePreset[] }
  | { kind: 'text'; name: string };

export type Codec<F> = { fromParams(params: URLSearchParams, now?: number): F; toParams(filters: F): URLSearchParams };

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
    case 'flag':
      return params.get(key.name) === '1';
    case 'list':
      return (params.get(key.name) ?? '').split(',').filter(Boolean);
    case 'range': {
      const from = instantOf(params.get('from'));
      const asked = instantOf(params.get('to'));
      // The end is capped before the pair is judged, as the range control caps it, so a pair wholly after now reads
      // as one that ends before it starts.
      const to = asked !== '' && Date.parse(asked) > now ? new Date(now).toISOString() : asked;
      // A pair is a span only while `from` precedes `to`, so a half, backward or unreadable pair reads as the preset.
      if (from !== '' && to !== '' && Date.parse(from) < Date.parse(to)) return { from, preset: 'custom', to } satisfies RangeValue;
      const name = params.get('range');
      return { from: '', preset: key.presets.some((p) => p.name === name) && name !== null ? name : key.default, to: '' } satisfies RangeValue;
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
    case 'flag':
      if (value === true) params.set(key.name, '1');
      return;
    case 'list':
      if ((value as string[]).length > 0) params.set(key.name, (value as string[]).join(','));
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
  return {
    fromParams(params, now = clock.now()) {
      return Object.fromEntries(keys.map(([field, key]) => [field, readKey(key, params, now)])) as F;
    },
    toParams(filters) {
      const params = new URLSearchParams();
      for (const [field, key] of keys) writeKey(key, (filters as Record<string, unknown>)[field], params);
      return params;
    },
  };
}

/** A preset's window from `now`, or a custom span's own instants. Named apart from quota.ts's windowOf, which the test entry also star-exports. */
export function addressWindow(range: RangeValue, presets: readonly RangePreset[], fallback: string, now: number): { from: number; to: number } {
  if (range.preset === 'custom') {
    const from = Date.parse(range.from);
    const to = Date.parse(range.to);
    if (from < to) return { from, to };
  }
  const preset = presets.find((p) => p.name === range.preset) ?? presets.find((p) => p.name === fallback);
  return { from: now - (preset?.ms ?? 0), to: now };
}

const list = (name: string): KeySpec => ({ kind: 'list', name });
const text = (name: string): KeySpec => ({ kind: 'text', name });

export interface RequestsAddress {
  accounts: string[];
  /** The user agent pattern. */
  agent: string;
  agentTypes: string[];
  cacheMissReasons: string[];
  client: string;
  efforts: string[];
  /** Guardrail ids, `-` for a request no guardrail matched. */
  guardrails: string[];
  held: boolean;
  keys: string[];
  kind: '' | RequestKind;
  models: string[];
  outcomes: string[];
  pools: string[];
  range: RangeValue;
  request: string;
  requestClasses: string[];
  /** The page size. */
  rows: string;
  search: string;
  session: string;
  speeds: string[];
  statuses: string[];
  turnOrigins: string[];
}

const REQUEST_KINDS: readonly ('' | RequestKind)[] = ['', ...REQUEST_KIND_VALUES];

export const REQUESTS_CODEC = codecOf<RequestsAddress>({
  search: text('search'),
  request: text('request'),
  session: text('session'),
  client: text('client'),
  agent: text('agent'),
  agentTypes: list('agent_types'),
  turnOrigins: list('turn_origins'),
  efforts: list('efforts'),
  requestClasses: list('request_classes'),
  speeds: list('speeds'),
  held: { kind: 'flag', name: 'held' },
  keys: list('keys'),
  pools: list('pools'),
  accounts: list('accounts'),
  models: list('models'),
  statuses: list('statuses'),
  outcomes: list('outcomes'),
  cacheMissReasons: list('cache_miss_reasons'),
  guardrails: list('guardrails'),
  kind: { default: '', kind: 'choice', name: 'kind', values: REQUEST_KINDS },
  rows: { default: '25', kind: 'choice', name: 'rows', values: ['10', '25', '50', '100'] },
  range: { default: '24h', kind: 'range', presets: RANGE_PRESETS },
});

export interface AnalyticsAddress {
  accounts: string[];
  /** The user agent pattern. */
  agent: string;
  /** The group-by, the Burn section's own `session` among its values. */
  by: string;
  /** The client pattern. */
  class: string;
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
  /** A shared link's unit position, which the page reads once into the console-wide setting and drops. */
  unit: string;
}

// A key table rather than a list, so a grouping the burn route gains fails to compile until it is listed here. The
// burn route's groupings are the breakdown's and its own `session`.
const GROUP_BY_KEYS = { account: true, client: true, effort: true, key: true, kind: true, model: true, pool: true, request_class: true, session: true, speed: true, user_agent: true } satisfies Record<BurnGroup, true>;
const GROUP_BYS = Object.keys(GROUP_BY_KEYS) as BurnGroup[];

export const ANALYTICS_CODEC = codecOf<AnalyticsAddress>({
  session: text('session'),
  key: text('key'),
  accounts: list('accounts'),
  agent: text('agent'),
  by: { default: 'key', kind: 'choice', name: 'by', values: GROUP_BYS },
  class: text('class'),
  groups: text('groups'),
  keys: list('keys'),
  models: list('models'),
  pools: list('pools'),
  probes: { kind: 'flag', name: 'probes' },
  unit: text('unit'),
  range: { default: '24h', kind: 'range', presets: RANGE_PRESETS },
});

/** The routing log's filters as the address holds them. */
export interface LogFilters {
  accounts: string[];
  keys: string[];
  kinds: string[];
  models: string[];
  pools: string[];
  range: RangeValue;
  request: string;
  session: string;
}

/**
 * The routing log's filters to and from its address. A kind this build does not know stays, so the route's refusal
 * reaches the page where a link from a newer router would otherwise read as every kind.
 */
export const ROUTING_LOG_CODEC = codecOf<LogFilters>({
  accounts: list('accounts'),
  keys: list('keys'),
  kinds: list('kinds'),
  models: list('models'),
  pools: list('pools'),
  request: text('request'),
  session: text('session'),
  range: { default: '1h', kind: 'range', presets: RANGE_PRESETS },
});

export interface KeysAddress {
  pools: string[];
  search: string;
  status: string;
}

const KEY_STATUSES: readonly ('all' | KeyStatus)[] = ['all', 'active', 'disabled', 'expired', 'revoked'];

export const KEYS_CODEC = codecOf<KeysAddress>({
  pools: list('pools'),
  search: text('search'),
  status: { default: 'all', kind: 'choice', name: 'status', values: KEY_STATUSES },
});

export interface AccountsAddress {
  search: string;
  /** The list's order: `name` the default, `reset` the soonest reset, `usage` the most used, `status`. */
  sort: string;
  /** `all` the default, else an account status. */
  status: string;
}

const ACCOUNT_SORTS = ['name', 'reset', 'usage', 'status'] as const;

export const ACCOUNTS_CODEC = codecOf<AccountsAddress>({
  search: text('search'),
  sort: { default: 'name', kind: 'choice', name: 'sort', values: ACCOUNT_SORTS },
  status: { default: 'all', kind: 'choice', name: 'status', values: ['all', ...ACCOUNT_STATUSES] },
});

export interface ModelsAddress {
  /** The chosen key, empty for the first listed key. */
  key: string;
}

export const MODELS_CODEC = codecOf<ModelsAddress>({ key: text('key') });

// These views have no filter, so their codecs read and write nothing: the Pools page keeps its hash and its one-shot
// `edit` and `group`, the Quota page's hash is its section, and a filter either view gains later lands here.
export const POOLS_CODEC = codecOf<Record<string, never>>({});
export const QUOTA_CODEC = codecOf<Record<string, never>>({});
export const SESSIONS_CODEC = codecOf<Record<string, never>>({});
