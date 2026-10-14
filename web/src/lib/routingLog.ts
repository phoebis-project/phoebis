import { iso, PIN_RELEASE_REASON_VALUES } from '@/api/decode';
import type { DecisionKind, DecisionRecord, DecisionSummary, FactEntity, FactUnit, ModelTiers, PinReason, PinReleaseReason, Placement, PrefixEntry, RecordCandidate, RecordOutcome, RecordSettings, RoutingEvent, RoutingLogRow, WalkFact, WalkStep } from '@/api/types';
import { ROUTING_LOG_CODEC, type RangeValue } from './address';
import { moveLabel, own } from './analytics';
import { compareByModelTier } from './costs';
import type { ChipText } from './filterBar';
import { fmtCompact, fmtInt, fmtPct, fmtShare, fmtTime, fmtUsd, instant, shortModel } from './format';
import { planLabel, POLL_OUTCOME_LABEL } from './quota';

// The routing log's words. The routes answer structured values and no English, so a label change touches this module
// alone and never a route or a fixture.

export type FactLabel = '5h' | '7d' | 'Fable' | 'answer' | 'attempt' | 'capture' | 'cause' | 'client' | 'depth' | 'epoch' | 'field' | 'fields' | 'fit' | 'gap' | 'held' | 'idle' | 'in flight' | 'key' | 'lasted' | 'models' | 'next' | 'over' | 'pick' | 'pins' | 'prefix' | 'rank' | 'resets' | 'retry' | 'runway' | 'spent' | 'state' | 'to' | 'turn' | 'until' | 'was';

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
  fit: 'fit',
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
  runway: 'runway',
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

/** The fact keys the router and the event rows carry, each to its label. A key without a label is a type error. */
export const FACT_LABEL: Record<string, FactLabel> = LABELS;

export interface FactPair {
  entity: FactEntity | null;
  label: FactLabel;
  value: string;
}

export interface Chip {
  entity?: FactEntity | null;
  /** Where the value links when it names a session or a request, which no fact entity covers. */
  href?: string;
  label?: string;
  /** The label names this entity and links it: a fact about one member takes the member as its key. */
  labelEntity?: FactEntity | null;
  mono?: boolean;
  title?: string;
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

export type VerdictTone = 'critical' | 'good' | 'neutral' | 'warning';

export const DECISION_KIND_TONE: Record<DecisionKind, VerdictTone> = {
  exhausted: 'critical',
  held: 'warning',
  moved: 'warning',
  placed: 'good',
  refused: 'critical',
  renewed: 'good',
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
  mark_cleared: 'mark cleared',
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
  refresh_failed: 'refresh failed',
  refresh_succeeded: 'refresh succeeded',
  refresh_uncertain: 'refresh uncertain',
  registry_changed: 'registry changed',
  reservation_expired: 'reservation expired',
  router_started: 'router started',
  shedding_round: 'shedding round',
  warm_prefix_expired: 'warm prefix expired',
  wind_down_started: 'wind-down started',
  window_reset: 'window reset',
  window_spent: 'window spent',
};

/** A named set of kinds, which the Kinds list checks or unchecks in one click. */
export interface KindPreset {
  label: string;
  members: readonly string[];
}

/** The eight groups of related kinds, in the order a reader follows a request. Every kind belongs to exactly one, and a preset is exactly its members. */
export const KIND_PRESETS: readonly KindPreset[] = [
  { label: 'placements and pins', members: ['placed', 'renewed', 'pin_created', 'pin_migrated', 'pin_released', 'pin_expired', 'reservation_expired'] },
  { label: 'moves and holds', members: ['moved', 'held', 'shedding_round', 'mark_cleared', 'wind_down_started'] },
  { label: 'exhaustion and refusals', members: ['exhausted', 'refused', 'window_spent', 'budget_reached'] },
  { label: 'account state and windows', members: ['account_state_changed', 'window_reset', 'overage_started', 'overage_ended', 'budget_period_ended', 'fast_credits_changed', 'refresh_failed', 'refresh_succeeded', 'refresh_uncertain'] },
  { label: 'cooldowns and backoff', members: ['cooldown_set', 'cooldown_cleared', 'poll_backoff_started', 'poll_backoff_ended'] },
  { label: 'keepalive', members: ['probe', 'warm_prefix_expired'] },
  { label: 'registry changes', members: ['registry_changed', 'catalog_changed', 'model_permitted', 'model_rejected'] },
  { label: 'router life cycle', members: ['router_started', 'ledger_gap'] },
];

/** Every kind the Kinds list offers: the six decision kinds, `probe`, and the 30 event kinds. */
export const ALL_KINDS: readonly string[] = [...Object.keys(DECISION_KIND_LABEL), 'probe', ...Object.keys(EVENT_KIND_LABEL)];

/** A kind's word: "probe decisions" for the pseudo-kind, else its label. */
export const kindWord = (kind: string): string => (kind === 'probe' ? 'probe decisions' : (DECISION_KIND_LABEL[kind as DecisionKind] ?? EVENT_KIND_LABEL[kind] ?? kind));

/** The words a Kinds chip prints before it counts the rest. */
const KINDS_LISTED = 3;

/**
 * The Kinds chip: one whole preset by its name, a union of whole presets and nothing else by their names in
 * KIND_PRESETS' order, else the kinds' words three then " +N", every word in the title. A set equal to every kind, or
 * empty, draws no chip (empty text).
 */
export function kindsChipText(kinds: readonly string[]): ChipText {
  const set = new Set(kinds);
  if (set.size === 0 || (set.size === ALL_KINDS.length && ALL_KINDS.every((k) => set.has(k)))) return { text: '', title: '', values: '' };
  const words = kinds.map(kindWord);
  // The presets share no kind, so whole presets that hold as many kinds as the set are the set.
  const whole = KIND_PRESETS.filter((p) => p.members.every((m) => set.has(m)));
  const values =
    whole.length > 0 && whole.reduce((n, p) => n + p.members.length, 0) === set.size
      ? whole.map((p) => p.label).join(', ')
      : `${words.slice(0, KINDS_LISTED).join(', ')}${words.length > KINDS_LISTED ? ` +${words.length - KINDS_LISTED}` : ''}`;
  return { text: `kinds: ${values}`, title: words.join(', '), values };
}

const EVENT_TONE: Record<string, 'critical' | 'serious' | 'warning'> = {
  account_state_changed: 'serious',
  budget_reached: 'serious',
  cooldown_set: 'warning',
  ledger_gap: 'serious',
  model_rejected: 'warning',
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
  fit: 'Fit',
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
  reserve: 'Reserve',
  'rung:content': 'Rung: content',
  'rung:static': 'Rung: static',
  'rung:with_main': 'Rung: with main',
  selection: 'Rest of the walk',
};

const GOOD = new Set(['booked', 'bound', 'chosen', 'fits', 'match', 'passed', 'placed', 'renewed']);
const WARNING = new Set(['blocked', 'dropped', 'fell_through', 'fired', 'held', 'least_bad', 'moved', 'preferred', 'released', 'rewrite_only']);
const CRITICAL = new Set(['answered', 'exhausted', 'refused']);
const NEUTRAL = new Set(['found', 'no', 'no_runway', 'none', 'not_run', 'off']);

/**
 * Every word the scheme names a tone for, the neutral ones included, so a fixture test can tell a toned word from an
 * unknown one.
 */
export const TONED_VERDICTS: ReadonlySet<string> = new Set([...GOOD, ...WARNING, ...CRITICAL, ...NEUTRAL]);

/**
 * One tone per verdict word across the log's rows, the card and the strip: good for a verdict that served the request,
 * warning for one that changed its course, critical for a refusal or an exhaustion, neutral for found, no, none,
 * not_run, off and a word the scheme does not know. The eligible step's `none` is critical, since no member was
 * eligible and the request had nowhere to go.
 */
export function verdictTone(verdict: string, stepId?: string): VerdictTone {
  if (verdict === 'none' && stepId === 'eligible') return 'critical';
  return GOOD.has(verdict) ? 'good' : WARNING.has(verdict) ? 'warning' : CRITICAL.has(verdict) ? 'critical' : 'neutral';
}

export const words = (name: string) => name.replace(/_/g, ' ');
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

const msOf = (value: number | string) => (typeof value === 'number' ? value : Date.parse(value));

/** A fact's value drawn by its unit: 1.34 %/h, 15:42, 2 min 41 s, 31.2K, $20.40. A value of no unit reads as sent. */
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
/** What a probe's pinned member could not get past. Any other name reads as sent. */
const OBSTACLE_WORD: Record<string, string> = { attempted: 'already tried', budget_unknown: 'budget unknown', operator: 'by hand', subscription_ended: 'subscription ended' };
/**
 * A key refusal's words by the structured code the record carries. A code outside the map reads through the fallback.
 */
const KEY_REFUSAL_WORD: Record<string, string> = {
  disabled: 'key disabled',
  expired: 'key expired',
  pool_disabled: 'pool disabled',
  pool_missing: 'pool missing',
  replaced: 'key replaced',
  revoked: 'key revoked',
  unknown: 'key unknown',
};

/** How the strategy ranks, the hover of a Pace chip on a routing log row. */
export const PACE_HINT = "Pace: the account with the most of its window left per hour until the window resets or the subscription ends, less the burn of the sessions already on it, wins. A request reads the model's own window when the model has one, else the 7-day window, and a window the account has not read counts as untouched.";

/** The strategy's word with the tier's priority when the tier is known: `Pace P0`, `round robin`. */
export function strategyWord(strategy: string, tier: number[] | null): string {
  return tier === null ? wordOf(STRATEGY_WORD, strategy) : `${wordOf(STRATEGY_WORD, strategy)} P${tier[1]}`;
}

/** The strategy's word and the tier's priority (`Pace P0`), a Pace chip with how the strategy ranks as its hover. */
const strategyChip = (strategy: string, tier: DecisionSummary['tier']): Chip => {
  const value = strategyWord(strategy, tier);
  return strategy === 'pace' ? { title: PACE_HINT, value } : { value };
};

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
      return value === 'strategy_pick' ? strategyChip(row.strategy, row.tier) : { value: words(value) };
    case 'none':
      return { value: 'no rule fired' };
    case 'refusal':
      if (value === 'client') return { value: 'client drift' };
      return { value: row.refusal && own(KEY_REFUSAL_WORD, row.refusal.code) ? KEY_REFUSAL_WORD[row.refusal.code] : `${words(value)} refused` };
    case 'rule':
      return { value: `${words(value)} rule` };
    case 'rung':
      return { value: wordOf(RUNG_WORD, value) };
    case 'strategy':
      return strategyChip(value, row.by.tier ?? row.tier);
  }
}

