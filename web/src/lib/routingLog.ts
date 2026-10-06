import { PIN_RELEASE_REASON_VALUES } from '@/api/decode';
import type { DecisionKind, DecisionSummary, FactEntity, FactUnit, PinReason, PinReleaseReason, RoutingEvent, RoutingLogRow } from '@/api/types';
import { moveLabel, own } from './analytics';
import { fmtCompact, fmtInt, fmtPct, fmtShortTime, fmtUsd, shortModel } from './format';
import { POLL_OUTCOME_LABEL } from './quota';

// The routing log's words. The routes answer structured values and no English, so a label change touches this module
// alone and never a route or a fixture.

export type FactLabel = '5h' | '7d' | 'Fable' | 'answer' | 'attempt' | 'capture' | 'cause' | 'client' | 'depth' | 'epoch' | 'field' | 'fields' | 'gap' | 'held' | 'idle' | 'in flight' | 'key' | 'lasted' | 'models' | 'next' | 'over' | 'pick' | 'pins' | 'prefix' | 'rank' | 'resets' | 'retry' | 'spent' | 'state' | 'to' | 'turn' | 'until' | 'was';

const LABELS = {
  answer: 'answer',
  attempt: 'attempt',
  capture: 'capture',
  cause: 'cause',
  client: 'client',
  depth: 'depth',
  epoch: 'epoch',
  field: 'field',
  fields: 'fields',
  five_hour: '5h',
  gap: 'gap',
  held: 'held',
  idle: 'idle',
  in_flight: 'in flight',
  key: 'key',
  lasted: 'lasted',
  models: 'models',
  next: 'next',
  over: 'over',
  pick: 'pick',
  pins: 'pins',
  prefix: 'prefix',
  rank: 'rank',
  resets: 'resets',
  retry: 'retry',
  seven_day: '7d',
  seven_day_fable: 'Fable',
  spent: 'spent',
  state: 'state',
  to: 'to',
  turn: 'turn',
  until: 'until',
  was: 'was',
} as const satisfies Record<string, FactLabel>;
type FactKey = keyof typeof LABELS;

/** The fact keys the router and the event rows carry, each to its label; a key without a label is a type error. */
export const FACT_LABEL: Record<string, FactLabel> = LABELS;

export interface FactPair {
  entity: FactEntity | null;
  label: FactLabel;
  value: string;
}

export interface Chip {
  entity?: FactEntity | null;
  label?: string;
  value: string;
}

/** Why the router released a pin without a move, in the order the selection found them. */
export const PIN_RELEASE_LABEL: Record<PinReleaseReason, string> = {
  attempted: 'already tried by this request',
  budget: 'account at its budget',
  client_error: 'client error',
  cooldown: 'account cooled',
  deleted: 'account deleted',
  disconnected: 'account disconnected',
  expired: 'pin idle past the affinity window',
  fast_credits: 'usage credits unavailable for fast mode',
  fast_rate_limit: 'fast lane rate limited',
  funding_required: 'funding required',
  overage: 'in overage',
  paused: 'account paused',
  reauth_required: 'needs re-auth',
  released: 'nothing else stood against the account',
  spent: 'window spent',
  subscription_ended: 'subscription ended',
  unpaired: 'account unpaired',
  unserved: 'model no longer served here',
};

/** A reason the console has no words for reads as the router sent it. */
export function releaseLabel(reason: string): string {
  const known = PIN_RELEASE_REASON_VALUES.find((v) => v === reason);
  return known === undefined ? reason : PIN_RELEASE_LABEL[known];
}

/** The Pin fold's row for a record that pins nothing: 'not looked up' (a refusal's) or 'pinning off'. */
export const PIN_REASON_LABEL: Record<PinReason, string> = {
  not_looked_up: 'not looked up',
  pinning_off: 'pinning off',
};

export const DECISION_KIND_LABEL: Record<DecisionKind, string> = {
  exhausted: 'exhausted',
  held: 'held',
  moved: 'moved',
  placed: 'placed',
  refused: 'refused',
  renewed: 'renewed',
};

export const DECISION_KIND_TONE: Record<DecisionKind, 'accent' | 'critical' | 'neutral' | 'warning'> = {
  exhausted: 'critical',
  held: 'warning',
  moved: 'accent',
  placed: 'accent',
  refused: 'critical',
  renewed: 'neutral',
};