const str = (v: unknown): string | null => (typeof v === 'string' ? v : null);
const num = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) ? v : null);
const list = (v: unknown): unknown[] => (Array.isArray(v) ? v : []);
const rec = (v: unknown): Record<string, unknown> => (v !== null && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : {});

/** A window's name as the windows read elsewhere in the console. */
export const WINDOW_WORD: Record<string, string> = { five_hour: '5h', seven_day: '7d', seven_day_fable: 'Fable' };
const STATE_WORD: Record<string, string> = { funding_required: 'funding required', reauth_required: 're-auth required' };
/** The registry's word for who moved an account's state. */
const CAUSE_WORD: Record<string, string> = { auth_failed: 'credential refused', funding_refused: 'credit balance', funding_retry: 'funding retried', operator: 'by hand', refresh_failed: 'refresh failed' };
const CLEARED_WORD: Record<string, string> = { admin: 'by hand', expired: 'deadline passed' };
const CREDITS_WORD: Record<string, string> = { org_spend_cap_reached: 'spend cap reached' };
const PERIOD_WORD: Record<string, string> = { day: 'daily budget', month: 'monthly budget' };
const REGISTRY_VERB: Record<string, string> = { updated: 'edited' };
/** The pool fields a registry edit reads as a share rather than a bare number, a row written before the rename among them. */
const SHARE_FIELDS = new Set(['pace_floor', 'select_below_utilization', 'shed_ahead_of_pace', 'shed_cap', 'shed_share', 'window_ceiling']);
/** A row written before the rename keeps the old field names: a renamed field reads its new word, a removed one says so. */
const FIELD_LABEL: Record<string, string> = {
  burn_window_seconds: 'burn average seconds (then burn_window_seconds)',
  pace_floor: 'pace floor (removed)',
  select_below_utilization: 'select below utilization (removed)',
  shed_ahead_of_pace: 'shed ahead of pace (removed)',
  shed_share: 'shed cap (then shed_share)',
  source_dwell_seconds: 'shed dwell seconds (then source_dwell_seconds)',
};
const fieldLabel = (field: string) => FIELD_LABEL[field] ?? words(field);

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
  if (reason !== 'rate_limit') return words(reason);
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
    // The usage service opens a backoff on a rate limit alone, so the kind decides the word.
    case 'poll_backoff_started':
      return { value: POLL_OUTCOME_LABEL.rate_limited };
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
    return ms === null || repeatsAt(e, 'time', ms) ? [] : [pair(key, ms, 'time', now)];
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
    case 'refresh_failed':
    case 'refresh_uncertain': {
      const cause = str(d.cause);
      return cause === null ? [] : [pair('cause', cause, 'text', now)];
    }
    case 'refresh_succeeded':
      return time('until', d.access_expires_at);
    case 'registry_changed': {
      // Only a diff whose every entry is a change names its fields, and the whole row the store writes on an edit names
      // none.
      const diff = Object.entries(rec(d.diff));
      if (diff.length === 0 || !diff.every(([, change]) => isChange(change))) return [];
      if (diff.length > 1) return [pair('fields', diff.length, 'count', now)];
      const [field, change] = diff[0];
      return [pair('field', fieldLabel(field), 'text', now), pair('to', fieldValue(field, rec(change).to), 'text', now)];
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
 * One or two pairs, never a time the row's `at` already gives. A failover move reads its answer off the decisions
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
    // The Keys and Pools pages find and focus their entity by the address's hash.
    case 'key':
    case 'pool':
      return `/${kind}s#${encodeURIComponent(id)}`;
    // Neither page has a filter for the entity, so the link opens the view.
    case 'model':
      return '/models';
    case 'session':
      return '/sessions';
  }
}

/** A fixed window as two instants, or a preset ending now by name. */
export type LogWindow = { from: number; to: number } | { range: string };

export type LogLink = { accounts?: string[]; kinds?: string[]; models?: string[]; pools?: string[]; request?: string; session?: string } & LogWindow;

export const identityOf = (row: RoutingLogRow) => (row.type === 'decision' ? `${row.requestId}/${row.seq}` : `event/${row.id}`);

/** The route's order: newest first, a decision before an event at one instant. */
export function compareRows(a: RoutingLogRow, b: RoutingLogRow): number {
  const at = Date.parse(b.at) - Date.parse(a.at);
  if (at !== 0) return at;
  if (a.type !== b.type) return a.type === 'decision' ? -1 : 1;
  if (a.type === 'decision' && b.type === 'decision') return a.requestId === b.requestId ? b.seq - a.seq : a.requestId < b.requestId ? 1 : -1;
  return (b as RoutingEvent).id - (a as RoutingEvent).id;
}

/** The log's address for `link` through its codec: a preset by name, nothing for the hour, and a window as two instants. */
export function logHref(link: LogLink): string {
  const range: RangeValue = 'range' in link ? { from: '', preset: link.range, to: '' } : { from: iso(link.from), preset: 'custom', to: iso(link.to) };
  const search = ROUTING_LOG_CODEC.toParams({ accounts: link.accounts ?? [], keys: [], kinds: link.kinds ?? [], models: link.models ?? [], pools: link.pools ?? [], range, request: link.request ?? '', session: link.session ?? '' }).toString();
  return search ? `/routing-log?${search}` : '/routing-log';
}

const QUARTER_HOUR = 15 * MINUTE;

/** A moment: a quarter hour either side of it, `to` at most now. */
export const aroundRequest = (at: number, finishedAt: number | null, now: number): LogLink => ({ from: at - QUARTER_HOUR, to: Math.min((finishedAt ?? at) + QUARTER_HOUR, now) });

/**
 * The one builder for a link that opens the log on a request, `{ request: id, ...aroundRequest(at, finishedAt, now) }`,
 * so no caller can set the range and drop the id.
 */
export const requestLink = (id: string, at: number, finishedAt: number | null, now: number): LogLink => ({ request: id, ...aroundRequest(at, finishedAt, now) });

/**
 * A subject that lives on: the last hour as the log's preset, which reads from the reader's load, or the hour either
 * side of its last activity, `to` at most now.
 */
export const aroundSubject = (lastActivity: number | null, now: number): LogLink => (lastActivity === null ? { range: '1h' } : { from: lastActivity - HOUR, to: Math.min(lastActivity + HOUR, now) });

/** A condition: from its start to now. */
export const sinceCondition = (since: number, now: number): LogLink => ({ from: since, to: now });

/** The clock minute that holds `at`, its last millisecond the end, `to` at most now. */
export const clockMinute = (at: number, now: number): LogLink => {
  const from = Math.floor(at / MINUTE) * MINUTE;
  return { from, to: Math.min(from + MINUTE - 1, now) };
};

// Held at module level, as the time format is, so `compareModels` keeps the shape `Array.sort` takes.
let modelTiers: ModelTiers = { modelTier: {}, tierOrder: [] };

/** The two tier members the shell read off the settings answer. The empty pair compares by version and id alone. */
export const setModelTiers = (tiers: ModelTiers): void => {
  modelTiers = tiers;
};

/** Model ids in the cost model's tier order, newest version first, an id of a tier the order lacks last. */
export function compareModels(a: string, b: string): number {
  return compareByModelTier(modelTiers, a, b);
}

// The decision card and the event detail: the record's walk, its candidates, its folds and its outcome, and an event's
// pairs. Every chip holds one value or one key-value pair, and every entity it names is a link.

const accountEntity = (id: string): FactEntity => ({ id, kind: 'account' });
export const accountChip = (id: string, label?: string): Chip => ({ entity: accountEntity(id), ...(label === undefined ? {} : { label }), value: id });
const modelChip = (id: string, label?: string): Chip => ({ entity: { id, kind: 'model' }, ...(label === undefined ? {} : { label }), mono: true, value: id });
const sessionChip = (id: string): Chip => ({ href: entityHref('session', id), mono: true, title: id, value: id.slice(0, 8) });
const requestChip = (id: string): Chip => ({ href: entityHref('request', id), mono: true, value: id });
export const plain = (value: string, label?: string): Chip => (label === undefined ? { value } : { label, value });

/** Why a member could not take the request, the hover of the candidate table's verdict chip. */
export const OBSTACLE_HINT: Record<string, string> = {
  above_ceiling: 'above the utilization ceiling',
  at_cap: 'at its in-flight cap',
  attempted: 'already tried by this request',
  budget: 'at its spend budget for the period',
  budget_unknown: 'its spend unknown while the ledger is degraded',
  cooldown: 'in a cooldown',
  credits: 'its usage credits unavailable for fast mode',
  floor: 'at the Pace floor',
  operator: 'held by an operator',
  overage: 'in overage',
  ranked: 'ranked behind the account chosen',
  spent: 'its window spent',
  subscription_ended: 'its subscription ended',
  winding_down: 'winding down toward its subscription end',
};
const hintOf = (name: string) => (own(OBSTACLE_HINT, name) ? OBSTACLE_HINT[name] : undefined);

/** How a rung placed the request, the hover of the card's rung chip. */
export const PLACED: Record<Placement['rung'], (depth: number) => string> = {
  content: (depth) => `placed by its messages, depth ${depth}`,
  static: (depth) => `placed by its static prefix, depth ${depth}`,
  with_main: () => 'placed with the main conversation',
};
export const HOLD_HINT = 'Every account the placement could land on was at its in-flight cap, so the request waited first';
/** What a placement's cache read is weighed against, the hover of the card's cache verdict. */
export const PROMISE_HINT: Record<Placement['rung'], string> = {
  content: 'A placement by its messages hits when the cache reads the whole prefix it was promised',
  static: 'A placement by its static prefix hits when the cache reads anything',
  with_main: 'A placement beside the main conversation hits when the cache reads anything',
};

/** A boundary's hash as its chip draws it. The whole 64 characters ride the chip's hover. */
export const shortHash = (hash: string) => hash.slice(0, 8);

/** When the entry stops being warm, its write plus its TTL. A link the cache never confirmed (`ttlMs` 0) names none. */
export const warmUntil = (entry: PrefixEntry, now: number): string | null => (entry.ttlMs === 0 ? null : `warm until ${instant(Date.parse(entry.writtenAt) + entry.ttlMs, now)}`);

/** The hash chip's hover: the whole hash, and the gate that refused the entry when one did. */
export const hashTitle = (entry: PrefixEntry): string => (entry.refusedBy === null ? entry.hash : `${entry.hash}\nrefused by the ${words(entry.refusedBy)}`);

const VERDICT_WORD: Record<string, string> = { dropped: 'all at cap' };
/** A walk verdict in the console's words: `not run`, `all at cap`, `fell through`. */
export const verdictWord = (verdict: string) => wordOf(VERDICT_WORD, verdict);

/**
 * `hash` is the entry the rung accepted on the member it took (`contentMatch` or `staticMatch`) on a `match` line and
 * the deepest refused entry on a `none` line, never the taken member's deeper refused entry.
 */