/** Every kind of the ledger's events table, in the console's words. */
export const EVENT_KIND_LABEL: Record<string, string> = {
  account_state_changed: 'account state changed',
  budget_period_ended: 'budget period ended',
  budget_reached: 'budget reached',
  catalog_changed: 'catalog changed',
  cooldown_cleared: 'cooldown cleared',
  cooldown_set: 'cooldown set',
  fast_credits_changed: 'fast credits changed',
  ledger_gap: 'ledger gap',
  model_permitted: 'model permitted',
  model_rejected: 'model rejected',
  overage_ended: 'overage ended',
  overage_started: 'overage started',
  pin_created: 'pin created',
  pin_expired: 'pin expired',
  pin_migrated: 'pin moved',
  pin_released: 'pin released',
  poll_backoff_ended: 'poll backoff ended',
  poll_backoff_started: 'poll backoff started',
  reauth_required: 're-auth required',
  refresh_failed: 'refresh failed',
  refresh_succeeded: 'refresh succeeded',
  refresh_uncertain: 'refresh uncertain',
  registry_changed: 'registry changed',
  router_started: 'router started',
  shedding_round: 'shedding round',
  warm_prefix_expired: 'warm prefix expired',
  wind_down_started: 'wind-down started',
  window_reset: 'window reset',
  window_spent: 'window spent',
};

const EVENT_TONE: Record<string, 'critical' | 'serious' | 'warning'> = {
  account_state_changed: 'serious',
  budget_reached: 'serious',
  cooldown_set: 'warning',
  ledger_gap: 'serious',
  model_rejected: 'warning',
  reauth_required: 'critical',
  window_spent: 'warning',
};

/** An event reads neutral unless it reports trouble. */
export function eventTone(kind: string): 'critical' | 'neutral' | 'serious' | 'warning' {
  return own(EVENT_TONE, kind) ? EVENT_TONE[kind] : 'neutral';
}

/** The record's walk steps in the console's words, keyed by the step the engine names. */
export const STEP_NAME: Record<string, string> = {
  admission: 'Admission',
  bound: 'Bound',
  cap_filter: 'Cap filter',
  cool: 'Cool reading',
  eligible: 'Eligible',
  exhausted: 'Exhausted',
  hold: 'Hold',
  'move:drain': 'Drain',
  'move:overage': 'Overage',
  'move:return_to_plan': 'Return to plan',
  'move:shed': 'Shed',
  'move:spent': 'Spent',
  partitions: 'Partitions',
  pick: 'Pick',
  pin: 'Pin',
  probe: 'Probe',
  renew: 'Renew',
  'rung:content': 'Rung: content',
  'rung:static': 'Rung: static',
  'rung:with_main': 'Rung: with main',
  selection: 'Rest of the walk',
};

const ACTED = new Set(['bound', 'chosen', 'fired', 'match', 'placed', 'preferred', 'renewed']);
const BLOCKED = new Set(['answered', 'blocked', 'dropped', 'exhausted', 'fell_through', 'held', 'refused', 'released']);

/** The accent tone for a step that acted, the warning tone for one that blocked, neutral for the rest. */
export function verdictTone(verdict: string): 'accent' | 'neutral' | 'warning' {
  return ACTED.has(verdict) ? 'accent' : BLOCKED.has(verdict) ? 'warning' : 'neutral';
}

const words = (name: string) => name.replace(/_/g, ' ');
const wordOf = (map: Record<string, string>, name: string) => (own(map, name) ? map[name] : words(name));

const HOUR = 3_600_000;
const MINUTE = 60_000;

/** A span the way the Fact cell reads one: `2.0 s`, `2 min 41 s`, `1 h 2 min`, `1 d 4 h`. */
function span(ms: number): string {
  const abs = Math.abs(ms);
  if (abs < 1_000) return `${Math.round(abs)} ms`;
  if (abs < MINUTE) return `${(abs / 1_000).toFixed(abs >= 10_000 ? 0 : 1)} s`;
  const pair = (major: number, majorUnit: string, minor: number, minorUnit: string) => (minor > 0 ? `${major} ${majorUnit} ${minor} ${minorUnit}` : `${major} ${majorUnit}`);
  if (abs < HOUR) return pair(Math.floor(abs / MINUTE), 'min', Math.floor((abs % MINUTE) / 1_000), 's');
  if (abs < 24 * HOUR) return pair(Math.floor(abs / HOUR), 'h', Math.floor((abs % HOUR) / MINUTE), 'min');
  return pair(Math.floor(abs / (24 * HOUR)), 'd', Math.floor((abs % (24 * HOUR)) / HOUR), 'h');
}

/**
 * An instant within a day of `now` reads as its clock time alone (`15:42`, a retry at the next midnight `00:00`); one
 * further off carries its date (`Sep 18 08:42`).
 */
function instant(ms: number, now: number): string {
  const at = new Date(ms).toISOString();
  if (Math.abs(ms - now) < 24 * HOUR) return fmtShortTime(at);
  return `${new Date(ms).toLocaleDateString('en-US', { day: 'numeric', month: 'short' })} ${fmtShortTime(at)}`;
}

const msOf = (value: number | string) => (typeof value === 'number' ? value : Date.parse(value));

/** A fact's value drawn by its unit: 1.34 %/h, 15:42, 2 min 41 s, 31.2K, $20.40; a value of no unit reads as sent. */
export function formatFact(value: number | string, unit: FactUnit | null, now: number): string {
  if (typeof value === 'string' && unit !== 'time') return value;
  switch (unit) {
    case 'count':
      return fmtInt(value as number);
    case 'ms':
      return span(value as number);
    case 'percent':
      return fmtPct(value as number);
    case 'percent_per_hour':
      return `${((value as number) * 100).toFixed(2)} %/h`;
    case 'time': {
      const ms = msOf(value);
      return Number.isNaN(ms) ? String(value) : instant(ms, now);
    }
    case 'tokens':
      return fmtCompact(value as number);
    case 'usd':
      return fmtUsd(value as number);
    default:
      return String(value);
  }
}

const atOf = (row: RoutingLogRow) => Date.parse(row.at);
/** A time inside the row's own minute is the one the Time column already gives, so the cell leaves it out. */
const repeatsAt = (row: RoutingLogRow, unit: FactUnit | null, value: number | string) => unit === 'time' && Math.floor(msOf(value) / MINUTE) === Math.floor(atOf(row) / MINUTE);

const pair = (key: FactKey, value: number | string, unit: FactUnit | null, now: number): FactPair => ({ entity: null, label: LABELS[key], value: formatFact(value, unit, now) });

const STRATEGY_WORD: Record<string, string> = { pace: 'Pace', priority: 'priority', round_robin: 'round robin' };
const RUNG_WORD: Record<string, string> = { content: 'content rung', static: 'static rung', with_main: 'with main rung' };
/** What a probe's pinned member could not get past; any other name reads as sent. */
const OBSTACLE_WORD: Record<string, string> = { attempted: 'already tried', budget_unknown: 'budget unknown', operator: 'by hand', subscription_ended: 'subscription ended' };
/**
 * A key refusal's summary names its kind alone and its reason in the router's words, so the chip reads the reason; one
 * the console has no words for reads as a key refusal.
 */
const KEY_REFUSAL_WORD: Record<string, string> = {
  'the key has expired': 'key expired',
  'the key is disabled': 'key disabled',
  'the key is not in the registry': 'key unknown',
  "the key's pool does not exist": 'pool missing',
  "the key's pool is disabled": 'pool disabled',
  'the key was replaced under its id': 'key replaced',
  'the key was revoked': 'key revoked',
};

/** The strategy's word and the tier's priority: `Pace P0`. */
const strategyChip = (strategy: string, tier: DecisionSummary['tier']) => (tier === null ? wordOf(STRATEGY_WORD, strategy) : `${wordOf(STRATEGY_WORD, strategy)} P${tier[1]}`);

function exhaustionWord(row: DecisionSummary): string {
  const x = row.exhaustion ?? { kind: row.by.value, obstacle: null, window: null };
  switch (x.kind) {
    case 'fast_credits':
      return 'credits';
    case 'fast_lane':
      return 'fast lane';
    case 'model_rejected':
      return 'model rejected';
    case 'pinned':
      return x.obstacle === null ? 'pinned' : wordOf(OBSTACLE_WORD, x.obstacle);
    // A budget's deadline travels under the overage window, the claim the relay renders on the wire.
    case 'timed':
      return x.window === 'budget' || x.window === 'overage' ? 'budget' : 'cooldown';
    case 'untimed':
      return 'no deadline';
    default:
      return words(x.kind);
  }
}