export interface WalkLine {
  chips: Chip[];
  collapsed?: string[];
  hash?: PrefixEntry | null;
  number: number;
  step: string;
  verdict: string;
}

/** The fact keys of the walk in the console's words. Any other key reads with its underscores as spaces. */
export const WALK_KEY: Record<string, string> = {
  above_ceiling: 'above ceiling',
  ahead_five_hour: '5h ahead',
  ahead_seven_day: '7d ahead',
  at_cap: 'at cap',
  binding: 'binding',
  bound_ms: 'bound',
  burn_five_hour: 'burn 5h',
  burn_seven_day: 'burn 7d',
  burn_seven_day_fable: 'burn Fable',
  deadline_five_hour: "5h to its subscription's end at",
  deadline_seven_day: "7d to its subscription's end at",
  dwell_age_ms: 'dwell age',
  elapsed_five_hour: '5h elapsed',
  elapsed_seven_day: '7d elapsed',
  excess: 'excess',
  expires_at: 'expires at',
  grace_ms: 'grace',
  horizon_five_hour: 'horizon 5h',
  horizon_seven_day: 'horizon 7d',
  horizon_seven_day_fable: 'horizon Fable',
  idle_ms: 'idle',
  last_shed_age_ms: 'last round',
  migrate_when_spent: 'move when spent',
  prefix_tokens: 'prefix',
  projection_five_hour: 'projection 5h',
  projection_seven_day: 'projection 7d',
  projection_seven_day_fable: 'projection Fable',
  refresh_age_ms: 'refresh age',
  reserved_five_hour: 'reserved 5h',
  reserved_seven_day: 'reserved 7d',
  reserved_seven_day_fable: 'reserved Fable',
  reset_in_grace: 'reset in',
  rewrite_five_hour: 'rewrite 5h',
  rewrite_seven_day: 'rewrite 7d',
  rewrite_seven_day_fable: 'rewrite Fable',
  runway_ms: 'runway',
  trigger: 'trigger',
  wait_ms: 'wait',
  worst_bucket: 'worst bucket',
};
export const keyWord = (key: string) => wordOf(WALK_KEY, key);
/** The keys whose value names itself, so a member's chip reads `primary eligible`, not `primary standing eligible`. */
const SELF_NAMED = new Set(['score', 'standing']);

/** `warmth_of`'s causes in words. */
const WARMTH_CAUSE_WORD: Record<string, string> = { chainless: 'no chain', class_auxiliary: 'auxiliary class', class_refreshes: 'refreshing class', continues_pin: 'continues the pin', count: 'token count' };
const WARMTH_WORD: Record<string, string> = { refresh: 'warm' };
const SNAKE = /^[a-z][a-z0-9]*(?:_[a-z0-9]+)+$/;
const FACT_UNITS: readonly FactUnit[] = ['count', 'ms', 'percent', 'percent_per_hour', 'text', 'time', 'tokens', 'usd'];
const unitOf = (unit: string | null): FactUnit | null => FACT_UNITS.find((u) => u === unit) ?? null;

/** A word the engine writes, in the console's words. An identifier such as a model or an id reads as written. */
function textWord(key: string, value: string): string {
  switch (key) {
    case 'cause':
      return wordOf(WARMTH_CAUSE_WORD, value);
    case 'strategy':
      return wordOf(STRATEGY_WORD, value);
    case 'warmth':
      return wordOf(WARMTH_WORD, value);
    case 'binding':
    case 'trigger':
    case 'window':
      return wordOf(WINDOW_WORD, value);
    default:
      return SNAKE.test(value) ? words(value) : value;
  }
}

/** A rewrite or a booking is a small share of its window, so it reads to three figures where a whole percent would read 0%. */
const SHARE_KEY = /^(reserved|rewrite)_/;

export const valueText = (key: string, value: unknown, unit: string | null, now: number): string => {
  if (value === null || value === undefined) return 'none';
  if (typeof value === 'boolean') return value ? 'yes' : 'no';
  if (typeof value === 'number' && key === 'runway_ms' && value === 0) return 'none';
  if (typeof value === 'number' && unit === 'percent' && SHARE_KEY.test(key)) return fmtShare(value);
  if (typeof value === 'number') return formatFact(value, unitOf(unit), now);
  if (typeof value === 'string') return unit === 'time' ? formatFact(value, 'time', now) : textWord(key, value);
  return String(value);
};

/** A tier as the engine keys it, `[group, priority, overage]`, from the list or the `0/0/0` text a walk fact holds. */
export const tierOf = (value: unknown): number[] | null => {
  const parts = Array.isArray(value) ? value.map(Number) : typeof value === 'string' ? value.split('/').map(Number) : [];
  return parts.length === 3 && parts.every((n) => Number.isFinite(n)) ? parts : null;
};
/** The kind tier's word: an API key, overage, or the plan's own quota. */
const tierWord = (tier: readonly number[], kind: RecordCandidate['kind'] | null, plan: string) => (kind === 'api_key' || tier[0] === 2 ? 'keys' : tier[0] === 1 || tier[2] === 1 ? 'overage' : plan);

/** A pin's lane is `model|speed|thinking|effort`. The model is the request's own, so the chips name the other three. */
export function laneChips(lane: string): Chip[] {
  const parts = lane.split('|');
  if (parts.length !== 4) return [plain(lane, 'lane')];
  const [, speed, thinking, effort] = parts;
  return [plain(speed, 'speed'), plain(thinking, 'thinking'), plain(effort === '-' ? 'none' : effort, 'effort')];
}

/**
 * An exhausted line's obstacle, `account:name` as the engine weighs it, the account as the key. An operator obstacle
 * reads the member's own pause or state, as its candidate row does.
 */
function obstacleChip(text: string, record: DecisionRecord): Chip {
  const at = text.lastIndexOf(':');
  const name = at < 0 ? text : text.slice(at + 1);
  const id = at < 0 ? '' : text.slice(0, at);
  const member = name === 'operator' ? memberOf(record, id) : undefined;
  const { title, value } = member ? operatorWords(member) : { title: hintOf(name), value: wordOf(OBSTACLE_WORD, name) };
  return id === '' ? { label: 'obstacle', title, value } : { label: id, labelEntity: accountEntity(id), title, value };
}

/**
 * A shedding mark is its pin's key, `key/session/agent/model`. The session's prefix and the agent tell a round's marks
 * apart.
 */
export function markChip(text: string): Chip {
  const [, session = '', agent = ''] = text.split('/');
  return { ...(session ? { href: entityHref('session', session) } : {}), label: 'mark', mono: true, title: text, value: agent ? `${session.slice(0, 8)}/${agent}` : session.slice(0, 8) };
}

/**
 * A pin a round passed over: its session and why, `session: reason`. The engine writes it as the pin's key,
 * `key/session/agent/model`, then the reason.
 */
function skipChip(session: string, reason: string, title: string, label?: string): Chip {
  return { ...(session ? { href: entityHref('session', session) } : {}), ...(label === undefined ? {} : { label }), title, value: `${session.slice(0, 8)}: ${words(reason)}` };
}

/** A drift refusal's client version: the summary's `client` fact carries it, the record's reason the trigger alone. */
function clientVersionOf(row: DecisionSummary | undefined): string | null {
  const value = row?.fact.find((f) => f.key === 'client')?.value;
  return typeof value === 'string' ? value : null;
}

/** A refusal's reason: a drift's trigger beside the client's version, or a key refusal's words by its code, its
 *  sentence for a code the console has no words for. */
function refusalChips(refusal: { code: string; kind: string; reason: string }, version: string | null): Chip[] {
  if (refusal.kind !== 'client') return [plain(own(KEY_REFUSAL_WORD, refusal.code) ? KEY_REFUSAL_WORD[refusal.code] : refusal.reason)];
  return [plain(words(refusal.reason || 'client'), 'drift'), ...(version ? [plain(version, 'client')] : [])];
}

export function factChips(f: WalkFact, record: DecisionRecord, now: number): Chip[] {
  const v = f.value;
  const members = new Set(record.candidates.map((c) => c.accountId));
  switch (f.key) {
    // The rung line draws its entry from the candidate table instead, eight characters with the rest on hover.
    case 'hash':
      return [];
    case 'account_id':
      return typeof v === 'string' ? [accountChip(v, 'on')] : [];
    case 'lane':
      if (typeof v === 'string') return laneChips(v);
      break;
    case 'marks':
      if (Array.isArray(v)) return v.map((m) => markChip(String(m)));
      break;
    case 'skipped':
      if (Array.isArray(v)) {
        return v.map((x) => {
          const text = String(x);
          const at = text.lastIndexOf(': ');
          const [, session = ''] = (at < 0 ? text : text.slice(0, at)).split('/');
          return skipChip(session, at < 0 ? '' : text.slice(at + 2), text, 'skipped');
        });
      }
      break;
    case 'obstacles':
      if (Array.isArray(v)) return v.map((o) => obstacleChip(String(o), record));
      break;
    case 'tier': {
      const tier = tierOf(v);
      if (tier) return [plain(`${tierWord(tier, null, 'plan')} P${tier[1]}`, 'tier')];
      break;
    }
  }
  if (Array.isArray(v)) {
    const label = keyWord(f.key).replace(/s$/, '');
    return v.map((x) => (members.has(String(x)) ? accountChip(String(x), label) : plain(textWord(f.key, String(x)), label)));
  }
  const value = valueText(f.key, v, f.unit, now);
  if (f.accountId) return [{ label: f.accountId, labelEntity: accountEntity(f.accountId), value: SELF_NAMED.has(f.key) ? value : `${keyWord(f.key)} ${value}` }];
  return [plain(value, keyWord(f.key))];
}

export const RUNG_OF: Record<string, 'content' | 'static' | 'with_main'> = { 'rung:content': 'content', 'rung:static': 'static', 'rung:with_main': 'with_main' };

/**
 * A rung line's entry: on a match the one the rung accepted on the member it took, on any other line the deepest entry
 * a member's gate refused. A placement beside the main conversation names no entry.
 */
export function rungEntry(s: WalkStep, record: DecisionRecord): PrefixEntry | null | undefined {
  const rung = own(RUNG_OF, s.step) ? RUNG_OF[s.step] : undefined;
  if (rung === undefined) return undefined;
  if (rung === 'with_main' || s.verdict === 'not_run') return null;
  if (s.verdict === 'match') {
    const took = s.facts.find((f) => f.key === 'account_id');
    const member = record.candidates.find((c) => c.accountId === (took?.accountId ?? took?.value));
    return (rung === 'static' ? member?.prefix.staticMatch : member?.prefix.contentMatch) ?? null;
  }
  const refused = record.candidates.map((c) => c.prefix[rung]).filter((e): e is PrefixEntry => e !== null && e.refusedBy !== null);
  return refused.sort((a, b) => b.depth - a.depth)[0] ?? null;
}