function decisionBy(row: DecisionSummary): Chip {
  const { kind, value } = row.by;
  if (row.kind === 'renewed') return { value: row.probe ? 'keepalive' : 'no rule fired' };
  switch (kind) {
    case 'exhaustion':
      return { value: exhaustionWord(row) };
    case 'hold':
      return { value: value === 'strategy_pick' ? strategyChip(row.strategy, row.tier) : words(value) };
    case 'none':
      return { value: 'no rule fired' };
    case 'refusal':
      if (value === 'client') return { value: 'client drift' };
      return { value: row.refusal && own(KEY_REFUSAL_WORD, row.refusal.reason) ? KEY_REFUSAL_WORD[row.refusal.reason] : `${words(value)} refused` };
    case 'rule':
      return { value: `${words(value)} rule` };
    case 'rung':
      return { value: wordOf(RUNG_WORD, value) };
    case 'strategy':
      return { value: strategyChip(value, row.by.tier ?? row.tier) };
  }
}

const str = (v: unknown): string | null => (typeof v === 'string' ? v : null);
const num = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) ? v : null);
const list = (v: unknown): unknown[] => (Array.isArray(v) ? v : []);
const rec = (v: unknown): Record<string, unknown> => (v !== null && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : {});

/** A window's name as the windows read elsewhere in the console. */
const WINDOW_WORD: Record<string, string> = { five_hour: '5h', seven_day: '7d', seven_day_fable: 'Fable' };
const STATE_WORD: Record<string, string> = { funding_required: 'funding required', reauth_required: 're-auth required' };
/** The registry's word for who moved an account's state. */
const CAUSE_WORD: Record<string, string> = { auth_failed: 'credential refused', funding_refused: 'credit balance', operator: 'by hand', refresh_failed: 'refresh failed' };
// `deadline` is the mock's word for a cooldown that ran out, which the router writes as `expired`.
const CLEARED_WORD: Record<string, string> = { admin: 'by hand', deadline: 'deadline passed', expired: 'deadline passed', operator: 'by hand' };
const CREDITS_WORD: Record<string, string> = { org_spend_cap_reached: 'spend cap reached' };
const PERIOD_WORD: Record<string, string> = { day: 'daily budget', month: 'monthly budget' };
const REGISTRY_VERB: Record<string, string> = { updated: 'edited' };
/** The pool fields a registry edit reads as a share rather than a bare number. */
const SHARE_FIELDS = new Set(['pace_floor', 'select_below_utilization', 'shed_ahead_of_pace', 'shed_share']);

const registryBy = (kind: string) => {
  const at = kind.indexOf('_');
  return at < 0 ? words(kind) : `${kind.slice(0, at)} ${wordOf(REGISTRY_VERB, kind.slice(at + 1))}`;
};

const fieldValue = (field: string, value: unknown) => {
  if (value === null || value === undefined) return 'none';
  if (typeof value === 'boolean') return value ? 'on' : 'off';
  if (typeof value === 'number' && SHARE_FIELDS.has(field)) return fmtPct(value);
  return typeof value === 'object' ? JSON.stringify(value) : String(value);
};

function cooldownBy(e: RoutingEvent): string {
  const d = e.detail;
  const scope = str(d.scope) ?? '';
  if (scope === 'fast') return 'fast lane 429';
  if (scope.startsWith('model:')) return `404 on ${shortModel(scope.slice('model:'.length))}`;
  const reason = str(d.reason) ?? '';
  const window = str(d.window) ?? '';
  if (reason !== 'rate_limit' && reason !== 'rate_limited') return words(reason);
  if (own(WINDOW_WORD, window)) return `429 on ${WINDOW_WORD[window]}`;
  return window === 'retry_after' ? '429 retry-after' : '429';
}

function eventBy(e: RoutingEvent, page: readonly RoutingLogRow[]): Chip {
  const d = e.detail;
  switch (e.kind) {
    case 'account_state_changed':
      return { value: wordOf(CAUSE_WORD, str(d.cause) ?? 'unrecorded') };
    case 'budget_period_ended':
    case 'budget_reached':
      return { value: wordOf(PERIOD_WORD, str(d.period) ?? '') };
    case 'catalog_changed': {
      const added = list(d.added).length;
      const removed = list(d.removed).length;
      return { value: added > 0 && removed > 0 ? 'models changed' : removed > 0 ? `model${removed > 1 ? 's' : ''} removed` : `model${added > 1 ? 's' : ''} added` };
    }
    case 'cooldown_cleared':
      return { value: wordOf(CLEARED_WORD, str(d.cause) ?? 'cleared') };
    case 'cooldown_set':
      return { value: cooldownBy(e) };
    case 'fast_credits_changed': {
      const blocked = str(d.blocked_after);
      return { value: blocked === null ? 'room' : wordOf(CREDITS_WORD, blocked) };
    }
    case 'ledger_gap':
      return { value: words(str(d.reason) ?? 'gap') };
    case 'model_permitted':
      return { value: list(d.rejected_by).length > 0 ? 'after 404' : 'permitted' };
    case 'model_rejected':
      return { value: '404' };
    case 'overage_ended':
      return { value: '7d reset' };
    case 'overage_started':
      return { value: 'in overage' };
    case 'pin_created': {
      // The pin a placement created lands in the same second as the decision that made it, under the same session.
      const placement = page.find((r): r is DecisionSummary => r.type === 'decision' && r.kind === 'placed' && r.sessionId === e.sessionId && r.accountId === e.accountId && Math.floor(atOf(r) / 1_000) === Math.floor(atOf(e) / 1_000));
      return placement ? decisionBy(placement) : { value: 'placement' };
    }
    case 'pin_expired': {
      const first = str(list(d.reasons)[0]);
      return { value: first === null ? 'expired' : first === 'expired' ? 'idle' : releaseLabel(first) };
    }
    case 'pin_migrated':
      return { value: moveLabel(str(d.reason) ?? '') };
    case 'pin_released':
      return { value: 'by hand' };
    case 'poll_backoff_ended':
      return { value: 'clean poll' };
    // The usage service opens a backoff on a rate limit alone, and its `reason` is the hold's English sentence, so the
    // kind decides the word.
    case 'poll_backoff_started':
      return { value: POLL_OUTCOME_LABEL.rate_limited };
    case 'reauth_required':
      return { value: 'credential refused' };
    case 'refresh_failed':
      return { value: 'refresh failed' };
    case 'refresh_succeeded':
      return { value: 'refreshed' };
    case 'refresh_uncertain':
      return { value: 'refresh uncertain' };
    case 'registry_changed':
      return { value: registryBy(str(d.kind) ?? 'registry_updated') };
    case 'router_started':
      return { value: 'restart' };
    case 'shedding_round': {
      const window = str(d.window) ?? '';
      const reading = num(d.utilization);
      return { value: reading === null ? `on ${wordOf(WINDOW_WORD, window)}` : `${fmtPct(reading)} on ${wordOf(WINDOW_WORD, window)}` };
    }
    case 'warm_prefix_expired': {
      const ttl = num(rec(d.deepest).ttl_ms);
      return { value: ttl === null ? 'expired' : `${span(ttl)} tier` };
    }
    case 'wind_down_started':
      return { value: 'subscription ends' };
    case 'window_reset':
      return { value: `${wordOf(WINDOW_WORD, str(d.window) ?? '')} reset` };
    case 'window_spent':
      return { value: `${str(d.source) === 'poll' ? 'poll' : 'head'} on ${wordOf(WINDOW_WORD, str(d.window) ?? '')}` };
    default:
      return { value: words(e.kind) };
  }
}

/**
 * The By chip of a row, from a summary's `by` or an event's `kind` and `detail`. A pin created by a placement takes
 * the placement's By, read off the decision of the same second among `page`, the rows drawn beside it.
 */
export function byOf(row: RoutingLogRow, page: readonly RoutingLogRow[] = []): Chip {
  return row.type === 'decision' ? decisionBy(row) : eventBy(row, page);
}

/** The moves a failed attempt forced, whose Fact is the answer that attempt got. */
const FAILOVER_MOVES = new Set(['cooldown', 'fast_lane']);

/**
 * The status a failover's failed attempt answered. The move lands in the second of the decision that placed the next
 * attempt, under the same session and account. The route joins each decision to the attempt it dispatched, so the
 * request's decision before that one carries the failed attempt's status.
 */
function failoverAnswer(e: RoutingEvent, page: readonly RoutingLogRow[]): number | null {
  if (e.sessionId === null) return null;
  const second = Math.floor(atOf(e) / 1_000);
  const decisions = page.filter((r): r is DecisionSummary => r.type === 'decision');
  for (const next of decisions) {
    if (next.sessionId !== e.sessionId || next.accountId !== e.accountId || Math.floor(atOf(next) / 1_000) !== second) continue;
    const failed = decisions.filter((r) => r.requestId === next.requestId && r.seq < next.seq && r.status !== null).sort((a, b) => b.seq - a.seq)[0];
    if (failed) return failed.status;
  }
  return null;
}