const ruleWord = (step: string) => (STEP_NAME[step] ?? words(step.slice('move:'.length))).toLowerCase();

/**
 * The walk as numbered lines in the engine's order. The rules after a fired one never ran, so they fold into one line,
 * "Rules after spent", with the rules as chips. A drift refusal's admission line reads the client's version off `row`.
 */
export function walkLines(record: DecisionRecord, row?: DecisionSummary): WalkLine[] {
  const at = Date.parse(record.request.at);
  const lines: WalkLine[] = [];
  let fired: string | null = null;
  for (const s of record.walk) {
    if (fired !== null && s.step.startsWith('move:') && s.verdict === 'not_run') {
      const last = lines[lines.length - 1];
      if (last?.collapsed) {
        last.collapsed.push(s.step);
        last.chips.push(plain(ruleWord(s.step)));
      } else {
        lines.push({ chips: [plain(ruleWord(s.step))], collapsed: [s.step], number: lines.length + 1, step: `Rules after ${ruleWord(fired)}`, verdict: 'not_run' });
      }
      continue;
    }
    if (s.step.startsWith('move:') && s.verdict === 'fired') fired = s.step;
    const refusal = record.outcome.refusal;
    const chips =
      s.step === 'admission' && refusal !== null
        ? refusalChips(refusal, clientVersionOf(row))
        : s.step === 'selection' && s.facts.length === 0
          ? ['moves', 'rungs', 'partitions'].map((p) => plain(p))
          : s.facts.flatMap((f) => factChips(f, record, at));
    const entry = rungEntry(s, record);
    lines.push({ chips, ...(entry === undefined ? {} : { hash: entry }), number: lines.length + 1, step: STEP_NAME[s.step] ?? words(s.step), verdict: s.verdict });
  }
  return lines;
}

const memberOf = (record: DecisionRecord, id: string | null) => (id === null ? undefined : record.candidates.find((c) => c.accountId === id));

/** The verdict line's words, with the accounts and the pool as entities: "Moved", secondary, "to", primary. */
export function verdictSentence(record: DecisionRecord, row: DecisionSummary): Chip[] {
  const o = record.outcome;
  const failover = row.attempt > 1 && row.attemptCount !== null && row.attempt <= row.attemptCount;
  const lead = (text: string) => plain(failover ? `Attempt ${row.attempt} of ${row.attemptCount}: ${text.charAt(0).toLowerCase()}${text.slice(1)}` : text);
  const on = (id: string | null) => (id === null ? [] : [accountChip(id)]);
  switch (o.kind) {
    case 'exhausted':
      return [plain('Exhausted in'), { entity: { id: record.request.poolId, kind: 'pool' }, value: record.request.poolId }];
    case 'held':
      return [lead('Held on'), ...on(o.hold?.accountId ?? o.accountId)];
    case 'moved':
      return [lead('Moved'), ...on(o.fromAccountId), plain('to'), ...on(o.accountId)];
    case 'refused':
      return [plain('Refused before selection')];
    case 'renewed':
      return [lead('Renewed on'), ...on(o.accountId)];
    default:
      return [lead('Placed on'), ...on(o.accountId)];
  }
}

const ttlWord = (ttlMs: number) => (ttlMs >= HOUR ? '1h' : '5 min');

/** A share of each plan window in the console's order, `5h 10.4%`, then any other window under its key. */
function shareChips(shares: Record<string, unknown>): Chip[] {
  const keys = [...WINDOWS.filter((w) => own(shares, w)), ...Object.keys(shares).filter((w) => !WINDOWS.includes(w))];
  return keys.flatMap((w) => {
    const v = num(shares[w]);
    return v === null ? [] : [plain(fmtShare(v), wordOf(WINDOW_WORD, w))];
  });
}

/** `for the reset at 18:20`, or the reset alone where the hold carried no instant. */
const resetWords = (hold: NonNullable<RecordOutcome['hold']>, now: number) => (hold.until === null ? 'for the reset' : `for the reset at ${formatFact(hold.until, 'time', now)}`);

/** The tier, the rule or the rung, the pin's warmth and the cache verdict once the attempt has usage. */
export function verdictChips(record: DecisionRecord, row: DecisionSummary): Chip[] {
  const o = record.outcome;
  const chips: Chip[] = [];
  const member = memberOf(record, o.accountId);
  const tier = o.tier ?? member?.tier ?? null;
  if (tier !== null && o.accountId !== null) chips.push(plain(`${tierWord(tier, member?.kind ?? null, 'plan quota')} P${tier[1]}`, 'tier'));
  const rung = o.rung;
  const placed = rung === null ? undefined : PLACED[rung.rung](rung.depth);
  if (row.by.kind === 'rung') {
    chips.push({ ...decisionBy(row), title: placed });
    if (rung !== null && rung.rung !== 'with_main') chips.push(plain(String(rung.depth), 'depth'));
  } else {
    chips.push(decisionBy(row));
    // A move or a hold's placement can land by a rung too, so its depth is keyed by the rung, never by the rule.
    if (rung !== null) chips.push({ ...(rung.rung === 'with_main' ? plain(RUNG_WORD.with_main) : plain(`depth ${rung.depth}`, RUNG_WORD[rung.rung])), title: placed });
  }
  if (o.kind === 'held' && o.hold?.reason === 'reset') chips.push(plain(`held ${resetWords(o.hold, Date.parse(record.request.at))}`));
  // The chip names the first plan window the booking holds a share of, the 5-hour one wherever it does.
  const booked = o.reservation === null ? undefined : WINDOWS.find((w) => num(o.reservation?.rewrite[w]) !== null);
  if ((o.kind === 'moved' || o.kind === 'placed') && o.reservation !== null && booked !== undefined) chips.push(plain(`reserved ${fmtShare(o.reservation.rewrite[booked])} of ${WINDOW_WORD[booked]}`));
  if (record.pin?.found === 'live' && record.pin.warmth) chips.push(plain(wordOf(WARMTH_WORD, record.pin.warmth), 'pin'));
  const c = o.completion;
  const promise = rung !== null && c !== null && c.promisedTokens !== null ? PROMISE_HINT[rung.rung] : undefined;
  if (c !== null && c.hit !== null && c.cacheReadTokens !== null) {
    const read = fmtCompact(c.cacheReadTokens);
    chips.push({ ...plain(c.promisedTokens !== null ? `${read} of ${fmtCompact(c.promisedTokens)} promised` : `${read} of ${fmtCompact(c.cacheReadTokens + (c.inputTokens ?? 0))}`, c.hit ? 'cache hit' : 'cache miss'), title: promise });
  } else if (c !== null && c.completedAt === null) {
    chips.push(plain('cache pending'));
  }
  return chips;
}

const WINDOWS = ['five_hour', 'seven_day', 'seven_day_fable'];
/** A reading's age, whole seconds under a minute. */
export const ageWord = (ms: number) => (ms < MINUTE ? `${Math.round(ms / 1_000)} s` : span(ms));
const ordinal = (n: number) => `${n}${n % 100 >= 11 && n % 100 <= 13 ? 'th' : (['th', 'st', 'nd', 'rd'][n % 10] ?? 'th')}`;
const VERDICT_BADGE: Record<string, string> = { held_on: 'held on', not_needed: 'not needed' };
const PARTITION_WORD: Record<string, string> = { above_ceiling: 'above the ceiling', at_cap: 'at its cap', floor: 'at the Pace floor', spent: 'window spent', winding_down: 'winding down' };

/** The plan beneath a member's account: `Max 20x`, `Pro`, `API key`. */
const planWord = (c: RecordCandidate) => (c.kind === 'api_key' ? 'API key' : c.planTier === null ? 'OAuth' : planLabel(c.planTier));

/** The member's prefix entry: the match on the rung the outcome names, else the deepest entry a gate refused. */
function prefixEntryOf(c: RecordCandidate, record: DecisionRecord): PrefixEntry | null {
  const rung = record.outcome.rung?.rung;
  const match = rung === 'static' ? c.prefix.staticMatch : rung === 'content' ? c.prefix.contentMatch : (c.prefix.contentMatch ?? c.prefix.staticMatch);
  if (match) return match;
  const order = rung === 'static' ? [c.prefix.static, c.prefix.content] : [c.prefix.content, c.prefix.static];
  return order.find((e): e is PrefixEntry => e !== null && e.refusedBy !== null) ?? null;
}

function cooldownWords(o: NonNullable<RecordCandidate['obstacle']>, now: number): string {
  const until = o.until === null ? '' : ` until ${formatFact(o.until, 'time', now)}`;
  const reason = o.reason ?? '';
  if (reason === 'model_rejected' || o.window === 'model_rejection') return `model 404${until}`;
  if (reason === 'fast_rate_limit' || o.window === 'fast') return `fast lane 429${until}`;
  if (reason === 'rate_limit') return o.window !== null && own(WINDOW_WORD, o.window) ? `429 on ${WINDOW_WORD[o.window]}${until}` : `429${until}`;
  return `${words(reason || 'cooldown')}${until}`;
}

/** The budget a member's budget obstacle names by its period. */
const reachedBudget = (c: RecordCandidate) => c.budget.find((b) => b.period === c.obstacle?.reason);

/** A member's state other than ready, as the console's account views word it. */
const MEMBER_STATE_WORD: Record<string, string> = { deleted: 'deleted', disconnected: 'disconnected', funding_required: 'needs funding', reauth_required: 'needs re-auth', unpaired: 'not paired' };

/**
 * The engine's `operator` obstacle covers a pause and a state other than ready alike, so the member's own `enabled`
 * and `state` tell them apart, in the candidate table and the exhausted line alike. A deleted member reads disabled
 * too, and its state says why.
 */
function operatorWords(c: RecordCandidate): { title: string | undefined; value: string } {
  if (!c.enabled && c.state !== 'deleted') return { title: hintOf('operator'), value: 'held by hand' };
  const title = c.state === 'deleted' ? 'the account left the registry' : own(STATE_EFFECT, c.state) ? STATE_EFFECT[c.state] : hintOf('operator');
  return { title, value: wordOf(MEMBER_STATE_WORD, c.state) };
}

function obstacleWords(c: RecordCandidate, now: number): string {
  const o = c.obstacle;
  if (o === null) return '';
  switch (o.name) {
    case 'budget': {
      const b = reachedBudget(c);
      return b && b.spentUsd !== null ? `budget ${fmtUsd(b.spentUsd)} of ${fmtUsd(b.budgetUsd)}` : 'budget';
    }
    case 'cooldown':
      return cooldownWords(o, now);
    case 'credits':
      return `credits ${o.reason === null ? 'unavailable' : wordOf(CREDITS_WORD, o.reason)}`;
    case 'operator':
      return operatorWords(c).value;
    case 'spent':
      return `${o.window !== null ? wordOf(WINDOW_WORD, o.window) : 'window'} spent`;
    default:
      return own(PARTITION_WORD, o.name) ? PARTITION_WORD[o.name] : wordOf(OBSTACLE_WORD, o.name);
  }
}