/** A diff entry the console can read as one field's change: its old value and its new one, nothing else. */
const isChange = (v: unknown): boolean => {
  const keys = Object.keys(rec(v));
  return keys.length === 2 && keys.includes('from') && keys.includes('to');
};

function eventFacts(e: RoutingEvent, now: number, page: readonly RoutingLogRow[]): FactPair[] {
  const d = e.detail;
  const at = atOf(e);
  const time = (key: FactKey, v: unknown) => {
    const ms = num(v);
    return ms === null ? [] : [pair(key, ms, 'time', now)];
  };
  const lasted = (ms: number | null) => (ms === null ? [] : [pair('lasted', ms, 'ms', now)]);
  const count = (key: FactKey, v: unknown) => {
    const n = num(v);
    return n === null ? [] : [pair(key, n, 'count', now)];
  };
  switch (e.kind) {
    case 'account_state_changed':
      return [pair('state', wordOf(STATE_WORD, str(d.to) ?? 'unknown'), 'text', now)];
    case 'budget_period_ended': {
      const spent = num(d.previous_spent_usd);
      return spent === null ? [] : [pair('spent', spent, 'usd', now)];
    }
    case 'budget_reached': {
      const spent = num(d.spent_usd);
      const budget = num(d.budget_usd);
      return spent === null || budget === null ? [] : [pair('spent', `${fmtUsd(spent)} of ${fmtUsd(budget)}`, 'text', now)];
    }
    case 'catalog_changed':
      return count('models', d.count);
    case 'cooldown_set':
      return time('until', d.until);
    case 'fast_credits_changed':
      return [pair('state', str(d.blocked_after) === null ? 'available' : 'unavailable', 'text', now)];
    case 'ledger_gap':
      return str(d.reason)?.startsWith('chunk') ? [pair('capture', 'truncated', 'text', now)] : [];
    case 'model_permitted': {
      // A permit is the attempt that answered, so it is the request's last.
      const n = num(d.attempt);
      return n === null ? [] : [pair('attempt', `${n} of ${n}`, 'text', now)];
    }
    case 'overage_ended': {
      const since = num(d.since);
      return lasted(since === null ? null : at - since);
    }
    case 'pin_created':
      return count('epoch', d.epoch);
    case 'pin_migrated': {
      if (!FAILOVER_MOVES.has(str(d.reason) ?? '')) return count('epoch', d.epoch);
      // The epoch would stand where the answer belongs, so a failover whose failed attempt is not drawn reads nothing.
      const status = failoverAnswer(e, page);
      return status === null ? [] : [pair('answer', status, 'count', now)];
    }
    case 'poll_backoff_ended':
      return lasted(num(d.duration_ms));
    case 'poll_backoff_started':
      return time('next', d.next_attempt_at);
    case 'reauth_required':
      return [pair('state', 'routing off', 'text', now)];
    case 'registry_changed': {
      // Only a diff whose every entry is a change names its fields, and the whole row the store writes on an edit names
      // none.
      const diff = Object.entries(rec(d.diff));
      if (diff.length === 0 || !diff.every(([, change]) => isChange(change))) return [];
      if (diff.length > 1) return [pair('fields', diff.length, 'count', now)];
      const [field, change] = diff[0];
      return [pair('field', words(field), 'text', now), pair('to', fieldValue(field, rec(change).to), 'text', now)];
    }
    case 'router_started':
      return count('pins', d.pins_loaded);
    case 'shedding_round':
      return [pair('pins', `${list(d.marks).length} of ${list(d.pins).length}`, 'text', now)];
    case 'warm_prefix_expired': {
      const deepest = rec(d.deepest);
      const tokens = num(deepest.prefix_tokens);
      const depth = num(deepest.depth);
      return [...(tokens === null ? [] : [pair('prefix', tokens, 'tokens', now)]), ...(depth === null ? [] : [pair('depth', depth, 'count', now)])];
    }
    case 'wind_down_started':
      return time('until', d.ends_at);
    case 'window_reset': {
      const before = num(d.utilization_before);
      return before === null ? [] : [pair('was', before, 'percent', now)];
    }
    case 'window_spent':
      return time('resets', d.reset);
    default:
      return [];
  }
}