/** The chip of a destination the gate read and found short: rewrite only and least bad warn, an ending account is neutral. */
const FIT_CHIP: Record<string, { tone: VerdictTone; word: string }> = { ending: { tone: 'neutral', word: 'ending' }, least_bad: { tone: 'warning', word: 'least bad' }, rewrite_only: { tone: 'warning', word: 'rewrite only' } };

/** The verdict cell's one chip, and the tone it carries where it names the destination's fit. */
function verdictChipOf(c: RecordCandidate, now: number): { chip: Chip | null; tone: VerdictTone | null } {
  const untoned = (chip: Chip | null) => ({ chip, tone: null });
  if (c.obstacle !== null) return untoned(c.obstacle.name === 'operator' ? operatorWords(c) : { title: hintOf(c.obstacle.name), value: obstacleWords(c, now) });
  if (c.partition !== null) return untoned({ title: hintOf(c.partition), value: wordOf(PARTITION_WORD, c.partition) });
  // The cap held the request on its member whatever that member's fit, so the cap explains the hold.
  if (c.verdict === 'held_on' && c.inFlight >= c.inFlightCap) return untoned(plain('at its cap'));
  if (c.fit !== null && own(FIT_CHIP, c.fit)) return { chip: plain(FIT_CHIP[c.fit].word), tone: FIT_CHIP[c.fit].tone };
  return untoned(placementChipOf(c));
}

function placementChipOf(c: RecordCandidate): Chip | null {
  if (c.verdict === 'destination') return plain('not placed');
  if (c.verdict === 'source') {
    const spent = WINDOWS.find((w) => own(c.windows, w) && c.windows[w].utilization >= 1);
    return spent === undefined ? null : plain(`${WINDOW_WORD[spent]} spent`);
  }
  return null;
}

/** A Pace window's word in the column header and its phrase in a cell's hover. Any other window reads its id. */
const PACE_WINDOW_WORD: Record<string, string> = { seven_day: '7-day', seven_day_fable: 'Fable week' };
const PACE_WINDOW_PHRASE: Record<string, string> = { seven_day: 'the 7-day window', seven_day_fable: 'the Fable week' };

/**
 * The Pace column's header: the window the first subscription candidate's score read. It reads "Pace" when that
 * candidate names no window or the record holds no subscription candidate. A version 1 record read no burn, so its
 * score was the share left per hour.
 */
export function paceHeader(record: DecisionRecord): string {
  const pace = record.candidates.find((c) => c.kind === 'oauth')?.pace;
  const id = pace?.window ?? null;
  if (id === null) return 'Pace';
  return (pace?.burn ?? null) === null ? `Pace, ${wordOf(PACE_WINDOW_WORD, id)} left per hour` : `Spare rate, ${wordOf(PACE_WINDOW_WORD, id)} per hour`;
}

/** A Pace cell's hover: the share left, the hours it is spread over and the rate, or an API key's share alone. */
function paceTitle(c: RecordCandidate, now: number): string | null {
  const p = c.pace;
  if (p === null) return null;
  if (c.kind === 'api_key') return `${fmtPct(p.remaining)} of its worst live bucket left. Scored by the remaining share alone, with no division by hours.`;
  // A pace built by hand without the key names no window, as a null one does.
  const id = p.window ?? null;
  const share = id === null ? 'its window' : own(PACE_WINDOW_PHRASE, id) ? PACE_WINDOW_PHRASE[id] : `the ${words(id)} window`;
  // Whole hours would read a deadline under half an hour away as "0 h".
  const over = p.hours === null ? '' : ` over ${p.hours < 1 ? `${Math.round(p.hours * 60)} min` : `${Math.round(p.hours)} h`}`;
  // The deadline is the earlier of the two, so one equal to the reset is the reset whatever the end reads.
  const deadline = p.deadline === null ? ' with no reset read' : ` to ${p.reset !== null && msOf(p.reset) === msOf(p.deadline) ? 'its reset' : "its subscription's end"} at ${formatFact(p.deadline, 'time', now)}`;
  const score = formatFact(p.score, 'percent_per_hour', now);
  // A version 1 record read no burn, and its score was the share left an hour above the pace floor.
  const burn = p.burn ?? null;
  if (burn === null) return `${fmtPct(p.remaining)} of ${share} left${over}${deadline}: ${score}. Inside its tier, the highest above the pace floor wins.`;
  return `${fmtPct(p.remaining)} of ${share} left${over}${deadline}, less ${formatFact(burn, 'percent_per_hour', now)} of burn: ${score}. Inside its tier, the highest spare rate wins.`;
}

/**
 * The projection at each window's horizon the candidate reports, one line a window: the horizon as a time when it is
 * the reset, else as the span to it from the request (one 5-hour length, or the account's end). A candidate the gate
 * did not read reads a dash.
 */
function projectionLines(c: RecordCandidate, record: DecisionRecord, now: number): string[] {
  if (c.fit === null) return ['-'];
  const at = Date.parse(record.request.at);
  const lines = WINDOWS.flatMap((name) => {
    const w = own(c.windows, name) ? c.windows[name] : undefined;
    if (w === undefined || w.projection === null) return [];
    const horizon = w.horizon === null ? null : Date.parse(w.horizon);
    const when = horizon === null ? '' : w.reset !== null && Date.parse(w.reset) === horizon ? ` at ${formatFact(horizon, 'time', now)}` : ` in ${span(horizon - at)}`;
    return [`${WINDOW_WORD[name]} ${fmtPct(w.projection)}${when}`];
  });
  return lines.length > 0 ? lines : ['-'];
}

/** The time a pin could stay before its window that binds reaches the ceiling, unbounded where nothing binds. */
const runwayWord = (c: RecordCandidate) => (c.fit === null ? '-' : c.runwayMs === null ? 'unbounded' : c.runwayMs === 0 ? 'none' : span(c.runwayMs));

/**
 * One member's row of the candidate table: the account and its plan, the tier, the three windows (or a key's one
 * merged reading), the projection and the runway the gate read, the slots, the Pace score with its hover, the prefix
 * entry and the verdict with the one chip that decided it. A chip that names the destination's fit carries its tone.
 */
export function candidateCells(c: RecordCandidate, record: DecisionRecord, now: number): { account: Chip; inFlight: string; pace: string; paceTitle: string | null; plan: string; prefix: { hash: PrefixEntry | null; until: string | null }; projection: string[]; runway: string; tier: string; verdict: { badge: string; chip: Chip | null; tone: VerdictTone | null }; windows: string[][] } {
  const atCap = c.verdict === 'out' && (c.partition === 'at_cap' || c.obstacle?.name === 'at_cap');
  const badge = c.verdict === 'ranked' && c.rank !== null ? `ranked ${ordinal(c.rank)}` : atCap ? 'at cap' : wordOf(VERDICT_BADGE, c.verdict);
  const windows =
    c.kind === 'api_key'
      ? [[own(c.windows, 'worst_bucket') ? `worst bucket ${fmtPct(c.windows.worst_bucket.utilization)}` : 'not reported']]
      : WINDOWS.map((name) => {
          if (!own(c.windows, name)) return ['not reported'];
          const w = c.windows[name];
          return [fmtPct(w.utilization), w.reset === null ? 'no reset' : formatFact(w.reset, 'time', now), `${w.source} ${ageWord(w.ageMs)}`];
        });
  const entry = prefixEntryOf(c, record);
  return {
    account: accountChip(c.accountId),
    inFlight: `${c.inFlight} of ${c.inFlightCap}`,
    pace: c.pace === null ? '-' : c.kind === 'api_key' ? `${fmtPct(c.pace.remaining)} left` : formatFact(c.pace.score, 'percent_per_hour', now),
    paceTitle: paceTitle(c, now),
    plan: planWord(c),
    prefix: { hash: entry, until: entry === null ? null : warmUntil(entry, now) },
    projection: projectionLines(c, record, now),
    runway: runwayWord(c),
    tier: `${tierWord(c.tier, c.kind, 'plan')} P${c.tier[1]}`,
    verdict: { badge, ...verdictChipOf(c, now) },
    windows,
  };
}

/**
 * A setting the card can draw a row of: every member of the record's settings but a version 1 record's `legacy`. The
 * rows leave out the pool's edit instant, which the fold's head reads.
 */
type SettingKey = Exclude<keyof RecordSettings, 'legacy'>;

/** The settings of the card in its order, Strategy to Retry in place. */
export const SETTINGS_ROWS: { key: SettingKey; label: string }[] = [
  { key: 'strategy', label: 'Strategy' },
  { key: 'preferSubscription', label: 'Tier order' },
  { key: 'overagePolicy', label: 'Overage' },
  { key: 'affinityTtlSeconds', label: 'Pin window' },
  { key: 'affinityMode', label: 'Pin when' },
  { key: 'sessionPlacement', label: 'Sessions placed by' },
  { key: 'minSharedPrefixTokens', label: 'Minimum shared prefix' },
  { key: 'subagentPlacement', label: 'Subagents placed by' },
  { key: 'windowCeiling', label: 'Window ceiling' },
  { key: 'loadShedding', label: 'Load shedding' },
  { key: 'migrateWhenSpent', label: 'Move when spent' },
  { key: 'overageMoveGraceMinutes', label: 'Overage grace' },
  { key: 'windDownMinutes', label: 'Wind-down' },
  { key: 'shedCap', label: 'Shed cap' },
  { key: 'burnAverageSeconds', label: 'Burn average' },
  { key: 'shedDwellSeconds', label: 'Shed dwell' },
  { key: 'warmSessions', label: 'Warm sessions' },
  { key: 'inFlightCap', label: 'In-flight cap' },
  { key: 'retryInPlaceSeconds', label: 'Retry in place' },
];

const SECONDS: readonly SettingKey[] = ['affinityTtlSeconds', 'burnAverageSeconds', 'retryInPlaceSeconds', 'shedDwellSeconds'];
const MINUTES: readonly SettingKey[] = ['overageMoveGraceMinutes', 'windDownMinutes'];
const SHARES: readonly SettingKey[] = ['shedCap', 'windowCeiling'];
/** The two settings a version 1 record carried no value for, since the pool had neither when it was written. */
const UNRECORDED: readonly SettingKey[] = ['loadShedding', 'windowCeiling'];