function decisionFacts(row: DecisionSummary, now: number): FactPair[] {
  // A key the console has no label for is dropped rather than drawn under a guessed one.
  return row.fact.flatMap((f) => (own(LABELS, f.key) && !repeatsAt(row, f.unit, f.value) ? [{ entity: f.entity, label: LABELS[f.key as FactKey], value: formatFact(f.value, f.unit, now) }] : []));
}

/**
 * One or two pairs; never a time the row's `at` already gives. A failover move reads its answer off the decisions
 * among `page`, the rows drawn beside it.
 */
export function factsOf(row: RoutingLogRow, now: number, page: readonly RoutingLogRow[] = []): FactPair[] {
  return (row.type === 'decision' ? decisionFacts(row, now) : eventFacts(row, now, page)).slice(0, 2);
}

export function entityHref(kind: FactEntity['kind'] | 'request' | 'session', id: string): string {
  switch (kind) {
    case 'account':
      return `/accounts/${encodeURIComponent(id)}`;
    case 'request':
      return `/requests/${encodeURIComponent(id)}`;
    default:
      return `/${kind === 'session' ? 'sessions' : `${kind}s`}?${new URLSearchParams({ [kind]: id })}`;
  }
}

export interface LogLink {
  accounts?: string[];
  from: number;
  kinds?: string[];
  models?: string[];
  pools?: string[];
  request?: string;
  session?: string;
  to: number;
}

/** The log's address for `link`: the plural keys comma-joined, the range as two ISO instants after the filters. */
export function logHref(link: LogLink): string {
  const params = new URLSearchParams();
  const put = (name: string, value: string | undefined) => {
    if (value) params.set(name, value);
  };
  put('accounts', link.accounts?.join(','));
  put('kinds', link.kinds?.join(','));
  put('models', link.models?.join(','));
  put('pools', link.pools?.join(','));
  put('request', link.request);
  put('session', link.session);
  put('from', new Date(link.from).toISOString());
  put('to', new Date(link.to).toISOString());
  return `/routing-log?${params}`;
}

const QUARTER_HOUR = 15 * MINUTE;

/** A moment: a quarter hour either side of it, `to` at most now. */
export const aroundRequest = (at: number, finishedAt: number | null, now: number): LogLink => ({ from: at - QUARTER_HOUR, to: Math.min((finishedAt ?? at) + QUARTER_HOUR, now) });

/**
 * The one builder for a link that opens the log on a request, `{ request: id, ...aroundRequest(at, finishedAt, now) }`,
 * so no caller can set the range and drop the id.
 */
export const requestLink = (id: string, at: number, finishedAt: number | null, now: number): LogLink => ({ request: id, ...aroundRequest(at, finishedAt, now) });

/** A subject that lives on: the last hour, or the hour either side of its last activity, `to` at most now. */
export const aroundSubject = (lastActivity: number | null, now: number): LogLink => (lastActivity === null ? { from: now - HOUR, to: now } : { from: lastActivity - HOUR, to: Math.min(lastActivity + HOUR, now) });

/** A condition: from its start to now. */
export const sinceCondition = (since: number, now: number): LogLink => ({ from: since, to: now });

/** The clock minute that holds `at`, its last millisecond the end, `to` at most now. */
export const clockMinute = (at: number, now: number): LogLink => {
  const from = Math.floor(at / MINUTE) * MINUTE;
  return { from, to: Math.min(from + MINUTE - 1, now) };
};

const FAMILIES = ['fable', 'opus', 'sonnet', 'haiku'];

/** Model ids in the order fable, opus, sonnet, haiku, newest version first; an id of no known family sorts last. */
export function compareModels(a: string, b: string): number {
  const read = (id: string) => {
    const family = FAMILIES.findIndex((f) => id.includes(`-${f}-`) || id.endsWith(`-${f}`));
    const version = (id.replace(/-\d{8}$/, '').match(/(\d+(?:-\d+)*)$/)?.[1] ?? '').split('-').filter(Boolean).map(Number);
    return { family: family < 0 ? FAMILIES.length : family, version };
  };
  const x = read(a);
  const y = read(b);
  if (x.family !== y.family) return x.family - y.family;
  for (let i = 0; i < Math.max(x.version.length, y.version.length); i += 1) {
    const d = (y.version[i] ?? 0) - (x.version[i] ?? 0);
    if (d !== 0) return d;
  }
  return a.localeCompare(b);
}