/** One setting's value as the grid draws it: a span, a share, `on` or `off`, or the setting's word. */
export function settingValue(key: SettingKey, settings: RecordSettings): string {
  const v = settings[key];
  if (key === 'strategy') return wordOf(STRATEGY_WORD, String(v));
  if (key === 'preferSubscription') return v ? 'subscriptions first' : 'priority alone';
  if (v === null) return 'off';
  if (typeof v === 'boolean') return v ? 'on' : 'off';
  if (typeof v === 'number') {
    if (SECONDS.includes(key)) return v === 0 ? 'off' : span(v * 1_000);
    if (MINUTES.includes(key)) return span(v * MINUTE);
    if (SHARES.includes(key)) return fmtPct(v);
    if (key === 'minSharedPrefixTokens') return fmtCompact(v);
    return fmtInt(v);
  }
  return words(v);
}

/** The relay's status for an exhaustion: a dated one a 429, a model no member serves a 404, the rest a 503. */
const EXHAUSTION_STATUS: Record<string, number> = { fast_credits: 429, fast_lane: 429, model_rejected: 404, timed: 429 };
export const PLACE_WORD: Record<string, string> = { strategy_pick: 'strategy pick' };

/** The card's outcome: Pin written, Cache-map entries, Cache read, Hold, Answer and Spend, each where it applies. */
export function outcomeRows(record: DecisionRecord, now: number, row?: DecisionSummary): CardRow[] {
  const o = record.outcome;
  const c = o.completion;
  const at = Date.parse(record.request.at);
  const rows: CardRow[] = [];
  if (o.pinWritten !== null && o.accountId !== null) {
    rows.push({ chips: [accountChip(o.accountId, `${o.pinWritten.action} on`), plain(fmtInt(o.pinWritten.epoch), 'epoch'), ...(o.pinWritten.refreshStamped ? [plain(fmtTime(record.request.at), 'refresh')] : [])], label: 'Pin written' });
  }
  if (o.reservation !== null) {
    const r = o.reservation;
    rows.push({ chips: [accountChip(r.accountId, 'on'), ...shareChips(r.rewrite), plain(formatFact(r.expiresAt, 'time', now), 'expires')], label: 'Reservation' });
  }
  if (c !== null) {
    const entries = c.cacheMapWritten ?? [];
    if (entries.length === 0) rows.push({ chips: [plain(c.completedAt === null ? 'pending' : 'none')], label: 'Cache-map entries' });
    else {
      const accounts = [...new Set(entries.map((e) => e.accountId))];
      rows.push({ chips: [...accounts.map((id) => accountChip(id, 'on')), ...entries.map((e) => plain(String(e.depth), `${e.tier} depth`)), plain(fmtCompact(Math.max(...entries.map((e) => e.prefixTokens))), 'prefix')], label: 'Cache-map entries' });
    }
    if (c.hit !== null && c.cacheReadTokens !== null) {
      const read = fmtCompact(c.cacheReadTokens);
      // The request's own TTL is the one its deepest entry was written under.
      const deepest = entries.reduce<(typeof entries)[number] | null>((best, e) => (best === null || e.depth > best.depth ? e : best), null);
      rows.push({
        chips: [plain(c.hit ? 'hit' : 'miss'), plain(c.promisedTokens !== null ? `${read} of ${fmtCompact(c.promisedTokens)} promised` : `${read} of ${fmtCompact(c.cacheReadTokens + (c.inputTokens ?? 0))} input`, 'read'), ...(deepest ? [plain(ttlWord(deepest.ttlMs), 'tier')] : [])],
        label: 'Cache read',
        ...(o.rung !== null && c.promisedTokens !== null ? { title: PROMISE_HINT[o.rung.rung] } : {}),
      });
    } else if (c.completedAt === null) {
      rows.push({ chips: [plain('pending')], label: 'Cache read' });
    }
  }
  // The relay answers a hold whose bound ran out with a 429, whatever the exhaustion the selection after it names.
  const last = record.request.holds[record.request.holds.length - 1];
  const ranOut = last?.ranOut === true;
  const ranOutChip = ranOut ? [plain('ran out')] : [];
  if (o.hold !== null && o.hold.reason === 'reset') {
    // The pin waits on its own account for the window to reset rather than for a slot.
    rows.push({ chips: [accountChip(o.hold.accountId, 'on'), plain(resetWords(o.hold, now)), plain(span(o.hold.boundMs), 'bound'), ...ranOutChip], label: 'Hold' });
  } else if (o.hold !== null) {
    const place = str(record.walk.find((s) => s.step === 'hold')?.facts.find((f) => f.key === 'place')?.value) ?? (o.hold.cohort !== null ? 'cohort' : 'strategy_pick');
    rows.push({ chips: [accountChip(o.hold.accountId, 'on'), plain(wordOf(PLACE_WORD, place)), plain(span(o.hold.boundMs), 'bound'), ...ranOutChip], label: 'Hold', title: HOLD_HINT });
  } else if (o.cohortHold !== null) {
    const h = o.cohortHold;
    const on = h.cohort ?? h.waitedOn[h.waitedOn.length - 1] ?? null;
    const held = c?.heldMs ?? h.heldMs;
    rows.push({ chips: [...(on === null ? [] : [accountChip(on, 'on')]), plain(h.cohort !== null ? 'cohort' : 'strategy pick'), plain(span(held), c?.freed ? 'freed after' : 'held'), ...ranOutChip], label: 'Hold', title: HOLD_HINT });
  } else if (o.exhaustion !== null && last !== undefined && last.attempt === record.request.attempt) {
    // The relay keeps every hold across a failover, and an earlier attempt's hold is no part of this decision.
    rows.push({ chips: [accountChip(last.accountId, 'on'), plain(last.cohort !== null ? 'cohort' : 'strategy pick'), plain(span(last.heldMs), 'held'), ...ranOutChip], label: 'Hold', title: HOLD_HINT });
  }
  if (o.exhaustion !== null) {
    const x = o.exhaustion;
    rows.push({
      chips: [
        plain(String(ranOut ? 429 : own(EXHAUSTION_STATUS, x.kind) ? EXHAUSTION_STATUS[x.kind] : 503), 'status'),
        ...(x.window !== null ? [plain(wordOf(WINDOW_WORD, x.window), 'window')] : []),
        ...(x.reason !== null ? [plain(wordOf(CREDITS_WORD, x.reason), 'cause')] : []),
        plain(x.deadline === null ? 'none' : formatFact(x.deadline, 'time', at), x.deadline === null ? 'retry' : 'retry after'),
      ],
      label: 'Answer',
    });
  }
  if (o.refusal !== null) {
    const version = clientVersionOf(row);
    const said = o.refusal.kind === 'client' ? [plain('client drift'), ...refusalChips(o.refusal, version).filter((ch) => ch.label === 'client')] : refusalChips(o.refusal, version);
    rows.push({ chips: [...said, ...(record.request.keyId ? [{ entity: { id: record.request.keyId, kind: 'key' as const }, label: 'key', value: record.request.keyId }] : [])], label: 'Answer' });
  }
  const budget = (b: RecordCandidate['budget'][number]) => (b.spentUsd === null ? `${fmtUsd(b.budgetUsd)} spend unknown` : `${fmtUsd(b.spentUsd)} of ${fmtUsd(b.budgetUsd)}`);
  const member = memberOf(record, o.accountId);
  if (member && member.budget.length > 0) {
    rows.push({ chips: member.budget.map((b) => plain(budget(b), wordOf(PERIOD_WORD, b.period))), label: 'Spend' });
  } else if (o.exhaustion !== null) {
    const reached = record.candidates.filter((m) => m.obstacle?.name === 'budget');
    const chips = reached.flatMap((m) => {
      const b = reachedBudget(m);
      return b ? [{ label: m.accountId, labelEntity: accountEntity(m.accountId), value: budget(b) }] : [];
    });
    if (chips.length > 0) rows.push({ chips, label: 'Spend' });
  }
  return rows;
}

/** A pair of the event detail. The shedding round's pins are a table of their own. */
export interface DetailPair {
  chips: Chip[];
  label: string;
  table?: { head: string[]; rows: Chip[][] };
}

/** An instant to the second in UTC, the detail's When. */
const utcSecond = (ms: number) => `${new Date(ms).toISOString().slice(0, 19).replace('T', ' ')} UTC`;
const RULE_MOVES = new Set(['drain', 'overage', 'return_to_plan', 'shed', 'spent']);
/** What an account's new state means for routing, by the state it moved to. */
const STATE_EFFECT: Record<string, string> = {
  deleted: 'the account left the registry and this is its last row',
  disconnected: 'the account leaves every eligible set until it is connected again',
  funding_required: 'the account leaves every eligible set until funded',
  ready: 'the account joins the eligible sets',
  reauth_required: 'the account leaves every eligible set until it is paired again',
  unpaired: 'the account leaves every eligible set until it is paired',
};
/** What each reason the ledger writer gives for a gap means. */
const GAP_MEANING: Record<string, string> = {
  attempt_missing: 'the bus dropped an account selection and the trail is short an attempt row',
  capture_budget: 'the capture budget ran out and the largest capture gave up its bodies',
  chunk_dropped: 'the completion counted more chunks than reached the ledger',
  chunk_skipped: 'an interior chunk of the capture was dropped at the bus',
  expired: 'the completion never reached the ledger within the in-flight window',
  in_flight_cap: 'the requests in flight reached their cap and the oldest entry was evicted',
  poison: "a record failed alone after its batch's retries",
};
const SOURCE_WORD: Record<string, string> = { head: 'the response head', poll: 'the poll', predicted: 'the prediction' };
/**
 * A shedding round's rate: a share of the window that fired per hour, on every account, whichever table the round's
 * order read.
 */
const hourlyShare = (perHour: number) => `${(perHour * 100).toFixed(1)} %/h`;
/** A move's destination fit as the gate worded it. */
const FIT_WORD: Record<string, string> = { fits: 'fits', least_bad: 'least bad', rewrite_only: 'rewrite only' };

/** An event's detail as pairs: When, Account and Pool first, then the kind's own. `rows` are the rows the page holds,
 *  which a model rejection reads its cooldown's until off. */
export function detailPairs(row: RoutingEvent, now: number, rows: readonly RoutingLogRow[]): DetailPair[] {
  const d = row.detail;
  const pairs: DetailPair[] = [{ chips: [plain(utcSecond(atOf(row)))], label: 'When' }];
  if (row.accountId !== null) pairs.push({ chips: [accountChip(row.accountId)], label: 'Account' });
  if (row.poolId !== null) pairs.push({ chips: [{ entity: { id: row.poolId, kind: 'pool' }, value: row.poolId }], label: 'Pool' });
  const add = (label: string, ...chips: (Chip | null)[]) => {
    const kept = chips.filter((c): c is Chip => c !== null);
    if (kept.length > 0) pairs.push({ chips: kept, label });
  };
  const time = (v: unknown, label?: string) => {
    const ms = num(v);
    return ms === null ? null : plain(formatFact(ms, 'time', now), label);
  };
  const count = (v: unknown) => {
    const n = num(v);
    return n === null ? null : plain(fmtInt(n));
  };
  const pct = (v: unknown, label?: string) => {
    const n = num(v);
    return n === null ? null : plain(fmtPct(n), label);
  };
  const text = (v: unknown, map: Record<string, string> = {}) => {
    const s = str(v);
    return s === null ? null : plain(wordOf(map, s));
  };
  const key = row.keyId === null ? null : { entity: { id: row.keyId, kind: 'key' as const }, mono: true, value: row.keyId };
  const session = row.sessionId === null ? null : sessionChip(row.sessionId);
  const agent = row.agentId ?? str(d.agent_id);
  const model = row.model === null ? null : modelChip(row.model);
  const request = row.requestId === null ? null : requestChip(row.requestId);
  const pin = () => {
    add('Key', key);
    add('Session', session);
    add('Agent', agent === null ? null : plain(agent));
    add('Model', model);
  };
  const window = str(d.window);
  switch (row.kind) {
    case 'account_state_changed':
      add('From', text(d.from, STATE_WORD));
      add('To', text(d.to, STATE_WORD));
      add('Cause', text(d.cause, CAUSE_WORD));
      add('Detail', str(d.detail) === null ? null : plain(str(d.detail) ?? ''));
      add('Effect', text(d.to, STATE_EFFECT));
      break;
    case 'budget_period_ended': {
      const spent = num(d.previous_spent_usd);
      add('Previous period', text(d.period));
      add('Spent', spent === null ? null : plain(fmtUsd(spent)));
      add('Period start', time(d.period_start));
      break;
    }
    case 'budget_reached': {
      const budget = num(d.budget_usd);
      const spent = num(d.spent_usd);
      add('Budget', budget === null ? null : plain(`${fmtUsd(budget)} a ${str(d.period) ?? 'period'}`));
      add('Spent', spent === null ? null : plain(fmtUsd(spent)));
      add('Period ends', time(d.period_end));
      add('Effect', plain('the pool answers exhausted until then'));
      add('Crossed by', request);
      break;
    }
    case 'catalog_changed':
      // The writer lists the models alphabetically, and every cell that lists models reads them by family and version.
      add('Added', ...list(d.added).map(String).sort(compareModels).map((m) => modelChip(m)));
      add('Removed', ...list(d.removed).map(String).sort(compareModels).map((m) => modelChip(m)));
      add('Models', count(d.count));
      break;
    case 'cooldown_cleared':
    case 'cooldown_set': {
      const scope = str(d.scope) ?? '';
      add('Scope', scope.startsWith('model:') ? modelChip(scope.slice('model:'.length), 'model') : plain(scope === 'fast' ? 'fast lane' : words(scope || 'account')));
      if (row.kind === 'cooldown_cleared') {
        add('Cleared', text(d.cause, CLEARED_WORD));
        break;
      }
      add('Reason', text(d.reason));
      add('Window', window === null ? null : plain(wordOf(WINDOW_WORD, window)));
      add('Until', time(d.until));
      add('Set by', request);
      break;
    }
    case 'fast_credits_changed': {
      const state = (blocked: unknown, side: unknown) => [plain(str(blocked) === null ? 'available' : 'unavailable', 'state'), pct(rec(side).utilization, 'used')];
      if (d.before !== null && d.before !== undefined) add('Before', ...state(d.blocked_before, d.before));
      add('After', ...state(d.blocked_after, d.after));
      add('Cause', str(d.blocked_after) === null ? plain('room') : text(d.blocked_after, CREDITS_WORD));
      add('Effect', plain(str(d.blocked_after) === null ? 'a fast request may draw on this account' : 'a fast request skips this account'));
      break;
    }
    case 'ledger_gap':
      add('Reason', text(d.reason));
      add('Meaning', str(d.reason) !== null && own(GAP_MEANING, str(d.reason) ?? '') ? plain(GAP_MEANING[str(d.reason) ?? '']) : null);
      if (str(d.reason)?.startsWith('chunk')) add('Effect', plain('the captured response is stored truncated'));
      add('Request', request);
      break;
    case 'model_permitted':
    case 'model_rejected': {
      const wire = str(d.wire_model);
      const attempt = num(d.attempt);
      if (row.kind === 'model_permitted') {
        add('Rejected by', ...list(d.rejected_by).map((a) => accountChip(String(a))));
        add('Served at', attempt === null ? null : plain(`attempt ${attempt} of ${attempt}`));
      }
      add('Model sent', wire === null ? null : modelChip(wire));
      add('Model resolved', model);
      if (row.kind === 'model_rejected') {
        add('Cooldown scope', plain('the model on this account'));
        add('Attempt', attempt === null ? null : plain(String(attempt)));
        // The event's own fact is the 404 alone. The until is on the cooldown_set row the same answer wrote, found by
        // the client request since the router dates the two rows with separate clock reads.
        const set = row.requestId !== null && row.model !== null ? rows.find((r): r is RoutingEvent => r.type === 'event' && r.kind === 'cooldown_set' && r.requestId === row.requestId && r.accountId === row.accountId && str(r.detail.scope) === `model:${row.model}`) : undefined;
        add('Until', set === undefined ? null : time(set.detail.until));
      }
      add('Request', request);
      break;
    }
    case 'overage_ended': {
      const since = num(d.since);
      add('Since', time(since));
      add('Lasted', since === null ? null : plain(span(atOf(row) - since)));
      break;
    }
    case 'overage_started':
      add('Effect', plain('requests on this account draw on extra usage'));
      break;
    case 'pin_created':
      pin();
      add('Epoch', count(d.epoch));
      break;
    case 'pin_expired':
      pin();
      add('Reasons', ...list(d.reasons).map((r) => plain(releaseLabel(String(r)))));
      add('Found by', request ?? plain('the sweep'));
      break;
    case 'pin_migrated': {
      const from = str(d.from_account);
      const reason = str(d.reason) ?? '';
      const epoch = num(d.epoch);
      add('From', from === null ? null : accountChip(from));
      add('To', row.accountId === null ? null : accountChip(row.accountId), epoch === null ? null : plain(fmtInt(epoch), 'epoch'));
      add('Reason', plain(moveLabel(reason)));
      add('Moved by', plain(RULE_MOVES.has(reason) ? `${words(reason)} rule` : FAILOVER_MOVES.has(reason) ? 'failover' : words(reason || 'move')));
      // A move the gate read carries its verdict, and a runway no window bounds is written as none at all.
      const fit = str(d.fit);
      if (fit !== null) {
        const runway = num(d.runway_ms);
        add('Destination fit', plain(wordOf(FIT_WORD, fit)));
        add('Runway', plain(runway === null ? 'unbounded' : runway === 0 ? 'none' : span(runway)));
      }
      pin();
      break;
    }
    case 'pin_released':
      add('Actor', plain(row.actor ?? 'admin'));
      pin();
      break;
    case 'poll_backoff_ended': {
      const resumed = num(d.resumed_at) ?? atOf(row);
      const lasted = num(d.duration_ms);
      add('Backed off', lasted === null ? null : plain(formatFact(resumed - lasted, 'time', now), 'from'), plain(formatFact(resumed, 'time', now), 'to'));
      add('Attempts', count(d.attempts));
      add('Resumed', time(d.resumed_at));
      break;
    }
    case 'poll_backoff_started':
      add('Reason', plain(POLL_OUTCOME_LABEL.rate_limited));
      add('Attempts', count(d.attempts));
      add('Next attempt', time(d.next_attempt_at));
      add('Last clean poll', time(d.last_clean_poll_at));
      add('Effect', plain("decisions read this account's windows from response heads alone"));
      break;
    case 'refresh_failed':
    case 'refresh_succeeded':
    case 'refresh_uncertain':
      add('Access token', time(d.access_expires_at, 'valid until'));
      add('Refresh token', time(d.refresh_expires_at, 'valid until'));
      add('Error', text(d.error));
      break;
    case 'registry_changed': {
      const kind = str(d.kind) ?? 'registry_updated';
      const target = str(d.target);
      const subject = kind.slice(0, kind.indexOf('_'));
      add('Change', plain(registryBy(kind)), target === null ? null : subject === 'pool' || subject === 'key' || subject === 'account' ? { entity: { id: target, kind: subject }, value: target } : plain(target));
      const diff = Object.entries(rec(d.diff));
      const primitive = (v: unknown) => v === null || ['boolean', 'number', 'string'].includes(typeof v);
      if (diff.every(([, change]) => isChange(change) && primitive(rec(change).from) && primitive(rec(change).to))) {
        for (const [field, change] of diff) add(fieldLabel(field), plain(fieldValue(field, rec(change).from), 'from'), plain(fieldValue(field, rec(change).to), 'to'));
      } else {
        add('Fields', plain(fmtInt(diff.length)));
      }
      add('Actor', row.actor === null ? null : plain(row.actor));
      // A record holds the settings it was decided under, so one written before the edit reads each field's old value.
      if (diff.every(([, change]) => isChange(change))) {
        const before = formatFact(atOf(row), 'time', now);
        add('Reading it', ...diff.filter(([, change]) => ['boolean', 'number', 'string'].includes(typeof rec(change).from) || rec(change).from === null).map(([field, change]) => plain(`decisions before ${before} carry ${fieldLabel(field)} ${fieldValue(field, rec(change).from)}`)));
      }
      break;
    }
    case 'router_started':
      add('Version', text(d.version));
      add('Pins loaded', count(d.pins_loaded));
      add('Shedding marks loaded', count(d.shedding_marks_loaded));
      add('Window readings loaded', count(d.windows_loaded));
      add('Affinity entries loaded', count(d.affinity_entries_loaded));
      add('Down from', time(d.down_since));
      if (typeof d.holds_lost === 'boolean') add('Held requests', plain(d.holds_lost ? 'answered by the stop' : 'none'));
      break;
    case 'shedding_round': {
      const w = window === null ? 'window' : wordOf(WINDOW_WORD, window);
      const at = atOf(row);
      const projection = num(d.projection);
      const horizon = num(d.horizon);
      const ceiling = num(d.ceiling);
      const rate = (v: unknown, label?: string) => {
        const n = num(v);
        return n === null ? null : plain(hourlyShare(n), label);
      };
      const dwell = num(d.dwell_seconds);
      const average = num(d.burn_average_seconds);
      add('Started by', request, session, agent === null ? null : plain(agent, 'agent'));
      // A weekly window's horizon is one 5-hour length ahead, which reads as the span, and the reset reads as its time.
      const when = horizon === null ? '' : horizon - at === 5 * HOUR ? ` in ${span(horizon - at)}` : ` at ${formatFact(horizon, 'time', now)}`;
      add('Trigger', projection === null || ceiling === null ? null : plain(`${w} projection ${fmtPct(projection)}${when} over the ceiling ${fmtPct(ceiling)}`));
      add('Excess', rate(d.excess));
      add('Cap', pct(d.shed_cap), rate(d.target, 'target'), rate(d.removed, 'removed'));
      add('Unit', text(d.unit));
      add('Burn average', average === null ? null : plain(span(average * 1_000)));
      add('Warm sessions', text(d.warm_sessions));
      const pins = list(d.pins).map(rec);
      if (pins.length > 0) {
        pairs.push({
          chips: [],
          label: 'Pins',
          table: {
            head: ['Session', 'Agent', 'Warmth', 'Burn', 'Last prompt', 'Rewrite', 'Destination', 'Mark'],
            rows: pins.map((p) => {
              const id = str(p.session_id) ?? '';
              const b = num(p.burn);
              const last = num(p.last_input_tokens);
              const rewrite = num(p.rewrite);
              const planned = str(p.planned);
              const skip = str(p.skip);
              return [
                sessionChip(id),
                plain(str(p.agent_id) ?? '-'),
                plain(p.cold === true ? 'cold' : 'warm'),
                plain(b === null ? '-' : hourlyShare(b)),
                plain(last === null ? '-' : fmtCompact(last)),
                plain(rewrite === null ? '-' : fmtShare(rewrite)),
                planned === null ? plain('-') : accountChip(planned),
                plain(p.marked === true ? 'marked' : skip !== null ? words(skip) : (str(p.note) ?? '-')),
              ];
            }),
          },
        });
      }
      add('Skipped', ...pins.flatMap((p) => (str(p.skip) === null ? [] : [skipChip(str(p.session_id) ?? '', str(p.skip) ?? '', str(p.session_id) ?? '')])));
      add('Planned destinations', ...[...new Set(pins.flatMap((p) => (str(p.planned) === null ? [] : [str(p.planned) ?? ''])))].map((id) => accountChip(id)));
      const outcome = str(d.outcome);
      const reason = str(d.reason);
      add('Outcome', outcome === null ? null : plain(outcome === 'nothing_to_mark' ? `nothing to mark${reason === null ? '' : `: ${words(reason)}`}` : words(outcome)));
      add('Next round after', time(d.dwell_until));
      add('Dwell', dwell === null ? null : plain(span(dwell * 1_000)));
      if (typeof d.persisted === 'boolean') add('Survive a restart', plain(d.persisted ? 'persisted' : 'not persisted'));
      break;
    }
    case 'mark_cleared': {
      const reason = str(d.reason) ?? '';
      const marked = num(d.marked_at);
      const projection = num(d.projection);
      const markedWindow = str(d.marked_window);
      pin();
      // A mark heals when the account's projection reads under the ceiling again on the window that fired.
      add('Reason', plain(reason === 'healed' && projection !== null ? `healed: the ${markedWindow === null ? 'window' : wordOf(WINDOW_WORD, markedWindow)} projection read ${fmtPct(projection)} under the ceiling` : words(reason)));
      add('Marked', time(marked), str(d.marked_on) === null ? null : accountChip(str(d.marked_on) ?? '', 'on'));
      add('Standing for', marked === null ? null : plain(span(atOf(row) - marked)));
      break;
    }
    case 'reservation_expired': {
      const age = num(d.age_ms);
      add('Request', request);
      add('Attempt', count(d.attempt));
      add('Cause', text(d.cause));
      add('Reserved', ...shareChips(rec(d.shares)));
      add('Held for', age === null ? null : plain(span(age)));
      add('Effect', plain("the booked share left the account's prediction"));
      break;
    }
    case 'warm_prefix_expired': {
      const deepest = rec(d.deepest);
      const tokens = num(deepest.prefix_tokens);
      const ttl = num(deepest.ttl_ms);
      add('Boundary', text(deepest.tier));
      add('Depth', count(deepest.depth));
      add('Size', tokens === null ? null : plain(fmtCompact(tokens)));
      add('Tier', ttl === null ? null : plain(ttlWord(ttl)));
      add('Written', time(deepest.written_at));
      add('Entries', count(d.entries));
      add('Listed', plain('when the kind is chosen'));
      break;
    }
    case 'wind_down_started': {
      const minutes = num(d.wind_down_minutes);
      add('Subscription ends', time(d.ends_at));
      add('Wind-down', minutes === null ? null : plain(span(minutes * MINUTE)));
      add('Effect', plain('no fresh session starts on this account'));
      add('Cold pins', plain('drain to a sibling'));
      add('End from', text(d.end_source));
      break;
    }
    case 'window_reset':
      add('Window', window === null ? null : plain(wordOf(WINDOW_WORD, window)));
      add('Before the reset', pct(d.utilization_before));
      add('Prediction', plain('0% until the next head'));
      add('Next reset', time(d.next_reset));
      break;
    case 'window_spent':
      add('Window', window === null ? null : plain(wordOf(WINDOW_WORD, window)));
      add('Reading', pct(d.utilization));
      add('Read from', text(d.source, SOURCE_WORD));
      add('Resets', time(d.reset));
      add('Effect', plain('a pinned session moves by the spent rule at its next request or waits in place for a reset within the retry bound'));
      add('Request', request);
      break;
  }
  return pairs;
}

/** A row of a fold or the outcome: one value, one entity, or a row of chips. */
export interface CardRow {
  chips: Chip[];
  label: string;
  title?: string;
}

/** The Request facts fold: the request as the engine read it, each entity a link, the lane and flags as chips. */
export function requestRows(record: DecisionRecord, row: DecisionSummary): CardRow[] {
  const r = record.request;
  const counted = row.attemptCount !== null && r.attempt <= row.attemptCount;
  const flags = Object.entries({ compacted: r.flags.contextCompacted, count: r.flags.count, fast: r.flags.fast, marker: r.flags.marker, portable: r.flags.portable, probe: r.flags.probe })
    .filter(([, on]) => on)
    .map(([name]) => plain(name));
  const upstream = record.outcome.upstreamModel;
  return [
    { chips: [plain(counted ? `${r.attempt} of ${row.attemptCount}` : String(r.attempt))], label: 'Attempt' },
    { chips: [plain(fmtTime(r.at))], label: 'At' },
    { chips: [{ entity: { id: r.keyId, kind: 'key' }, mono: true, value: r.keyId }], label: 'Key' },
    { chips: [{ entity: { id: r.poolId, kind: 'pool' }, value: r.poolId }], label: 'Pool' },
    { chips: [plain(r.agentId)], label: 'Agent' },
    { chips: [plain(r.requestClass ?? words(r.clientClass))], label: 'Class' },
    { chips: [modelChip(r.canonicalModel)], label: 'Model resolved' },
    ...(upstream !== null && upstream !== r.canonicalModel ? [{ chips: [modelChip(upstream)], label: 'Upstream model' }] : []),
    { chips: [plain(r.lane.speed, 'speed'), plain(r.lane.thinking, 'thinking'), plain(r.lane.effort === '-' ? 'none' : r.lane.effort, 'effort')], label: 'Lane' },
    { chips: flags.length > 0 ? flags : [plain('none')], label: 'Flags' },
    ...(r.attempted.length > 0 ? [{ chips: r.attempted.map((id) => accountChip(id)), label: 'Attempted' }] : []),
  ];
}

/** The Pin and warmth fold. A record that pins nothing reads one row, Pin, with why. */
export function pinRows(record: DecisionRecord, now: number): CardRow[] {
  const p = record.pin;
  if (p === null) return [{ chips: [plain(record.pinReason === null ? 'none' : PIN_REASON_LABEL[record.pinReason])], label: 'Pin' }];
  const rows: CardRow[] = [];
  const released = p.released.length > 0 ? [{ chips: p.released.map((r) => plain(releaseLabel(r))), label: 'Released' }] : [];
  if (p.accountId === null) return [{ chips: [plain(p.found === 'none' ? 'none found' : words(p.found))], label: 'Pin' }, ...released];
  rows.push({ chips: [accountChip(p.accountId)], label: 'Pin on' });
  if (p.found !== 'live') rows.push({ chips: [plain(words(p.found))], label: 'Found' });
  if (p.epoch !== null) rows.push({ chips: [plain(fmtInt(p.epoch))], label: 'Epoch' });
  if (p.createdAt !== null) rows.push({ chips: [plain(formatFact(p.createdAt, 'time', now))], label: 'Created' });
  if (p.idleMs !== null) rows.push({ chips: [plain(span(p.idleMs))], label: 'Idle' });
  if (p.refreshAgeMs !== null) rows.push({ chips: [plain(span(p.refreshAgeMs))], label: 'Refresh age' });
  if (p.lane !== null) rows.push({ chips: laneChips(p.lane), label: 'Pin lane' });
  if (p.warmth !== null) rows.push({ chips: [plain(wordOf(WARMTH_WORD, p.warmth))], label: 'Warmth' });
  if (p.warmthCause !== null) rows.push({ chips: [plain(wordOf(WARMTH_CAUSE_WORD, p.warmthCause))], label: 'Cause' });
  if (p.marked || p.shed !== null) {
    // A version 1 record carried the mark alone, with no account, instant or window.
    const trigger = p.markedWindow === null ? [] : [plain(`by the ${wordOf(WINDOW_WORD, p.markedWindow)} trigger`)];
    const marked = p.markedOn !== null && p.markedAt !== null ? [accountChip(p.markedOn, 'marked on'), plain(formatFact(p.markedAt, 'time', now), 'at'), ...trigger] : [plain('marked by a round')];
    rows.push({ chips: [...(p.marked ? marked : []), ...(p.shed !== null ? [accountChip(p.shed.from, 'shed from'), plain(formatFact(p.shed.at, 'time', now), 'at')] : [])], label: 'Shed mark' });
  }
  return [...rows, ...released];
}

/**
 * The Settings in force grid. A
 * version 1 record reads the switch and the ceiling as not recorded and its three removed settings after the list.
 */
export function settingsRows(record: DecisionRecord): CardRow[] | null {
  const s = record.settings;
  if (s === null) return null;
  const rows = SETTINGS_ROWS.map((r) => ({ chips: [plain(UNRECORDED.includes(r.key) && s[r.key] === null ? 'not recorded' : settingValue(r.key, s))], label: r.label }));
  const share = (v: number | null) => (v === null ? 'off' : fmtPct(v));
  const legacy = s.legacy === null ? [] : [
    { chips: [plain(share(s.legacy.shedAheadOfPace))], label: 'Shed above (then)' },
    { chips: [plain(share(s.legacy.selectBelowUtilization))], label: 'Select below (then)' },
    { chips: [plain(fmtPct(s.legacy.paceFloor))], label: 'Pace floor (then)' },
  ];
  return [...rows, ...legacy];
}
