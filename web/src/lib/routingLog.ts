import { iso, list, numOrNull as num, oneOf, strings, strOrNull as str } from '@/api/decode';
import { FACT_UNIT_VALUES, RESET_OUTCOME_VALUES } from '@/api/types';
import type { CohortHold, DecisionKind, DecisionRecord, DecisionSummary, FactEntity, FactUnit, ModelTiers, PinReason, Placement, PrefixEntry, RecordCandidate, RecordCompletion, RecordOutcome, RecordSettings, Refusal, ResetOutcome, RoutingEvent, RoutingLogRow, WalkFact, WalkStep } from '@/api/types';
import { accountHref, type LogFilters, requestHref, ROUTING_LOG_CODEC, type RangeValue, SESSIONS_CODEC } from './address';
import { FAILOVER_MOVE_REASONS, moveLabel, releaseLabel, RULE_MOVE_REASONS, type WeekReading } from './analytics';
import { tierOrder } from './costs';
import { has, lookup } from './counts';
import type { ChipText } from './filterBar';
import { fmtAbsolute, fmtCompact, fmtInt, fmtPct, fmtShare, fmtTime, fmtUsd, instant, listed, plural, shortModel, weekShort } from './format';
import { accountBound, transformationLabel, transformationOrder } from './inputTransformations';
import { ALL_KINDS, DECISION_KIND_LABEL, EVENT_KIND_LABEL } from './kinds';
import { isFastScope, MODEL_SCOPE_PREFIX, PLAN_WORD_LABEL, planLabel, POLL_OUTCOME_LABEL, scopeLabel, scopeModel, shareLeft } from './quota';
import { INELIGIBLE_LABEL, LAST_RESORT_LABEL, speedWord } from './selection';
import { ageWord, HOUR, MINUTE, span } from './time';
import { knownWindowsFirst, storedFableKey, WINDOW_KEYS, WINDOW_LABEL, windowEntry, windowLabelOf, windowTag, windowTagOf } from './windows';

export { ageWord, ALL_KINDS, DECISION_KIND_LABEL, EVENT_KIND_LABEL };

// The routing log's words. The routes answer structured values and no English, so a label change touches this module
// alone and never a route or a fixture.

const LABELS = {
  account_bound: 'account-bound',
  answer: 'answer',
  attempt: 'attempt',
  capture: 'capture',
  cause: 'cause',
  client: 'client',
  depth: 'depth',
  epoch: 'epoch',
  fable_model: 'Fable model',
  fable_reservation: 'Fable reservation',
  factor: 'speed',
  factor_min: 'slowest',
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
  rewrite: 'rewrite in flight',
  runway: 'runway',
  secret: 'secret',
  seven_day: '7d',
  seven_day_fable: 'Fable',
  slow_down: 'slow-down',
  spent: 'spent',
  state: 'state',
  to: 'to',
  turn: 'turn',
  until: 'until',
  was: 'was',
} as const;
type FactKey = keyof typeof LABELS;
export type FactLabel = (typeof LABELS)[FactKey];

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

/**
 * The Pin fold's row for a record that pins nothing: 'classifier call without a pin', which heads the rows of the turn
 * the call judged, 'not looked up' (a refusal's) or 'pinning off'.
 */
const PIN_REASON_LABEL: Record<PinReason, string> = {
  classifier: 'classifier call without a pin',
  not_looked_up: 'not looked up',
  pinning_off: 'pinning off',
};

/** Where a classifier call went, in the Pin fold's words. A word the console lacks reads as sent. */
const DIRECTION_WORDS: Record<string, string> = {
  pin_account: 'the account its pin stands on',
  strategy: 'placed by the strategy',
  turn_account: "the judged turn's account",
};

export type VerdictTone = 'critical' | 'good' | 'neutral' | 'serious' | 'warning';

export const DECISION_KIND_TONE: Record<DecisionKind, VerdictTone> = {
  exhausted: 'critical',
  held: 'warning',
  moved: 'warning',
  placed: 'good',
  refused: 'critical',
  renewed: 'good',
};

/** A named set of kinds, which the Kinds list checks or unchecks in one click. */
export interface KindPreset {
  label: string;
  members: readonly string[];
}

/** The eight groups of related kinds, in the order a reader follows a request. Every kind belongs to exactly one, and a preset is exactly its members. */
export const KIND_PRESETS: readonly KindPreset[] = [
  { label: 'placements and pins', members: ['placed', 'renewed', 'pin_created', 'pin_migrated', 'pin_released', 'pin_expired', 'reservation_expired'] },
  { label: 'moves and holds', members: ['moved', 'held', 'shedding_round', 'mark_cleared', 'wind_down_started', 'slow_down_started', 'slow_down_ended'] },
  { label: 'exhaustion and refusals', members: ['exhausted', 'refused', 'window_spent', 'budget_reached', 'stream_interrupted'] },
  { label: 'account state and windows', members: ['account_state_changed', 'window_reset', 'window_cleared', 'window_dropped', 'limit_reset_claimed', 'overage_started', 'overage_ended', 'budget_period_ended', 'fast_credits_changed', 'refresh_failed', 'poll_failed', 'plan_ended', 'plan_restored', 'refresh_succeeded', 'refresh_uncertain'] },
  { label: 'cooldowns and backoff', members: ['cooldown_set', 'cooldown_cleared', 'poll_backoff_started', 'poll_backoff_ended'] },
  { label: 'keepalive', members: ['probe', 'warm_prefix_expired'] },
  { label: 'registry changes', members: ['registry_changed', 'catalog_changed', 'model_permitted', 'model_rejected'] },
  { label: 'router life cycle', members: ['router_started', 'ledger_gap'] },
];

/** A kind's word: "probe decisions" for the pseudo-kind, else its label. */
export const kindWord = (kind: string): string => (kind === 'probe' ? 'probe decisions' : (lookup(DECISION_KIND_LABEL, kind) ?? lookup(EVENT_KIND_LABEL, kind) ?? kind));

/**
 * The Kinds chip: one whole preset by its name, a union of whole presets and nothing else by their names in
 * KIND_PRESETS' order, else the kinds' words three then " +N", every word in the title. An empty set draws no chip
 * (empty text).
 */
export function kindsChipText(kinds: readonly string[]): ChipText {
  const set = new Set(kinds);
  if (set.size === 0) return { text: '', title: '', values: '' };
  const words = kinds.map(kindWord);
  // The presets share no kind, so whole presets that hold as many kinds as the set are the set.
  const whole = KIND_PRESETS.filter((p) => p.members.every((m) => set.has(m)));
  const values =
    whole.length > 0 && whole.reduce((n, p) => n + p.members.length, 0) === set.size
      ? whole.map((p) => p.label).join(', ')
      : listed(words);
  return { text: `kinds: ${values}`, title: words.join(', '), values };
}

const EVENT_TONE: Record<string, 'critical' | 'serious' | 'warning'> = {
  account_state_changed: 'serious',
  budget_reached: 'serious',
  cooldown_set: 'warning',
  ledger_gap: 'serious',
  model_rejected: 'warning',
  slow_down_started: 'warning',
  window_spent: 'warning',
};

/** An event reads neutral unless it reports trouble. A state change reads by the state it reached, so an account's
 *  return to ready reports none. */
export function eventTone(row: RoutingEvent): 'critical' | 'neutral' | 'serious' | 'warning' {
  if (row.kind === 'account_state_changed' && str(row.detail.to) === 'ready') return 'neutral';
  return lookup(EVENT_TONE, row.kind) ?? 'neutral';
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
const NEUTRAL = new Set(['found', 'no', 'no_runway', 'none', 'not_run', 'off', 'off_window']);

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
export const wordOf = (map: Record<string, string>, name: string) => lookup(map, name) ?? words(name);
/** A window's tag, or the key's words where the console does not know the window. */
const windowTagWord = (key: string): string => windowTagOf(key) ?? words(key);
/** The window whose trigger set a shedding mark, as the Pin fold and the Sessions view word it. */
export const triggerPhrase = (window: string): string => `by the ${windowTagWord(window)} trigger`;
/** A no-return stamp's origin. A failover's landing stamps the pin as a round's move does, so the reason picks it. */
export const shedOrigin = (reason: string): string => (reason === 'round' ? 'shed from' : 'failed over from');

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
const OBSTACLE_WORD: Record<string, string> = {
  attempted: 'already tried',
  budget_unknown: INELIGIBLE_LABEL.budget_unknown,
  cooldown: INELIGIBLE_LABEL.cooldown,
  credits: 'credits unavailable',
  no_pin: 'no pin',
  operator: INELIGIBLE_LABEL.unavailable,
  overage: INELIGIBLE_LABEL.overage,
  pin_moved: 'pin moved',
  spent: INELIGIBLE_LABEL.spent,
  subscription_ended: INELIGIBLE_LABEL.subscription_ended,
};
/** The hover of a pinned obstacle on the By chip, for an obstacle whose word does not say why the probe stayed. */
const PINNED_OBSTACLE_HINT: Record<string, string> = { pin_moved: "The pin stands on another account than the recipe's, so the probe was not sent." };
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
const PACE_HINT = "Pace: the account with the most of its window left per hour until the window resets or the subscription ends, less the burn of the sessions already on it, wins. A request reads the model's own window when the model has one, else the 7-day window, and a window the account has not read counts as untouched. A request of a model without a week of its own first sets aside the room the account's expected Fable use needs.";

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
      if (x.window === 'budget' || x.window === 'overage') return 'budget';
      return x.window === SPEND_WINDOW ? 'spend cap' : 'cooldown';
    case 'untimed':
      return 'no deadline';
    default:
      return words(x.kind);
  }
}

function decisionBy(row: DecisionSummary): Chip {
  const { kind, value } = row.by;
  // A renewal after a slow-down hold or a wait for a rewrite in flight names the hold, since the wait explains the row
  // more than the renewal does.
  if (row.kind === 'renewed' && !(kind === 'hold' && (value === 'slow_down' || value === 'rewrite'))) return { value: row.probe ? 'keepalive' : 'no rule fired' };
  switch (kind) {
    case 'exhaustion': {
      const x = row.exhaustion;
      const title = x?.kind === 'pinned' && x.obstacle !== null ? lookup(PINNED_OBSTACLE_HINT, x.obstacle) : undefined;
      return title === undefined ? { value: exhaustionWord(row) } : { title, value: exhaustionWord(row) };
    }
    case 'hold':
      return value === 'strategy_pick' ? strategyChip(row.strategy, row.tier) : { value: value === 'slow_down' ? LABELS.slow_down : value === 'rewrite' ? LABELS.rewrite : words(value) };
    case 'none':
      return { value: 'no rule fired' };
    case 'refusal':
      if (value === 'client') return { value: row.refusal?.code === 'contract' ? 'client contract' : 'client drift' };
      if (value === 'guardrail') return { value: 'guardrail blocked' };
      return { value: (row.refusal ? lookup(KEY_REFUSAL_WORD, row.refusal.code) : undefined) ?? `${words(value)} refused` };
    case 'rule':
      return { value: `${words(value)} rule` };
    case 'rung':
      return { value: wordOf(RUNG_WORD, value) };
    case 'strategy':
      return strategyChip(value, row.by.tier ?? row.tier);
  }
}

const rec = (v: unknown): Record<string, unknown> => (v !== null && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : {});

/** The account a moved pin left, null on any row but a pin's move. */
export const movedFrom = (row: RoutingLogRow): string | null => (row.type === 'event' && row.kind === 'pin_migrated' ? str(row.detail.from_account) : null);

/**
 * The account or the key a registry change edited. The ledger's row names a pool's change in its pool column and
 * these two in the change's target alone.
 */
export const registryTarget = (row: RoutingLogRow, subject: 'account' | 'key'): string | null =>
  row.type === 'event' && row.kind === 'registry_changed' && (str(row.detail.kind) ?? '').startsWith(`${subject}_`) ? str(row.detail.target) : null;

/** The one model a catalog change names, null for a change of several and on any other row. */
export const catalogModel = (row: RoutingLogRow): string | null => {
  if (row.type !== 'event' || row.kind !== 'catalog_changed') return null;
  const named = [...list(row.detail.added), ...list(row.detail.removed)];
  return named.length === 1 ? str(named[0]) : null;
};

/**
 * The window the router names a usage-based seat's spend cap by, in a timed exhaustion and a spent obstacle alike. It
 * stays out of `WINDOW_TAG`, whose entries are the plan windows the decision strip draws a column for.
 */
const SPEND_WINDOW = 'spend';
const STATE_WORD: Record<string, string> = { funding_required: 'funding required', reauth_required: 're-auth required' };
/** The registry's word for who moved an account's state. */
const CAUSE_WORD: Record<string, string> = { auth_failed: 'credential refused', funding_refused: 'credit balance', funding_retry: 'funding retried', operator: 'by hand', refresh_failed: 'refresh failed' };
const CLEARED_WORD: Record<string, string> = { admin: 'by hand', expired: 'deadline passed' };
const CREDITS_WORD: Record<string, string> = { org_spend_cap_reached: 'spend cap reached' };
const PERIOD_WORD: Record<string, string> = { day: 'daily budget', month: 'monthly budget' };
const REGISTRY_VERB: Record<string, string> = { updated: 'edited' };
/** The By chip's words for a claim's outcome, which the panel's Outcome pair repeats. */
const RESET_OUTCOME_WORD: Record<ResetOutcome, string> = { refused: 'reset refused', unsettled: 'reset not confirmed', used: 'reset used' };
/** A class this console does not know is open, so it reads as not confirmed. A row with no outcome reads null. */
const resetOutcome = (v: unknown): string | null => {
  const outcome = str(v);
  return outcome === null ? null : RESET_OUTCOME_WORD[oneOf(RESET_OUTCOME_VALUES, outcome) ?? 'unsettled'];
};
/**
 * The pool fields a registry edit reads as a share rather than a bare number, a row written before the rename among
 * them, and the one ceiling a row written before the split into a ceiling per kind of window names.
 */
const SHARE_FIELDS = new Set(['five_hour_ceiling', 'pace_floor', 'select_below_utilization', 'shed_ahead_of_pace', 'shed_cap', 'shed_share', 'weekly_ceiling', 'window_ceiling']);
/**
 * The fields whose label is not their words: a row written before the rename keeps the old field names, so a renamed
 * field reads its new word and a removed one says so, and a pool's keepalive policy member, written under its dotted
 * name, reads its label.
 */
const FIELD_LABEL: Record<string, string> = {
  burn_window_seconds: 'burn average seconds (then burn_window_seconds)',
  'keepalive.enabled': 'keepalive',
  'keepalive.max_idle_seconds': 'keepalive idle window seconds',
  'keepalive.max_parked_seconds': 'keepalive parked seconds',
  'keepalive.scheduled_horizon_seconds': 'keepalive scheduled horizon seconds',
  pace_floor: 'pace floor (removed)',
  select_below_utilization: 'select below utilization (removed)',
  shed_ahead_of_pace: 'shed ahead of pace (removed)',
  shed_share: 'shed cap (then shed_share)',
  source_dwell_seconds: 'shed dwell seconds (then source_dwell_seconds)',
};
const fieldLabel = (field: string) => wordOf(FIELD_LABEL, field);

const registryBy = (kind: string) => {
  const at = kind.indexOf('_');
  return at < 0 ? words(kind) : `${kind.slice(0, at)} ${wordOf(REGISTRY_VERB, kind.slice(at + 1))}`;
};

/** The registry fields that hold an instant: a key's expiry and an account's subscription end. */
const TIME_FIELDS = new Set(['expires_at', 'subscription_ends_at']);

const fieldValue = (field: string, value: unknown, now: number): string => {
  if (value === null || value === undefined) return 'none';
  // A list, such as the guardrails a pool or a key runs, reads as the console's lists do elsewhere.
  if (Array.isArray(value)) return value.length === 0 ? 'none' : value.map((item) => fieldValue(field, item, now)).join(', ');
  if (typeof value === 'boolean') return value ? 'on' : 'off';
  if (typeof value === 'number' && SHARE_FIELDS.has(field)) return fmtPct(value);
  if (typeof value === 'number' && TIME_FIELDS.has(field)) return formatFact(value, 'time', now);
  return typeof value === 'object' ? JSON.stringify(value) : String(value);
};

/** A registry change's field entries without the flag a staged secret sets, and whether it stands. */
const registryDiff = (diff: unknown): { entries: [string, unknown][]; secret: boolean } => {
  const { secret_replaced: secret, ...rest } = rec(diff);
  return { entries: Object.entries(rest), secret: secret === true };
};

/**
 * A cooldown's By words. The relay files a Fable model's 429 on its own week and an API key's retry-after under the
 * model's scope, so a 429 reads by its window whatever its scope. Under a model's scope a rejection names its 404 and
 * a permission error its 403, and any other reason reads its words as under an account's scope.
 */
function cooldownBy(e: RoutingEvent): string {
  const d = e.detail;
  const scope = str(d.scope) ?? '';
  if (scope === 'fast') return 'fast lane 429';
  const reason = str(d.reason) ?? '';
  const window = str(d.window) ?? '';
  if (reason === 'rate_limit') return rateLimitWords(window) ?? (window === 'retry_after' ? '429 retry-after' : '429');
  const model = scopeModel(scope);
  if (model !== null && reason === 'model_rejected') return `404 on ${shortModel(model)}`;
  if (model !== null && reason === 'permission') return `403 on ${shortModel(model)}`;
  return words(reason);
}

/** The decisions of one rows array that a pin's By and a failover's answer read. */
interface PageIndex {
  /** Each request's decisions that carry a status, the highest seq first and page order among equal ones. */
  answered: Map<string, DecisionSummary[]>;
  /** Every decision of a slot, in page order. */
  decisions: Map<string, DecisionSummary[]>;
  /** The first placed decision of a slot in page order. */
  placed: Map<string, DecisionSummary>;
}

/**
 * A slot is one session, account and second. A row whose `at` does not parse has none, as the second it would land in
 * matches no other.
 */
const slotOf = (sessionId: string | null, accountId: string | null, at: number): string | null => (Number.isNaN(at) ? null : JSON.stringify([sessionId, accountId, Math.floor(at / 1_000)]));

// Every render asks each pin and failover row of the log for its match, and the rows array keeps its identity until a
// poll or a read replaces it, so each array is indexed once in place of a scan of the whole list per row.
const pageIndexes = new WeakMap<readonly RoutingLogRow[], PageIndex>();

function pageIndexOf(page: readonly RoutingLogRow[]): PageIndex {
  const known = pageIndexes.get(page);
  if (known !== undefined) return known;
  const index: PageIndex = { answered: new Map(), decisions: new Map(), placed: new Map() };
  const add = <T>(map: Map<string, T[]>, key: string, value: T) => {
    const list = map.get(key);
    if (list === undefined) map.set(key, [value]);
    else list.push(value);
  };
  for (const r of page) {
    if (r.type !== 'decision') continue;
    if (r.status !== null) add(index.answered, r.requestId, r);
    const slot = slotOf(r.sessionId, r.accountId, atOf(r));
    if (slot === null) continue;
    add(index.decisions, slot, r);
    if (r.kind === 'placed' && !index.placed.has(slot)) index.placed.set(slot, r);
  }
  for (const list of index.answered.values()) list.sort((a, b) => b.seq - a.seq);
  pageIndexes.set(page, index);
  return index;
}

/** A move the state of the pin's account forced: its event carries the replaced pin's whole list of reasons. */
const causedMove = (d: Record<string, unknown>): boolean => Array.isArray(d.reasons);

// A cause shares `cooldown`, `overage` and `spent` with a rule or a failover, so the list alone tells them apart.
const movedLabel = (d: Record<string, unknown>): string => {
  const reason = str(d.reason) ?? '';
  return causedMove(d) ? releaseLabel(reason) : moveLabel(reason);
};

const reasonChips = (d: Record<string, unknown>): Chip[] => list(d.reasons).map((r) => plain(releaseLabel(String(r))));

/** A failed poll's answer, for its By chip and its panel's Status. A transport failure answered no status, so the
 *  words say what failed in its place. */
const pollStatus = (d: Record<string, unknown>): string | null => {
  if (d.transport === true) return 'transport failure';
  const status = num(d.status);
  return status === null ? null : String(status);
};

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
    // The router names the class, so the chip never reads the upstream's word for it.
    case 'limit_reset_claimed':
      return { value: resetOutcome(d.outcome) ?? words(e.kind) };
    case 'mark_cleared':
      return { value: words(str(d.reason) ?? e.kind) };
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
      const slot = slotOf(e.sessionId, e.accountId, atOf(e));
      const placement = slot === null ? undefined : pageIndexOf(page).placed.get(slot);
      return placement ? decisionBy(placement) : { value: 'placement' };
    }
    case 'pin_expired': {
      const first = str(list(d.reasons)[0]);
      return { value: first === null ? 'expired' : first === 'expired' ? 'idle' : releaseLabel(first) };
    }
    case 'pin_migrated':
      return { value: movedLabel(d) };
    case 'pin_released':
      return { value: 'by hand' };
    // A row written before the cause was a clean poll's.
    case 'poll_backoff_ended':
      return { value: str(d.cause) === 'plan_ended' ? 'plan ended' : 'clean poll' };
    // The usage service opens a backoff on a rate limit alone, so the kind decides the word.
    case 'poll_backoff_started':
      return { value: POLL_OUTCOME_LABEL.rate_limited };
    case 'poll_failed':
      return { value: pollStatus(d) ?? words(e.kind) };
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
      return { value: reading === null ? `on ${windowTagWord(window)}` : `${fmtPct(reading)} on ${windowTagWord(window)}` };
    }
    case 'slow_down_ended':
    case 'slow_down_started':
      return { value: LABELS.slow_down };
    // The Kind column already names the interruption, so a cut's code drops its stream stem to fit the By column.
    case 'stream_interrupted': {
      const code = str(d.code);
      return { value: code === null ? words(e.kind) : words(code.replace(/^stream_/, '')) };
    }
    case 'warm_prefix_expired': {
      const ttl = num(rec(d.deepest).ttl_ms);
      return { value: ttl === null ? 'expired' : `${span(ttl)} tier` };
    }
    case 'wind_down_started':
      return { value: 'subscription ends' };
    // The sentence runs past the By column, so the chip draws its first words and its title carries it whole.
    case 'window_cleared':
    case 'window_dropped': {
      const value = `${windowTagWord(str(d.window) ?? '')} ${e.kind === 'window_cleared' ? 'cleared' : 'dropped'}`;
      const reading = num(d.utilization);
      return reading === null ? { value } : { title: `${value} to ${fmtPct(reading)} under its reset`, value };
    }
    case 'window_reset':
      return { value: `${windowTagWord(str(d.window) ?? '')} reset` };
    // A row that names no source draws none, since any word would name a source the row does not hold.
    case 'window_spent': {
      const source = str(d.source);
      const window = windowTagWord(str(d.window) ?? '');
      return { value: source === null ? `on ${window}` : `${lookup(SOURCE_CHIP_WORD, source) ?? source} on ${window}` };
    }
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
const FAILOVER_MOVES = new Set<string>(FAILOVER_MOVE_REASONS);

/**
 * The status a failover's failed attempt answered. The move lands in the second of the decision that placed the next
 * attempt, under the same session and account. The route joins each decision to the attempt it dispatched, so the
 * request's decision before that one carries the failed attempt's status.
 */
function failoverAnswer(e: RoutingEvent, page: readonly RoutingLogRow[]): number | null {
  if (e.sessionId === null) return null;
  const slot = slotOf(e.sessionId, e.accountId, atOf(e));
  if (slot === null) return null;
  const { answered, decisions } = pageIndexOf(page);
  for (const next of decisions.get(slot) ?? []) {
    const failed = answered.get(next.requestId)?.find((r) => r.seq < next.seq);
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
  const fact = (key: FactKey, v: unknown, unit: FactUnit) => {
    const n = num(v);
    return n === null ? [] : [pair(key, n, unit, now)];
  };
  const count = (key: FactKey, v: unknown) => fact(key, v, 'count');
  switch (e.kind) {
    case 'account_state_changed':
      return [pair('state', wordOf(STATE_WORD, str(d.to) ?? 'unknown'), 'text', now)];
    case 'budget_period_ended':
      return fact('spent', d.previous_spent_usd, 'usd');
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
      if (causedMove(d) || !FAILOVER_MOVES.has(str(d.reason) ?? '')) return count('epoch', d.epoch);
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
      // Only a diff whose every entry is a change names its fields: a creation's snapshot names none.
      const { entries: diff, secret } = registryDiff(d.diff);
      if (diff.length === 0) return secret ? [pair('secret', 'replaced', 'text', now)] : [];
      if (!diff.every(([, change]) => isChange(change))) return [];
      if (diff.length > 1) return [pair('fields', diff.length, 'count', now)];
      const [field, change] = diff[0];
      return [pair('field', fieldLabel(field), 'text', now), pair('to', fieldValue(field, rec(change).to, now), 'text', now)];
    }
    case 'router_started':
      return count('pins', d.pins_loaded);
    case 'shedding_round':
      return [pair('pins', `${list(d.marks).length} of ${list(d.pins).length}`, 'text', now)];
    case 'slow_down_ended':
    case 'slow_down_started': {
      const key = e.kind === 'slow_down_started' ? 'factor' : 'factor_min';
      const factor = num(d[key]);
      return factor === null ? [] : [pair(key, speedWord(factor), 'text', now)];
    }
    case 'warm_prefix_expired': {
      const deepest = rec(d.deepest);
      return [...fact('prefix', deepest.prefix_tokens, 'tokens'), ...fact('depth', deepest.depth, 'count')];
    }
    case 'wind_down_started':
      return time('until', d.ends_at);
    case 'window_reset':
      return fact('was', d.utilization_before, 'percent');
    case 'window_spent':
      return time('resets', d.reset);
    default:
      return [];
  }
}

/** A speed, the factor or the one the holds realize, reads as the speed chips print it wherever it stands. */
const SPEED_KEYS = new Set(['factor', 'factor_min', 'speed']);

function decisionFacts(row: DecisionSummary, now: number): FactPair[] {
  const value = (f: DecisionSummary['fact'][number]) => {
    if (SPEED_KEYS.has(f.key) && typeof f.value === 'number') return speedWord(f.value);
    // A cause is the engine's word, which reads as the card words it. An id under any other key keeps its underscores.
    if (f.key === 'cause' && typeof f.value === 'string') return wordOf(WARMTH_CAUSE_WORD, f.value);
    // A bound landing's pick reads as the walk's pick step words it.
    if (f.key === 'pick' && f.value === 'account_bound') return LABELS.account_bound;
    return formatFact(f.value, f.unit, now);
  };
  // A key the console has no label for is dropped rather than drawn under a guessed one.
  return row.fact.flatMap((f) => {
    const label = lookup(LABELS, f.key);
    return label !== undefined && !repeatsAt(row, f.unit, f.value) ? [{ entity: f.entity, label, value: value(f) }] : [];
  });
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
      return accountHref(id);
    case 'request':
      return requestHref(id);
    // The Keys and Pools pages find and focus their entity by the address's hash.
    case 'key':
    case 'pool':
      return `/${kind}s#${encodeURIComponent(id)}`;
    // A model has no view of its own, so its link opens the log on the model's rows over the last day.
    case 'model':
      return logHref({ models: [id], range: '24h' });
    // Finished sessions is on so a finished session's row is not hidden behind its own link.
    case 'session':
      return `/sessions?${SESSIONS_CODEC.toParams({ ...SESSIONS_CODEC.defaults, finished: true, session: id })}`;
  }
}

/** A fixed window as two instants, or a preset ending now by name. */
export type LogWindow = { from: number; to: number } | { range: string };

export type LogLink = Partial<Pick<LogFilters, 'accounts' | 'agentId' | 'kinds' | 'models' | 'pools' | 'request' | 'session'>> & LogWindow;

export const identityOf = (row: RoutingLogRow) => (row.type === 'decision' ? `${row.requestId}/${row.seq}` : `event/${row.id}`);

/** The fields of a row the route's order reads, which a cursor names, so a decoded cursor compares as its row does. */
type OrderFields = Pick<DecisionSummary, 'at' | 'requestId' | 'seq' | 'type'> | Pick<RoutingEvent, 'at' | 'id' | 'type'>;

/** The route's order: newest first, a decision before an event at one instant. */
export function compareRows(a: OrderFields, b: OrderFields): number {
  const at = Date.parse(b.at) - Date.parse(a.at);
  if (at !== 0) return at;
  if (a.type !== b.type) return a.type === 'decision' ? -1 : 1;
  if (a.type === 'decision' && b.type === 'decision') return a.requestId === b.requestId ? b.seq - a.seq : a.requestId < b.requestId ? 1 : -1;
  return (b as RoutingEvent).id - (a as RoutingEvent).id;
}

/** A window as an address's range: a preset by name, and a fixed window as two instants under the custom preset. */
export const rangeOfWindow = (link: LogWindow): RangeValue => ('range' in link ? { from: '', preset: link.range, to: '' } : { from: iso(link.from), preset: 'custom', to: iso(link.to) });

/** The log's address for `link` through its codec: a preset by name, nothing for the hour, and a window as two instants. */
export function logHref(link: LogLink): string {
  const range = rangeOfWindow(link);
  const d = ROUTING_LOG_CODEC.defaults;
  const search = ROUTING_LOG_CODEC.toParams({ ...d, accounts: link.accounts ?? d.accounts, agentId: link.agentId ?? d.agentId, kinds: link.kinds ?? d.kinds, models: link.models ?? d.models, pools: link.pools ?? d.pools, range, request: link.request ?? d.request, session: link.session ?? d.session }).toString();
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

// The decision card and the event detail: the record's walk, its candidates, its folds and its outcome, and an event's
// pairs. Every chip holds one value or one key-value pair, and every entity it names is a link.

const accountEntity = (id: string): FactEntity => ({ id, kind: 'account' });
export const accountChip = (id: string, label?: string): Chip => ({ entity: accountEntity(id), ...(label === undefined ? {} : { label }), value: id });
const modelChip = (id: string, label?: string): Chip => ({ entity: { id, kind: 'model' }, ...(label === undefined ? {} : { label }), mono: true, value: id });
const sessionChip = (id: string): Chip => ({ href: entityHref('session', id), mono: true, title: id, value: id.slice(0, 8) });
const requestChip = (id: string): Chip => ({ href: entityHref('request', id), mono: true, value: id });
export const plain = (value: string, label?: string): Chip => (label === undefined ? { value } : { label, value });

/** Why a member could not take the request, the hover of the candidate table's verdict chip. */
const OBSTACLE_HINT: Record<string, string> = {
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
const hintOf = (name: string) => lookup(OBSTACLE_HINT, name);

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

/** A completion's cache figure: the read over the promised prefix where the rung promised one, over the read, the
 *  write and the uncached input otherwise, with the three counts as the hover. A promise's hover is its rung's. */
const cacheFigure = (c: Pick<RecordCompletion, 'cacheReadTokens' | 'cacheWriteTokens' | 'inputTokens' | 'promisedTokens'>): { text: string; title: string | null } | null => {
  if (c.cacheReadTokens === null) return null;
  const read = fmtCompact(c.cacheReadTokens);
  if (c.promisedTokens !== null) return { text: `${read} of ${fmtCompact(c.promisedTokens)} promised`, title: null };
  const write = c.cacheWriteTokens ?? 0;
  const input = c.inputTokens ?? 0;
  return { text: `${read} of ${fmtCompact(c.cacheReadTokens + write + input)}`, title: `${read} read from the cache, ${fmtCompact(write)} written to it, ${fmtCompact(input)} uncached` };
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

/**
 * The walk's fact keys in the console's words, one for every key the router writes. A key it gains later reads with
 * its underscores as spaces until it has one here.
 */
export const WALK_KEY: Record<string, string> = {
  above_ceiling: 'above ceiling',
  age_five_hour: 'age 5h',
  ahead_five_hour: '5h ahead',
  ahead_seven_day: '7d ahead',
  at_cap: 'at cap',
  attempt: 'attempt',
  binding: 'binding',
  bound_ms: 'bound',
  burn: 'burn',
  burn_five_hour: 'burn 5h',
  burn_seven_day: 'burn 7d',
  burn_seven_day_fable: 'burn Fable',
  by_runway: 'by runway',
  cause: 'cause',
  ceiling: 'ceiling',
  cold: 'cold',
  cooldown: 'cooldown',
  credit: 'credit issuer',
  cycle_ms: 'cycle',
  deadline: 'deadline',
  deadline_five_hour: "5h to its subscription's end at",
  deadline_seven_day: "7d to its subscription's end at",
  demand_five_hour: 'demand',
  depth: 'depth',
  destinations: 'destinations',
  direction: 'sent to',
  drawn: 'drawn',
  dwell_age_ms: 'dwell age',
  dwell_ms: 'dwell',
  elapsed_five_hour: '5h elapsed',
  elapsed_seven_day: '7d elapsed',
  end: 'end',
  ending: 'ending',
  epoch: 'epoch',
  excess: 'excess',
  expires_at: 'expires at',
  fable_reservation: 'Fable reservation',
  factor: 'speed',
  fast: 'fast',
  fired_by: 'fired by',
  five_hour_ceiling: 'ceiling 5h',
  found: 'found',
  grace_ms: 'grace',
  hashes_stamped: 'hashes stamped',
  hold_five_hour: 'age hold 5h',
  holds_allowance: 'holds allowance',
  horizon_five_hour: 'horizon 5h',
  horizon_seven_day: 'horizon 7d',
  horizon_seven_day_fable: 'horizon Fable',
  idle_ms: 'idle',
  in_flight: 'in flight',
  in_overage: 'in overage',
  judged: 'judged account',
  judged_agent: 'judged turn',
  judged_model: 'judged model',
  kind: 'kind',
  ladder: 'ladder',
  lane: 'lane',
  last_shed_age_ms: 'last round',
  marked: 'marked',
  marks: 'marks',
  migrate_when_spent: 'move when spent',
  mode: 'mode',
  no_destination: 'no destination',
  no_fit: 'no fit',
  obstacle: 'obstacle',
  obstacles: 'obstacles',
  over_ms: 'over for',
  over_since: 'over since',
  over_since_fable_reservation: 'under since Fable reservation',
  over_since_five_hour: 'over since 5h',
  over_since_seven_day: 'over since 7d',
  over_since_seven_day_fable: 'over since Fable',
  overage_policy: 'overage policy',
  place: 'place',
  prefix_tokens: 'prefix',
  projection_five_hour: 'projection 5h',
  projection_seven_day: 'projection 7d',
  projection_seven_day_fable: 'projection Fable',
  ramp: 'ramp',
  reason: 'reason',
  refresh_age_ms: 'refresh age',
  refresh_stamped: 'refresh stamped',
  reserved_five_hour: 'reserved 5h',
  reserved_seven_day: 'reserved 7d',
  reserved_seven_day_fable: 'reserved Fable',
  reset: 'reset',
  reset_in_grace: 'reset in',
  rewrite_five_hour: 'rewrite 5h',
  rewrite_seven_day: 'rewrite 7d',
  rewrite_seven_day_fable: 'rewrite Fable',
  round: 'round',
  runway_ms: 'runway',
  skipped: 'skipped',
  slow_down: 'slow-down',
  slow_down_until: 'slowed until',
  speed: 'realized',
  spend: 'spend',
  spent: 'spent',
  strategy: 'strategy',
  stretch_ms: 'stretch',
  target: 'target',
  threshold_ms: 'threshold',
  tier: 'tier',
  ties: 'tied',
  tried: 'tried',
  trigger: 'trigger',
  turn: 'turn',
  verdict: 'verdict',
  wait_fable_reservation: 'wait Fable reservation',
  wait_five_hour: 'wait 5h',
  wait_ms: 'wait',
  wait_seven_day: 'wait 7d',
  wait_seven_day_fable: 'wait Fable',
  wall_five_hour: 'wall 5h',
  wall_seven_day: 'wall 7d',
  wall_seven_day_fable: 'wall Fable',
  warmth: 'warmth',
  weekly_ceiling: 'ceiling weekly',
  winding_down: 'winding down',
  window: 'window',
  window_share_fast: 'window share fast',
  worst_bucket: 'worst bucket',
};
export const keyWord = (key: string) => wordOf(WALK_KEY, key);
/** The keys whose value names itself, so a member's chip reads `primary eligible`, not `primary standing eligible`. */
const SELF_NAMED = new Set(['score', 'standing']);

/** `warmth_of`'s causes in words. */
const WARMTH_CAUSE_WORD: Record<string, string> = { chainless: 'no chain', class_auxiliary: 'auxiliary class', class_refreshes: 'refreshing class', continues_pin: 'continues the pin', count: 'token count' };
const WARMTH_WORD: Record<string, string> = { refresh: 'warm' };
/** Why the reservation's test fired no round, in the console's words. */
const RESERVATION_WORD: Record<string, string> = { no_other_model: 'no session of another model to move' };
const SNAKE = /^[a-z][a-z0-9]*(?:_[a-z0-9]+)+$/;
const unitOf = (unit: string | null): FactUnit | null => FACT_UNIT_VALUES.find((u) => u === unit) ?? null;

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
      return windowTagWord(value);
    case 'fired_by':
      return wordOf(LABELS, value);
    case 'fable_reservation':
      return wordOf(RESERVATION_WORD, value);
    case 'obstacle':
      return wordOf(OBSTACLE_WORD, value);
    case 'reason':
      // A bound landing's rungs and pick name the bind in the word a round's pin row reads for it.
      if (value === 'account_bound') return LABELS.account_bound;
      break;
  }
  return SNAKE.test(value) ? words(value) : value;
}

/** A rewrite or a booking is a small share of its window, so it reads to three figures where a whole percent would read 0%. */
const SHARE_KEY = /^(reserved|rewrite)_/;

export const valueText = (key: string, value: unknown, unit: string | null, now: number): string => {
  if (value === null || value === undefined) return 'none';
  if (typeof value === 'boolean') return value ? 'yes' : 'no';
  if (typeof value === 'number' && key === 'runway_ms' && value === 0) return 'none';
  if (typeof value === 'number' && SPEED_KEYS.has(key)) return speedWord(value);
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
/**
 * The kind tier's word: paid, overage, or the plan's own quota. An API key and a usage-based seat read paid in any
 * group, since a pool that does not prefer subscriptions ranks them beside the plans. A walk fact names no member, so
 * its group alone decides.
 */
const tierWord = (tier: readonly number[], member: Pick<RecordCandidate, 'kind' | 'planTier'> | null, plan: string) =>
  tier[0] === 2 || member?.kind === 'api_key' || member?.planTier === 'usage_based' ? 'paid' : tier[0] === 1 || tier[2] === 1 ? 'overage' : plan;

/** A lane's speed, thinking and effort chips, an effort of `-` read as none. */
const laneChipsOf = ({ effort, speed, thinking }: { effort: string; speed: string; thinking: string }): Chip[] => [plain(speed, 'speed'), plain(thinking, 'thinking'), plain(effort === '-' ? 'none' : effort, 'effort')];

/** A pin's lane is `model|speed|thinking|effort`. The model is the request's own, so the chips name the other three. */
export function laneChips(lane: string): Chip[] {
  const parts = lane.split('|');
  if (parts.length !== 4) return [plain(lane, 'lane')];
  const [, speed, thinking, effort] = parts;
  return laneChipsOf({ effort, speed, thinking });
}

/**
 * An exhausted line's obstacle, `account:name` as the engine weighs it, the account as the key. An operator obstacle
 * reads the member's own pause or state whatever obstacle the member's row carries. Any other obstacle reads the
 * member's row words and hover where the row's own obstacle carries the line's name, and otherwise the partition's
 * word or the obstacle's label: the selector names `spent` over a row with no obstacle where a plan window or a
 * model's week passed the member over, which is no spend cap.
 */
function obstacleChip(text: string, record: DecisionRecord, now: number): Chip {
  const at = text.lastIndexOf(':');
  const name = at < 0 ? text : text.slice(at + 1);
  const id = at < 0 ? '' : text.slice(0, at);
  const member = memberOf(record, id);
  const { title, value } =
    name === 'operator' && member !== undefined
      ? operatorWords(member)
      : member !== undefined && member.obstacle !== null && member.obstacle.name === name
        ? rowObstacleChip(member, member.obstacle, now)
        : obstacleNameChip(name);
  return id === '' ? { label: 'obstacle', title, value } : { label: id, labelEntity: accountEntity(id), title, value };
}

/**
 * A pin key's session and agent where its text has one reading: four parts, none empty. A key id holds no `/`, but a
 * session id, an agent id and a model can, so any other split leaves the session unknown.
 */
function pinKeyParts(text: string): { agentId: string; sessionId: string } | null {
  const parts = text.split('/');
  return parts.length === 4 && !parts.includes('') ? { agentId: parts[2], sessionId: parts[1] } : null;
}

/** A pin key with no one reading, whole behind its key id, so its chip names no session the text does not fix. */
const behindKeyId = (text: string): string => text.slice(text.indexOf('/') + 1);

/**
 * A shedding mark is its pin's key, `key/session/agent/model`. The session's prefix and the agent tell a round's marks
 * apart. `member` is the record's own entry for the mark, which names the session whole.
 */
export function markChip(text: string, member: { agentId: string; sessionId: string } | null): Chip {
  const pin = member ?? pinKeyParts(text);
  if (pin === null) return { label: 'mark', mono: true, title: text, value: behindKeyId(text) };
  return { href: entityHref('session', pin.sessionId), label: 'mark', mono: true, title: text, value: `${pin.sessionId.slice(0, 8)}/${pin.agentId}` };
}

/**
 * A pin a round passed over: its session and why, `session: reason`. The engine writes it as the pin's key,
 * `key/session/agent/model`, then the reason. A key with no one reading passes no session and `shown` in its place.
 */
function skipChip(session: string, reason: string, title: string, label?: string, shown = session.slice(0, 8)): Chip {
  return { ...(session ? { href: entityHref('session', session) } : {}), ...(label === undefined ? {} : { label }), title, value: `${shown}: ${wordOf(LABELS, reason)}` };
}

/** A drift refusal's client version: the summary's `client` fact carries it, the record's reason the trigger alone. */
function clientVersionOf(row: DecisionSummary | undefined): string | null {
  const value = row?.fact.find((f) => f.key === 'client')?.value;
  return typeof value === 'string' ? value : null;
}

/** A refusal's reason: a contract's missing item, a drift's trigger beside the client's version, a guardrail's block
 *  beside the guardrail's name, or a key refusal's words by its code, its sentence for a code the console has no words
 *  for. */
function refusalChips(refusal: Refusal, version: string | null): Chip[] {
  if (refusal.kind === 'guardrail') return [plain('guardrail blocked'), plain(refusal.reason, 'guardrail')];
  if (refusal.kind !== 'client') return [plain(lookup(KEY_REFUSAL_WORD, refusal.code) ?? refusal.reason)];
  if (refusal.code === 'contract') return [plain(words(refusal.reason), 'contract')];
  return [plain(words(refusal.reason || 'client'), 'drift'), ...(version ? [plain(version, 'client')] : [])];
}

/** A round's reason for marking nothing, where its words differ from the cause's label on a pin's row. */
const ROUND_REASON_WORD: Record<string, string> = { account_bound: 'each holds state on its account' };
/** Why a move rule did not run, where the words differ from the reason's own. */
const NOT_RUN_REASON_WORD: Record<string, string> = { account_bound: 'the session is account-bound' };
export const isMoveRule = (step: string) => step.startsWith('move:');
/** The chips of a move rule's `reason` fact where the rule did not run and the table words the reason: the
 *  words alone, with no label. Undefined for each other fact, which keeps its own chips. */
export const notRunReasonChips = (step: Pick<WalkStep, 'step' | 'verdict'>, f: WalkFact): Chip[] | undefined => {
  if (!isMoveRule(step.step) || step.verdict !== 'not_run' || f.key !== 'reason' || typeof f.value !== 'string') return undefined;
  const word = lookup(NOT_RUN_REASON_WORD, f.value);
  return word === undefined ? undefined : [plain(word)];
};

export function factChips(f: WalkFact, record: DecisionRecord, now: number): Chip[] {
  const v = f.value;
  const members = new Set(record.candidates.map((c) => c.accountId));
  switch (f.key) {
    // The rung line draws its entry from the candidate table instead, eight characters with the rest on hover.
    case 'hash':
      return [];
    case 'account_id':
      return typeof v === 'string' ? [accountChip(v, 'on')] : [];
    // The pin step writes the issuer's or the judged turn's account as the value and about the same account, so one
    // chip names it once. A value about no account, the text `none` among them, reads as a plain fact.
    case 'credit':
    case 'judged': {
      const account = namedAccount(f);
      if (account !== null) return [accountChip(account, keyWord(f.key))];
      break;
    }
    case 'lane':
      if (typeof v === 'string') return laneChips(v);
      break;
    case 'marks':
      if (Array.isArray(v)) {
        const shed = record.outcome.shedMarks ?? [];
        return v.map((m) => {
          const text = String(m);
          return markChip(text, shed.find((mk) => `${mk.keyId}/${mk.sessionId}/${mk.agentId}/${mk.model}` === text) ?? null);
        });
      }
      break;
    // The record holds no member for a skipped pin, so its text alone names the session.
    case 'skipped':
      if (Array.isArray(v)) {
        return v.map((x) => {
          const text = String(x);
          const at = text.lastIndexOf(': ');
          const key = at < 0 ? text : text.slice(0, at);
          const pin = pinKeyParts(key);
          return skipChip(pin?.sessionId ?? '', at < 0 ? '' : text.slice(at + 2), text, 'skipped', pin === null ? behindKeyId(key) : undefined);
        });
      }
      break;
    case 'obstacles':
      if (Array.isArray(v)) return v.map((o) => obstacleChip(String(o), record, now));
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
  const rung = lookup(RUNG_OF, s.step);
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

const ruleWord = (step: string) => (lookup(STEP_NAME, step) ?? words(step.slice('move:'.length))).toLowerCase();

/**
 * The walk as numbered lines in the engine's order. The rules after a fired one never ran, so they fold into one line,
 * "Rules after spent", with the rules as chips. A drift refusal's admission line reads the client's version off `row`.
 */
export function walkLines(record: DecisionRecord, row?: DecisionSummary): WalkLine[] {
  const at = Date.parse(record.request.at);
  const lines: WalkLine[] = [];
  let fired: string | null = null;
  for (const s of record.walk) {
    if (fired !== null && isMoveRule(s.step) && s.verdict === 'not_run') {
      const last = lines[lines.length - 1];
      if (last?.collapsed) {
        last.collapsed.push(s.step);
        last.chips.push(plain(ruleWord(s.step)));
      } else {
        lines.push({ chips: [plain(ruleWord(s.step))], collapsed: [s.step], number: lines.length + 1, step: `Rules after ${ruleWord(fired)}`, verdict: 'not_run' });
      }
      continue;
    }
    if (isMoveRule(s.step) && s.verdict === 'fired') fired = s.step;
    const refusal = record.outcome.refusal;
    const chips =
      s.step === 'admission' && refusal !== null
        ? refusalChips(refusal, clientVersionOf(row))
        : s.step === 'selection' && s.facts.length === 0
          ? ['moves', 'rungs', 'partitions'].map((p) => plain(p))
          : s.facts.flatMap((f) => notRunReasonChips(s, f) ?? factChips(f, record, at));
    const entry = rungEntry(s, record);
    lines.push({ chips, ...(entry === undefined ? {} : { hash: entry }), number: lines.length + 1, step: wordOf(STEP_NAME, s.step), verdict: s.verdict });
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
      return [lead('Held on'), ...on(o.hold?.accountId ?? o.accountId), ...(o.hold?.reason === 'rewrite' ? rewriteWords(o.hold, Date.parse(record.request.at)) : [])];
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
  return knownWindowsFirst(Object.keys(shares)).flatMap((w) => {
    const v = num(shares[w]);
    return v === null ? [] : [plain(fmtShare(v), windowTagWord(w))];
  });
}

/** `for the reset at 18:20`, or the reset alone where the hold carried no instant. */
const resetWords = (hold: NonNullable<RecordOutcome['hold']>, now: number) => (hold.until === null ? WAIT_REASON_WORD.reset : `${WAIT_REASON_WORD.reset} at ${formatFact(hold.until, 'time', now)}`);
/** `for the slow-down until 18:20`, the pin's deadline, or the slow-down alone where the hold carried none. */
const slowDownWords = (hold: NonNullable<RecordOutcome['hold']>, now: number) => (hold.until === null ? WAIT_REASON_WORD.slow_down : `${WAIT_REASON_WORD.slow_down} until ${formatFact(hold.until, 'time', now)}`);
/**
 * `for the rewrite in flight of {request} until 18:20`: the first request's head the hold waited for, linked, and the
 * words without `of` on a hold that names no request.
 */
const rewriteWords = (hold: NonNullable<RecordOutcome['hold']>, now: number): Chip[] => [
  ...(hold.waitsFor === null ? [plain(WAIT_REASON_WORD.rewrite)] : [plain(`${WAIT_REASON_WORD.rewrite} of`), requestChip(hold.waitsFor)]),
  ...(hold.until === null ? [] : [plain(`until ${formatFact(hold.until, 'time', now)}`)]),
];

/**
 * The tier, the rule or the rung, the pin's warmth, the cache verdict once the attempt has usage, and on a move the
 * account-bound thinking its answer dropped.
 */
export function verdictChips(record: DecisionRecord, row: DecisionSummary): Chip[] {
  const o = record.outcome;
  const chips: Chip[] = [];
  const member = memberOf(record, o.accountId);
  const tier = o.tier ?? member?.tier ?? null;
  if (tier !== null && o.accountId !== null) chips.push(plain(`${tierWord(tier, member ?? null, 'plan quota')} P${tier[1]}`, 'tier'));
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
  if (o.kind === 'held' && o.hold?.reason === 'slow_down') {
    chips.push(plain(`held ${slowDownWords(o.hold, Date.parse(record.request.at))}`));
    if (o.hold.factor !== null) chips.push(plain(speedWord(o.hold.factor)));
  }
  if (o.kind === 'held' && o.hold?.reason === 'rewrite') chips.push(plain(`held ${WAIT_REASON_WORD.rewrite}`));
  // The chip names the first plan window the booking holds a share of, the 5-hour one wherever it does.
  const booked = o.reservation === null ? undefined : WINDOW_KEYS.find((w) => num(o.reservation?.rewrite[w]) !== null);
  if ((o.kind === 'moved' || o.kind === 'placed') && o.reservation !== null && booked !== undefined) chips.push(plain(`reserved ${fmtShare(o.reservation.rewrite[booked])} of ${windowTag(booked)}`));
  if (record.pin?.found === 'live' && record.pin.warmth) chips.push(plain(wordOf(WARMTH_WORD, record.pin.warmth), 'pin'));
  const c = o.completion;
  const promise = rung !== null && c !== null && c.promisedTokens !== null ? PROMISE_HINT[rung.rung] : undefined;
  const figure = c === null ? null : cacheFigure(c);
  if (c !== null && c.hit !== null && figure !== null) {
    chips.push({ ...plain(figure.text, c.hit ? 'cache hit' : 'cache miss'), title: figure.title ?? promise });
  } else if (c !== null && c.completedAt === null) {
    chips.push(plain('cache pending'));
  }
  // An account key stands on each answer from a move to the next compaction, and the prefix key on each answer of a
  // compacted session, so the chip takes the move's own record and an account reason. A failover's landing is a
  // placement whose pin moved, so the move reads off the pin event.
  const dropped = c?.inputTransformations ?? {};
  const bound = o.pinEvents.some((e) => e.kind === 'pin_migrated') ? transformationOrder(Object.keys(dropped)).filter(accountBound) : [];
  if (bound.length > 0) {
    const total = bound.reduce((n, key) => n + dropped[key].blocks, 0);
    chips.push({ ...plain(plural(total, 'block'), 'account-bound thinking'), title: bound.map((key) => `${transformationLabel(key)}: ${plural(dropped[key].blocks, 'block')}`).join('\n') });
  }
  return chips;
}

const ordinal = (n: number) => `${n}${n % 100 >= 11 && n % 100 <= 13 ? 'th' : (['th', 'st', 'nd', 'rd'][n % 10] ?? 'th')}`;
const VERDICT_BADGE: Record<string, string> = { held_on: 'held on', not_needed: 'not needed' };
const PARTITION_WORD: Record<string, string> = { above_ceiling: 'above the ceiling', at_cap: 'at its cap', floor: 'at the Pace floor', spent: 'window spent', winding_down: 'winding down' };
const obstacleNameChip = (name: string): Chip => ({ title: hintOf(name), value: lookup(PARTITION_WORD, name) ?? wordOf(OBSTACLE_WORD, name) });

/**
 * The plan beneath a member's account: `Ultra`, `Free`, `API key`. The record's resolved word reads through the label
 * table, and an older record's tier word through `planLabel`.
 */
const planWord = (c: RecordCandidate) => (c.kind === 'api_key' ? 'API key' : c.planTier === null ? 'OAuth' : (lookup(PLAN_WORD_LABEL, c.planTier) ?? planLabel(c.planTier)));

/** The member's prefix entry: the match on the rung the outcome names, else the deepest entry a gate refused. */
function prefixEntryOf(c: RecordCandidate, record: DecisionRecord): PrefixEntry | null {
  const rung = record.outcome.rung?.rung;
  const match = rung === 'static' ? c.prefix.staticMatch : rung === 'content' ? c.prefix.contentMatch : (c.prefix.contentMatch ?? c.prefix.staticMatch);
  if (match) return match;
  const order = rung === 'static' ? [c.prefix.static, c.prefix.content] : [c.prefix.content, c.prefix.static];
  return order.find((e): e is PrefixEntry => e !== null && e.refusedBy !== null) ?? null;
}

/** A 429's words on the window it names, null where it names none or one the console does not know. */
function rateLimitWords(window: string | null): string | null {
  const word = window === null ? undefined : windowTagOf(storedFableKey(window));
  return word === undefined ? null : `429 on ${word}`;
}

/** A cooldown's reason and window as the console words them, with no deadline. */
function cooledWords(reason: string | null, window: string | null): string {
  const r = reason ?? '';
  if (r === 'model_rejected' || window === 'model_rejection') return 'model 404';
  if (r === 'fast_rate_limit' || window === 'fast') return 'fast lane 429';
  if (r === 'rate_limit') return rateLimitWords(window) ?? '429';
  return words(r || 'cooldown');
}

const untilWords = (o: NonNullable<RecordCandidate['obstacle']>, now: number): string => (o.until === null ? '' : ` until ${formatFact(o.until, 'time', now)}`);

/**
 * The scope a cooldown's words leave out, as the account page spells it: the model a cooldown holds. It follows `for`,
 * since a chip holds one value and a comma would join two. The account scope is the default, and the cooled words of
 * the fast lane's two rows name the lane and its credits whatever window the router files them under.
 */
function scopeWords(o: NonNullable<RecordCandidate['obstacle']>): string {
  if (o.scope === null || o.scope === 'account' || isFastScope(o.scope)) return '';
  return ` for ${scopeLabel(o.scope).label}`;
}

function cooldownWords(o: NonNullable<RecordCandidate['obstacle']>, now: number): string {
  return `${cooledWords(o.reason, o.window)}${untilWords(o, now)}${scopeWords(o)}`;
}

/** A spent member's hover and words: the spend cap's reset, or the plan window's own where the obstacle names one. */
const spentChip = (o: NonNullable<RecordCandidate['obstacle']>, now: number): { title: string | undefined; value: string } => {
  const until = untilWords(o, now);
  if (o.window === SPEND_WINDOW) return { title: `at its spend cap${until}`, value: `spend cap reached${until}` };
  if (o.window === null) return { title: hintOf('spent'), value: `window spent${until}` };
  const tag = windowTagWord(o.window);
  return { title: `its ${tag} window spent${until}`, value: `${tag} spent${until}` };
};

/**
 * A failover's cause in one spelling for the event card and the record card: a cooldown's reason and window as
 * `cooldownWords` spells them ("429 on 5h", "fast lane 429"), a mark as `<state> by <cause>` in the words the account
 * state card reads.
 */
function causeWords(cause: { cause?: string | null; reason?: string | null; scope?: string | null; state?: string | null; window?: string | null }): string {
  if (typeof cause.state === 'string') return `${wordOf(STATE_WORD, cause.state)} by ${wordOf(CAUSE_WORD, cause.cause ?? 'unrecorded')}`;
  return cooledWords(cause.reason ?? null, cause.window ?? null);
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
  const title = c.state === 'deleted' ? 'the account left the registry' : (lookup(STATE_EFFECT, c.state) ?? hintOf('operator'));
  return { title, value: wordOf(MEMBER_STATE_WORD, c.state) };
}

/** A candidate row's obstacle as its chip: the operator's own words, a spent window's chip, else the obstacle's
 *  words with its hint. The candidate table and an exhausted line both draw it. */
function rowObstacleChip(c: RecordCandidate, obstacle: NonNullable<RecordCandidate['obstacle']>, now: number): Chip {
  const hinted = (value: string): Chip => ({ title: hintOf(obstacle.name), value });
  switch (obstacle.name) {
    case 'budget': {
      const b = reachedBudget(c);
      return hinted(b && b.spentUsd !== null ? `budget ${fmtUsd(b.spentUsd)} of ${fmtUsd(b.budgetUsd)}` : 'budget');
    }
    case 'cooldown':
      return hinted(cooldownWords(obstacle, now));
    case 'credits':
      return hinted(`credits ${obstacle.reason === null ? 'unavailable' : wordOf(CREDITS_WORD, obstacle.reason)}`);
    case 'operator':
      return operatorWords(c);
    case 'spent':
      return spentChip(obstacle, now);
    default:
      return obstacleNameChip(obstacle.name);
  }
}

/**
 * The chip of a destination the gate read and found short, where rewrite only and least bad warn and an ending account
 * is neutral, and of a placement's member with no allowance left, in the tone the Pools card gives the word.
 */
const FIT_CHIP: Record<string, { tone: VerdictTone; word: string }> = { ending: { tone: 'neutral', word: 'ending' }, least_bad: { tone: 'warning', word: 'least bad' }, rewrite_only: { tone: 'warning', word: 'rewrite only' }, spent: { tone: 'serious', word: LAST_RESORT_LABEL.spent } };

/** The verdict cell's one chip, and the tone it carries where it names the destination's fit. */
function verdictChipOf(c: RecordCandidate, now: number): { chip: Chip | null; tone: VerdictTone | null } {
  const untoned = (chip: Chip | null) => ({ chip, tone: null });
  if (c.obstacle !== null) return untoned(rowObstacleChip(c, c.obstacle, now));
  if (c.partition !== null) return untoned({ title: hintOf(c.partition), value: wordOf(PARTITION_WORD, c.partition) });
  // The cap held the request on its member whatever that member's fit, so the cap explains the hold.
  if (c.verdict === 'held_on' && c.inFlight >= c.inFlightCap) return untoned(plain('at its cap'));
  const fit = c.fit === null ? undefined : lookup(FIT_CHIP, c.fit);
  if (fit !== undefined) return { chip: plain(fit.word), tone: fit.tone };
  return untoned(placementChipOf(c));
}

function placementChipOf(c: RecordCandidate): Chip | null {
  if (c.verdict === 'destination') return plain('not placed');
  if (c.verdict === 'source') {
    const spent = WINDOW_KEYS.find((w) => (lookup(c.windows, w)?.utilization ?? 0) >= 1);
    return spent === undefined ? null : plain(`${windowTag(spent)} spent`);
  }
  return null;
}

/** A Pace window's word in the column header and in a cell's hover. A window the console does not know reads its id. */
const paceWindowWord = (id: string): string => windowEntry(WINDOW_LABEL, id) ?? words(id);

/**
 * A usage-based seat: the engine writes a paid source's pace with no hours, no deadline and no window whatever its
 * tier, and keeps it where a share that lapsed at its reset or was not yet read leaves no spend entry, so the shape
 * marks the class and neither the entry nor the tier does.
 */
const isSpendSeat = (c: RecordCandidate): boolean => c.kind === 'oauth' && c.pace !== null && c.pace.hours === null && c.pace.deadline === null && c.pace.window === null;

/**
 * A seat whose record carries the spend entry: the cells draw the reading the entry holds, while the title and the
 * header read the source's shape.
 */
const hasSpendEntry = (c: RecordCandidate): boolean => c.kind === 'oauth' && has(c.windows, 'spend');

/**
 * The Pace column's header in its parts, which the candidate table's head draws a line each: the window the score read
 * of the first subscription candidate that is no spend seat and whose pace names a window, so a lapsed seat listed
 * first leaves the header to the next one. It reads "Pace" when no such candidate stands. A version 1 record read no
 * burn, so its score was the share left per hour.
 */
export function paceHeaderParts(record: DecisionRecord): string[] {
  const pace = record.candidates.find((c) => c.kind === 'oauth' && !isSpendSeat(c) && (c.pace?.window ?? null) !== null)?.pace;
  const id = pace?.window ?? null;
  if (id === null) return ['Pace'];
  return (pace?.burn ?? null) === null ? ['Pace', `${paceWindowWord(id)} left per hour`] : ['Spare rate', `${paceWindowWord(id)} per hour`];
}

/** The Pace column's header whole, its parts joined behind their commas. */
export function paceHeader(record: DecisionRecord): string {
  return paceHeaderParts(record).join(', ');
}

/**
 * The score's unit is the record's: the pick step's `score` fact about the candidate where one names it, else the
 * pace's own shape. The engine writes score facts for the pick's first tier alone, so a seat on a renewal, a rung
 * placement or a lower tier finds none, and its paid arm writes a pace with neither hours nor window (`percent`) where
 * a plan pace carries both (`percent_per_hour`). A version 1 record reads the plan unit, since its plan pace always
 * carried its hours.
 */
const scoreUnit = (c: RecordCandidate, record: DecisionRecord): FactUnit => {
  const fact = record.walk.find((s) => s.step === 'pick')?.facts.find((f) => f.key === 'score' && f.accountId === c.accountId);
  const unit = unitOf(fact?.unit ?? null);
  if (unit !== null) return unit;
  return c.pace !== null && c.pace.hours === null && c.pace.window === null ? 'percent' : 'percent_per_hour';
};

/**
 * A Pace cell's hover: the share left, the Fable reservation where one applies, the hours it is spread over and the
 * rate, or an API key's share alone.
 */
function paceTitle(c: RecordCandidate, record: DecisionRecord, now: number): string | null {
  const p = c.pace;
  if (p === null) return null;
  if (c.kind === 'api_key') return `${fmtPct(p.remaining)} of its worst live bucket left. Scored by the remaining share alone, with no division by hours.`;
  if (isSpendSeat(c)) return `${fmtPct(p.remaining)} of its allowance left. Scored by the remaining share alone, with no division by hours.`;
  const id = p.window;
  const share = id === null ? 'its window' : `the ${paceWindowWord(id)} window`;
  const reserved = p.fableReservation;
  const left = `${fmtPct(p.remaining)} of ${share} left${reserved === null ? '' : `, less ${fmtPct(reserved)} reserved for Fable,`}`;
  // Whole hours would read a deadline under half an hour away as "0 h".
  const over = p.hours === null ? '' : ` over ${p.hours < 1 ? `${Math.round(p.hours * 60)} min` : `${Math.round(p.hours)} h`}`;
  // The deadline is the earlier of the two, so one equal to the reset is the reset whatever the end reads.
  const deadline = p.deadline === null ? ' with no reset read' : ` to ${p.reset !== null && msOf(p.reset) === msOf(p.deadline) ? 'its reset' : "its subscription's end"} at ${formatFact(p.deadline, 'time', now)}`;
  const score = formatFact(p.score, scoreUnit(c, record), now);
  // A version 1 record read no burn, and its score was the share left an hour above the pace floor.
  const burn = p.burn;
  if (burn === null) return `${left}${over}${deadline}: ${score}. Inside its tier, the highest above the pace floor wins.`;
  return `${left}${over}${deadline}, less ${formatFact(burn, 'percent_per_hour', now)} of burn: ${score}. Inside its tier, the highest spare rate wins.`;
}

/**
 * The projection at each window's horizon the candidate reports, one line a window: the horizon as a time when it is
 * the reset, else as the span to it from the request (one 5-hour length, or the account's end). A candidate the gate
 * did not read reads a dash.
 */
function projectionLines(c: RecordCandidate, record: DecisionRecord, now: number): string[] {
  if (c.fit === null) return ['-'];
  const at = Date.parse(record.request.at);
  const lines = WINDOW_KEYS.flatMap((name) => {
    const w = lookup(c.windows, name);
    if (w === undefined || w.projection === null) return [];
    const horizon = w.horizon === null ? null : Date.parse(w.horizon);
    const when = horizon === null ? '' : w.reset !== null && Date.parse(w.reset) === horizon ? ` at ${formatFact(horizon, 'time', now)}` : ` in ${span(horizon - at)}`;
    return [`${windowTag(name)} ${fmtPct(w.projection)}${when}`];
  });
  return lines.length > 0 ? lines : ['-'];
}

/**
 * The Projection cell's hover where a slowed account's demand differs from the burn its sessions achieve: the demand,
 * the burn and the speed the account is held to, which the projection does not show.
 */
function projectionTitle(c: RecordCandidate, now: number): string | null {
  const demand = lookup(c.windows, 'five_hour')?.demand ?? null;
  const burn = num(c.burn.five_hour);
  if (demand === null || burn === null || demand === burn) return null;
  return `demand ${formatFact(demand, 'percent_per_hour', now)} over burn ${formatFact(burn, 'percent_per_hour', now)}${c.factor === null ? '' : ` at ${speedWord(c.factor)}`}`;
}

/**
 * The time a pin could stay before its window that binds reaches the ceiling, unbounded where nothing binds. A spent
 * member states no runway, so it reads null.
 */
const runwayText = (fit: string, ms: number | null): string | null => (fit === 'spent' ? null : ms === null ? 'unbounded' : ms === 0 ? 'none' : span(ms));
/** A candidate's runway cell, a dash for a candidate with no fit and for a spent one. */
const runwayWord = (c: RecordCandidate) => (c.fit === null ? '-' : (runwayText(c.fit, c.runwayMs) ?? '-'));

/** A spend window's lines: the share over the reset, and the estimate's word where no head has named the reset yet. */
const spendLines = (w: RecordCandidate['windows'][string], now: number): string[] => [`spend ${fmtPct(w.utilization)}`, w.reset === null ? 'no reset' : formatFact(w.reset, 'time', now), ...(w.source === 'estimated' ? ['estimated'] : [])];

/**
 * A candidate's verdict: its badge and the badge's tone, whether the decision chose it, and the one chip that decided
 * it with that chip's tone.
 */
export function candidateVerdict(c: RecordCandidate, now: number): { badge: string; badgeTone: 'accent' | 'neutral' | 'warning'; chip: Chip | null; chosen: boolean; tone: VerdictTone | null } {
  const atCap = c.verdict === 'out' && (c.partition === 'at_cap' || c.obstacle?.name === 'at_cap');
  const badge = c.verdict === 'ranked' && c.rank !== null ? `ranked ${ordinal(c.rank)}` : atCap ? 'at cap' : wordOf(VERDICT_BADGE, c.verdict);
  const chosen = CHOSEN_VERDICTS.has(c.verdict);
  return { badge, badgeTone: chosen ? 'accent' : c.verdict === 'out' ? 'warning' : 'neutral', chosen, ...verdictChipOf(c, now) };
}

/**
 * One member's row of the candidate table: the account and its plan, the tier, the three windows (or a key's one
 * merged reading, or a usage-based seat's spend window), the projection with the demand on its hover and the runway the
 * gate read, the slots, the Pace score with its hover, the prefix entry and the verdict with the one chip that decided
 * it. A chip that names the destination's fit carries its tone.
 */
export function candidateCells(c: RecordCandidate, record: DecisionRecord, now: number): { account: Chip; inFlight: string; pace: string; paceTitle: string | null; plan: string; prefix: { hash: PrefixEntry | null; until: string | null }; projection: string[]; projectionTitle: string | null; runway: string; tier: string; verdict: ReturnType<typeof candidateVerdict>; windows: string[][] } {
  // A seat whose share lapsed carries no spend entry, so it reads as an OAuth candidate with no window at any tier.
  const spend = hasSpendEntry(c);
  const worst = lookup(c.windows, 'worst_bucket');
  const windows =
    c.kind === 'api_key'
      ? [[worst === undefined ? 'not reported' : `worst bucket ${fmtPct(worst.utilization)}`]]
      : spend
        ? [spendLines(c.windows.spend, now)]
        : WINDOW_KEYS.map((name) => {
            const w = lookup(c.windows, name);
            if (w === undefined) return ['not reported'];
            return [fmtPct(w.utilization), w.reset === null ? 'no reset' : formatFact(w.reset, 'time', now), `${w.source} ${ageWord(w.ageMs)}`];
          });
  const entry = prefixEntryOf(c, record);
  return {
    account: accountChip(c.accountId),
    inFlight: `${c.inFlight} of ${c.inFlightCap}`,
    pace: c.pace === null ? '-' : c.kind === 'api_key' || spend ? `${fmtPct(c.pace.remaining)} left` : formatFact(c.pace.score, scoreUnit(c, record), now),
    paceTitle: paceTitle(c, record, now),
    plan: planWord(c),
    prefix: { hash: entry, until: entry === null ? null : warmUntil(entry, now) },
    projection: projectionLines(c, record, now),
    projectionTitle: projectionTitle(c, now),
    runway: runwayWord(c),
    tier: `${tierWord(c.tier, c, 'plan')} P${c.tier[1]}`,
    verdict: candidateVerdict(c, now),
    windows,
  };
}

/** The verdicts that mark the candidate the decision chose, the strip's leaf and the card's tinted row alike. */
const CHOSEN_VERDICTS = new Set(['chosen', 'held_on', 'renewed']);
const VERDICT_ORDER: Record<string, number> = { chosen: 0, renewed: 1, held_on: 2, source: 3, destination: 4, ranked: 5, eligible: 6, out: 7, not_needed: 7 };

/**
 * The candidates by rank: the chosen, renewed, held-on, source and destination members in that order, then the ranked
 * ones by rank, then the eligible ones the strategy did not rank, then the out and not-needed ones, each group by
 * priority and then by account id.
 */
export function rankCandidates(candidates: RecordCandidate[]): RecordCandidate[] {
  const group = (c: RecordCandidate) => lookup(VERDICT_ORDER, c.verdict) ?? 8;
  const rank = (c: RecordCandidate) => (c.verdict === 'ranked' ? (c.rank ?? Number.POSITIVE_INFINITY) : 0);
  return [...candidates].sort((a, b) => group(a) - group(b) || rank(a) - rank(b) || a.priority - b.priority || a.accountId.localeCompare(b.accountId));
}

/**
 * A setting the card can draw a row of: every member of the record's settings but a version 1 record's `legacy`. The
 * rows leave out the pool's edit instant, which the fold's head reads.
 */
type SettingKey = Exclude<keyof RecordSettings, 'legacy'>;

/** The settings of the card in its order, Strategy to Retry in place. */
const SETTINGS_ROWS: { key: SettingKey; label: string }[] = [
  { key: 'strategy', label: 'Strategy' },
  { key: 'preferSubscription', label: 'Tier order' },
  { key: 'overagePolicy', label: 'Overage' },
  { key: 'affinityTtlSeconds', label: 'Pin window' },
  { key: 'affinityMode', label: 'Pin when' },
  { key: 'sessionPlacement', label: 'Sessions placed by' },
  { key: 'minSharedPrefixTokens', label: 'Minimum shared prefix' },
  { key: 'subagentPlacement', label: 'Subagents placed by' },
  { key: 'fiveHourCeiling', label: '5-hour ceiling' },
  { key: 'weeklyCeiling', label: 'Weekly ceiling' },
  { key: 'loadShedding', label: 'Load shedding' },
  { key: 'migrateWhenSpent', label: 'Move when spent' },
  { key: 'overageMoveGraceMinutes', label: 'Overage grace' },
  { key: 'returnToPlan', label: 'Return to plan' },
  { key: 'returnToPlanDwellSeconds', label: 'Return dwell' },
  { key: 'windDownMinutes', label: 'Wind-down' },
  { key: 'shedCap', label: 'Shed cap' },
  { key: 'burnAverageSeconds', label: 'Burn average' },
  { key: 'shedDwellSeconds', label: 'Shed dwell' },
  { key: 'fableReservationShedding', label: 'Shed on the Fable reservation' },
  { key: 'slowDown', label: 'Slow-down' },
  { key: 'slowDownStretchSeconds', label: 'Stretch' },
  { key: 'slowDownHoldSeconds', label: 'Hold' },
  { key: 'warmSessions', label: 'Warm sessions' },
  { key: 'inFlightCap', label: 'In-flight cap' },
  { key: 'retryInPlaceSeconds', label: 'Retry in place' },
];

const SECONDS: readonly SettingKey[] = ['affinityTtlSeconds', 'burnAverageSeconds', 'retryInPlaceSeconds', 'returnToPlanDwellSeconds', 'shedDwellSeconds', 'slowDownHoldSeconds', 'slowDownStretchSeconds'];
const MINUTES: readonly SettingKey[] = ['overageMoveGraceMinutes', 'windDownMinutes'];
const SHARES: readonly SettingKey[] = ['fiveHourCeiling', 'shedCap', 'weeklyCeiling'];
/** The settings a record from before carries no value for, since its pool had none of them when it was written. */
const UNRECORDED: readonly SettingKey[] = ['fiveHourCeiling', 'loadShedding', 'returnToPlan', 'returnToPlanDwellSeconds', 'slowDown', 'slowDownHoldSeconds', 'slowDownStretchSeconds', 'weeklyCeiling'];
/** The settings for which a record stored before the router wrote them draws no row, where `settingValue` would read
 *  the null as off. */
const UNWRITTEN: readonly SettingKey[] = ['fableReservationShedding'];

/** One setting's value as the grid draws it: a span, a share, `on` or `off`, or the setting's word. */
function settingValue(key: SettingKey, settings: RecordSettings): string {
  const v = settings[key];
  if (key === 'strategy') return wordOf(STRATEGY_WORD, String(v));
  if (key === 'preferSubscription') return v ? 'subscriptions first' : 'priority alone';
  if (v === null) return 'off';
  if (typeof v === 'boolean') return v ? 'on' : 'off';
  if (typeof v === 'number') {
    // Under `always` a return dwell of 0 returns a warm pin at once, where a zero span elsewhere turns its rule off.
    if (key === 'returnToPlanDwellSeconds' && v === 0) return 'at once';
    if (SECONDS.includes(key)) return v === 0 ? 'off' : span(v * 1_000);
    if (MINUTES.includes(key)) return span(v * MINUTE);
    if (SHARES.includes(key)) return fmtPct(v);
    if (key === 'minSharedPrefixTokens') return fmtCompact(v);
    return fmtInt(v);
  }
  return words(v);
}

export const PLACE_WORD: Record<string, string> = { strategy_pick: 'strategy pick' };
/** A wait's reason as a held row's hover and the Hold row read it: `cap` waited for a slot the in-flight cap freed. */
export const WAIT_REASON_WORD: Record<string, string> = { cap: 'for a slot', reset: 'for the reset', rewrite: 'for the rewrite in flight', slow_down: 'for the slow-down' };
/** Where a hold waited: its cohort's account, else the last one it waited on. */
export const heldAccount = (h: CohortHold): string | null => h.cohort ?? h.waitedOn[h.waitedOn.length - 1] ?? null;

/**
 * The card's outcome: Cause, Pin written, Cache-map entries, Cache read, Hold, Answer and Spend, each where it applies.
 * A failover's landing records a placement, so the Cause row follows the cause whatever the outcome's kind.
 */
export function outcomeRows(record: DecisionRecord, now: number, row?: DecisionSummary): CardRow[] {
  const o = record.outcome;
  const c = o.completion;
  const at = Date.parse(record.request.at);
  const rows: CardRow[] = [];
  if (o.cause) rows.push({ chips: [plain(causeWords(o.cause))], label: 'Cause' });
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
      const accounts = [...new Set(entries.map((e) => e.accountId))].map((id) => accountChip(id, 'on'));
      const depths = (list: typeof entries) => list.map((e) => plain(String(e.depth), `${e.tier} depth`));
      const prefix = plain(fmtCompact(Math.max(...entries.map((e) => e.prefixTokens))), 'prefix');
      if (entries.length < CACHE_MAP_COMPACT_FROM) rows.push({ chips: [...accounts, ...depths(entries), prefix], label: 'Cache-map entries' });
      else {
        // A long session's row would run to a chip per turn, so each tier folds to its count and its depth range.
        const tiers = new Map<string, number[]>();
        for (const e of entries) tiers.set(e.tier, [...(tiers.get(e.tier) ?? []), e.depth]);
        const tierChips = [...tiers].map(([tier, found]): Chip => {
          const [min, max] = [Math.min(...found), Math.max(...found)];
          const n = found.length;
          return { label: `${tier} ×${n}`, title: `${plural(n, 'cache-map entry', 'cache-map entries')} on the ${tier} tier`, value: min === max ? `depth ${min}` : `depth ${min} to ${max}` };
        });
        // The next turn reads the deepest message depths, while the router writes the static chain after them.
        const deepest = new Set(entries.filter((e) => e.tier === 'message').sort((a, b) => b.depth - a.depth).slice(0, 3));
        const latest = depths(entries.filter((e) => deepest.has(e)));
        rows.push({ chips: [...accounts, ...tierChips, ...latest, prefix], label: 'Cache-map entries', more: { all: [...accounts, ...depths(entries), prefix], count: entries.length } });
      }
    }
    const figure = cacheFigure(c);
    if (c.hit !== null && figure !== null) {
      // The request's own TTL is the one its deepest entry was written under.
      const deepest = entries.reduce<(typeof entries)[number] | null>((best, e) => (best === null || e.depth > best.depth ? e : best), null);
      const read = { ...plain(c.promisedTokens !== null ? figure.text : `${figure.text} input`, 'read'), ...(figure.title === null ? {} : { title: figure.title }) };
      rows.push({
        chips: [plain(c.hit ? 'hit' : 'miss'), read, ...(deepest ? [plain(ttlWord(deepest.ttlMs), 'tier')] : [])],
        label: 'Cache read',
        ...(o.rung !== null && c.promisedTokens !== null ? { title: PROMISE_HINT[o.rung.rung] } : {}),
      });
    } else if (c.completedAt === null) {
      rows.push({ chips: [plain('pending')], label: 'Cache read' });
    }
  }
  const last = record.request.holds[record.request.holds.length - 1];
  const ranOut = last?.ranOut === true;
  const ranOutChip = ranOut ? [plain('ran out')] : [];
  if (o.hold !== null && o.hold.reason === 'reset') {
    // The pin waits on its own account for the window to reset rather than for a slot.
    rows.push({ chips: [accountChip(o.hold.accountId, 'on'), plain(resetWords(o.hold, now)), plain(span(o.hold.boundMs), 'bound'), ...ranOutChip], label: 'Hold' });
  } else if (o.hold !== null && o.hold.reason === 'slow_down') {
    // The pin waits for its deadline rather than for a slot. A rejoin on a pool whose switch is off read no figures.
    const h = o.hold;
    const figures = [
      ...(h.factor === null ? [] : [plain(speedWord(h.factor), 'speed')]),
      ...(h.stretchMs === null || h.thresholdMs === null ? [] : [plain(`${span(h.stretchMs)} of ${span(h.thresholdMs)}`, 'stretch')]),
      plain(span(h.boundMs), 'bound'),
      ...(h.cycleMs === null ? [] : [plain(span(h.cycleMs), 'cycle')]),
    ];
    rows.push({ chips: [accountChip(h.accountId, 'on'), plain(slowDownWords(h, now)), ...figures, ...ranOutChip], label: 'Hold' });
  } else if (o.hold !== null && o.hold.reason === 'rewrite') {
    // The pin waits for the head of the request that carried its move rather than for a slot.
    const h = o.hold;
    rows.push({ chips: [accountChip(h.accountId, 'on'), ...rewriteWords(h, now), plain(span(h.boundMs), 'bound'), ...ranOutChip], label: 'Hold' });
  } else if (o.hold !== null) {
    const place = str(record.walk.find((s) => s.step === 'hold')?.facts.find((f) => f.key === 'place')?.value) ?? (o.hold.cohort !== null ? 'cohort' : 'strategy_pick');
    rows.push({ chips: [accountChip(o.hold.accountId, 'on'), plain(wordOf(PLACE_WORD, place)), plain(span(o.hold.boundMs), 'bound'), ...ranOutChip], label: 'Hold', title: HOLD_HINT });
  } else if (o.cohortHold !== null) {
    const h = o.cohortHold;
    const account = heldAccount(h);
    const on = account === null ? [] : [accountChip(account, 'on')];
    const slot = plain(h.cohort !== null ? 'cohort' : 'strategy pick');
    const ended = (ms: number) => plain(span(ms), c?.freed ? 'freed after' : 'held');
    if (h.waits.length === 0) {
      // A row written before the waits carries their sum alone.
      rows.push({ chips: [...on, slot, ended(c?.heldMs ?? h.heldMs), ...ranOutChip], label: 'Hold', title: HOLD_HINT });
    } else {
      // A wait for a reset or the slow-down is no slot's, so the in-flight cap's hover titles a slot's wait alone.
      h.waits.forEach((w, i) => {
        const final = i === h.waits.length - 1;
        const reason = w.reason === 'cap' ? slot : plain(wordOf(WAIT_REASON_WORD, w.reason));
        // The router fills the accounts waited on from the same holds in the same order, so each wait names its own.
        const waited = h.waitedOn[i] ?? null;
        const own = waited === null ? on : [accountChip(waited, 'on')];
        const chips = [...own, reason, final ? ended(w.heldMs) : plain(span(w.heldMs), 'held'), ...(final ? ranOutChip : [])];
        rows.push({ chips, label: 'Hold', ...(w.reason === 'cap' ? { title: HOLD_HINT } : {}) });
      });
    }
  } else if (o.exhaustion !== null && last !== undefined && last.attempt === record.request.attempt) {
    // The relay keeps every hold across a failover, and an earlier attempt's hold is no part of this decision.
    rows.push({ chips: [accountChip(last.accountId, 'on'), plain(last.cohort !== null ? 'cohort' : 'strategy pick'), plain(span(last.heldMs), 'held'), ...ranOutChip], label: 'Hold', title: HOLD_HINT });
  }
  if (o.exhaustion !== null) {
    const x = o.exhaustion;
    rows.push({
      chips: [
        ...(x.status !== null ? [plain(String(x.status), 'status')] : []),
        ...(x.window !== null ? [plain(windowTagWord(storedFableKey(x.window)), 'window')] : []),
        ...(x.reason !== null ? [plain(wordOf(CREDITS_WORD, x.reason), 'cause')] : []),
        plain(x.deadline === null ? 'none' : formatFact(x.deadline, 'time', at), x.deadline === null ? 'retry' : 'retry after'),
      ],
      label: 'Answer',
    });
  }
  if (o.refusal !== null) {
    const version = clientVersionOf(row);
    const said = o.refusal.kind === 'client' ? [plain(o.refusal.code === 'contract' ? 'client contract' : 'client drift'), ...refusalChips(o.refusal, version).filter((ch) => ch.label === 'client' || ch.label === 'contract')] : refusalChips(o.refusal, version);
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

const RULE_MOVES = new Set<string>(RULE_MOVE_REASONS);
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
const PLAN_SOURCE_WORD: Record<string, string> = { pairing: 'pairing confirm', profile: 'profile read', usage_403: 'usage 403' };
const SOURCE_WORD: Record<string, string> = { clock: 'the clock walk', head: 'the response head', poll: 'the poll', predicted: 'the prediction', sample: 'the usage sample' };
/** A spent window's source as its chip words it. A source outside the map prints as sent. */
const SOURCE_CHIP_WORD: Record<string, string> = { head: 'head', poll: 'poll', predicted: 'prediction' };
/**
 * A share of a window per hour as an event's detail reads one. A shedding round's rates read so on every account,
 * whichever table the round's order read.
 */
const hourlyShare = (perHour: number) => `${(perHour * 100).toFixed(1)} %/h`;
// The router records the wall as a line of 1, the window's whole share.
const lineWords = (limit: number): string => (limit >= 1 ? 'the wall' : `the ceiling ${fmtPct(limit)}`);
/**
 * A move's destination fit: the gate's word, or `spent` from the landing's row where a failover or a replaced pin lands
 * on a member with no allowance left.
 */
const FIT_WORD: Record<string, string> = { fits: 'fits', least_bad: 'least bad', rewrite_only: 'rewrite only', spent: LAST_RESORT_LABEL.spent };
/**
 * A pin's Rewrite cell. A round that names its `rewrite_unit` stored the Pro-week figure, which reads in the chosen
 * week titled with the window the pick compared on, untitled where the pick compared on none, and a dash while the
 * scale is out. An earlier build's round stored its window's 20x equivalent, which prints as stored under that label,
 * since a conversion would read it as a Pro-week figure, and its bare share where the pin names no window.
 */
const rewriteCell = (rewrite: number | null, window: string | null, unit: string | null, week: WeekReading): Chip => {
  if (rewrite === null) return plain('-');
  const word = window === null ? null : windowLabelOf(window);
  if (unit !== 'pro_week') return plain(word === null ? fmtShare(rewrite) : `${fmtShare(rewrite)} of 20x ${word}`);
  if (week.scale === null) return week.failure === null ? plain('-') : { ...plain('-'), title: week.failure.text };
  const cell = plain(weekShort(rewrite * week.scale, week.week));
  return word === null ? cell : { ...cell, title: `compared on the ${word} window` };
};

/** An event's detail as pairs: When, Account and Pool first, then the kind's own. `rows` are the rows the page holds,
 *  which a model rejection reads its cooldown's until off, `week` the chosen week a shedding round's Pro-week rewrite
 *  reads in, and `tiers` the cost model's tier order a catalog change lists its models in. */
export function detailPairs(row: RoutingEvent, now: number, rows: readonly RoutingLogRow[], week: WeekReading, tiers: ModelTiers): DetailPair[] {
  const d = row.detail;
  const pairs: DetailPair[] = [{ chips: [plain(fmtAbsolute(row.at))], label: 'When' }];
  if (row.accountId !== null) pairs.push({ chips: [accountChip(row.accountId)], label: 'Account' });
  if (row.poolId !== null) pairs.push({ chips: [{ entity: { id: row.poolId, kind: 'pool' }, value: row.poolId }], label: 'Pool' });
  const add = (label: string, ...chips: (Chip | null)[]) => {
    const kept = chips.filter((c): c is Chip => c !== null);
    if (kept.length > 0) pairs.push({ chips: kept, label });
  };
  const figure = (v: unknown, format: (n: number) => string, label?: string) => {
    const n = num(v);
    return n === null ? null : plain(format(n), label);
  };
  const time = (v: unknown, label?: string) => figure(v, (ms) => formatFact(ms, 'time', now), label);
  const count = (v: unknown) => figure(v, fmtInt);
  const pct = (v: unknown, label?: string) => figure(v, fmtPct, label);
  const seconds = (v: unknown) => figure(v, (s) => span(s * 1_000));
  const text = (v: unknown, map: Record<string, string> = {}) => {
    const s = str(v);
    return s === null ? null : plain(wordOf(map, s));
  };
  const asSent = (v: unknown) => {
    const s = str(v);
    return s === null ? null : plain(s);
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
      add('Previous period', text(d.period));
      add('Spent', figure(d.previous_spent_usd, fmtUsd));
      add('Period start', time(d.period_start));
      break;
    }
    case 'budget_reached': {
      add('Budget', figure(d.budget_usd, (usd) => `${fmtUsd(usd)} a ${str(d.period) ?? 'period'}`));
      add('Spent', figure(d.spent_usd, fmtUsd));
      add('Period ends', time(d.period_end));
      add('Effect', plain('the pool answers exhausted until then'));
      add('Crossed by', request);
      break;
    }
    case 'catalog_changed':
      // The writer lists the models alphabetically, and every cell that lists models reads them by family and version.
      add('Added', ...tierOrder(list(d.added).map(String), tiers).map((m) => modelChip(m)));
      add('Removed', ...tierOrder(list(d.removed).map(String), tiers).map((m) => modelChip(m)));
      add('Models', count(d.count));
      break;
    case 'cooldown_cleared':
    case 'cooldown_set': {
      const scope = str(d.scope) ?? '';
      const model = scopeModel(scope);
      add('Scope', model !== null ? modelChip(model, 'model') : plain(scope === 'fast' ? 'fast lane' : words(scope || 'account')));
      if (row.kind === 'cooldown_cleared') {
        add('Cleared', text(d.cause, CLEARED_WORD));
        break;
      }
      add('Reason', text(d.reason));
      add('Window', window === null ? null : plain(windowTagWord(storedFableKey(window))));
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
    case 'ledger_gap': {
      add('Reason', text(d.reason));
      const meaning = lookup(GAP_MEANING, str(d.reason) ?? '');
      add('Meaning', meaning === undefined ? null : plain(meaning));
      if (str(d.reason)?.startsWith('chunk')) add('Effect', plain('the captured response is stored truncated'));
      add('Request', request);
      break;
    }
    case 'limit_reset_claimed': {
      // The upstream's words keep their underscores, which `text` would read as spaces. The claim's request id names no
      // relayed request, so it draws unlinked.
      const grant = str(d.grant_id);
      const outcome = resetOutcome(d.outcome);
      const status = num(d.status);
      const error = str(d.error);
      const requestId = str(d.request_id);
      add('Grant', asSent(d.label), grant === null ? null : { mono: true, value: grant });
      add('Outcome', outcome === null ? null : plain(outcome));
      add('Result', asSent(d.result));
      add('Reason', asSent(d.reason));
      add('Cleared', ...strings(d.cleared).map((name) => plain(windowTag(name))));
      add('Resets left', count(d.resets_left));
      add('Cooldown until', time(d.cooldown_until));
      add('Status', status !== null ? plain(String(status)) : error === null ? null : plain('transport failure'));
      add('Error', asSent(d.error));
      add('Request id', requestId === null ? null : { mono: true, value: requestId });
      add('Actor', plain(row.actor ?? 'admin'));
      break;
    }
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
        const set = row.requestId !== null && row.model !== null ? rows.find((r): r is RoutingEvent => r.type === 'event' && r.kind === 'cooldown_set' && r.requestId === row.requestId && r.accountId === row.accountId && str(r.detail.scope) === `${MODEL_SCOPE_PREFIX}${row.model}`) : undefined;
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
      add('Reasons', ...reasonChips(d));
      add('Found by', request ?? plain('the sweep'));
      break;
    case 'pin_migrated': {
      const from = movedFrom(row);
      const reason = str(d.reason) ?? '';
      const epoch = num(d.epoch);
      const caused = causedMove(d);
      add('From', from === null ? null : accountChip(from));
      add('To', row.accountId === null ? null : accountChip(row.accountId), epoch === null ? null : plain(fmtInt(epoch), 'epoch'));
      add('Reason', plain(movedLabel(d)));
      if (caused) add('Reasons', ...reasonChips(d));
      // A failover's detail carries its cause as an object, a rule's move and a failover that marked nothing a null.
      if (d.cause !== null && typeof d.cause === 'object' && !Array.isArray(d.cause)) {
        const cause = rec(d.cause);
        add('Cause', plain(causeWords({ cause: str(cause.cause), reason: str(cause.reason), scope: str(cause.scope), state: str(cause.state), window: str(cause.window) })));
      }
      add('Moved by', plain(caused ? "the account's state" : RULE_MOVES.has(reason) ? `${words(reason)} rule` : FAILOVER_MOVES.has(reason) ? 'failover' : words(reason || 'move')));
      // A move the gate read carries its verdict, and the detail carries a null runway where no window bounds it. A
      // landing on a spent member states no runway, so its null draws no line.
      const fit = str(d.fit);
      if (fit !== null) {
        const runway = runwayText(fit, num(d.runway_ms));
        add('Destination fit', plain(wordOf(FIT_WORD, fit)));
        add('Runway', runway === null ? null : plain(runway));
      }
      pin();
      break;
    }
    case 'pin_released':
      add('Actor', plain(row.actor ?? 'admin'));
      pin();
      break;
    case 'plan_ended':
    case 'plan_restored':
      add('Source', text(d.source, PLAN_SOURCE_WORD));
      add('Organization type', text(d.organization_type));
      add('Rate-limit tier', text(d.rate_limit_tier));
      add('Subscription status', text(d.subscription_status));
      break;
    case 'poll_backoff_ended': {
      const resumed = num(d.resumed_at) ?? atOf(row);
      const lasted = num(d.duration_ms);
      add('Backed off', lasted === null ? null : plain(formatFact(resumed - lasted, 'time', now), 'from'), plain(formatFact(resumed, 'time', now), 'to'));
      add('Attempts', count(d.attempts));
      // The plan's end leaves the poll paused, so its close names the pause's instant, and a row written before the
      // cause was a clean poll's.
      if (str(d.cause) === 'plan_ended') add("Ended by the plan's end", time(d.resumed_at));
      else add('Resumed', time(d.resumed_at));
      break;
    }
    case 'poll_backoff_started':
      add('Reason', plain(POLL_OUTCOME_LABEL.rate_limited));
      add('Attempts', count(d.attempts));
      add('Next attempt', time(d.next_attempt_at));
      add('Last clean poll', time(d.last_clean_poll_at));
      add('Effect', plain("decisions read this account's windows from response heads alone"));
      break;
    case 'poll_failed': {
      const status = pollStatus(d);
      add('Endpoint', text(d.endpoint));
      add('Status', status === null ? null : plain(status));
      add('Error', text(d.error));
      break;
    }
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
      const { entries: diff, secret } = registryDiff(d.diff);
      const primitive = (v: unknown): boolean => v === null || ['boolean', 'number', 'string'].includes(typeof v) || (Array.isArray(v) && v.every(primitive));
      if (diff.every(([, change]) => isChange(change) && primitive(rec(change).from) && primitive(rec(change).to))) {
        for (const [field, change] of diff) add(fieldLabel(field), plain(fieldValue(field, rec(change).from, now), 'from'), plain(fieldValue(field, rec(change).to, now), 'to'));
      } else {
        add('Fields', plain(fmtInt(diff.length)));
      }
      if (secret) add('Secret', plain('replaced'));
      add('Actor', row.actor === null ? null : plain(row.actor));
      // A record holds the pool's settings it was decided under, so one written before a pool's edit reads each field's
      // old value. It holds no keepalive policy, so a policy member's dotted field has no reading.
      if (kind === 'pool_updated' && diff.every(([, change]) => isChange(change))) {
        const before = formatFact(atOf(row), 'time', now);
        const read = diff.filter(([field, change]) => !field.startsWith('keepalive.') && (['boolean', 'number', 'string'].includes(typeof rec(change).from) || rec(change).from === null));
        add('Reading it', ...read.map(([field, change]) => plain(`decisions before ${before} carry ${fieldLabel(field)} ${fieldValue(field, rec(change).from, now)}`)));
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
      const w = window === null ? 'window' : windowTagWord(window);
      // A round written before the projection trigger carries per-second rates in its own unit and a threshold in
      // place of a ceiling: it keeps its conversion and its reading, as the version 1 record keeps its own maps.
      if (d.ceiling === undefined && d.threshold !== undefined) {
        const unit = str(d.unit);
        const perHour = (v: unknown, label?: string) => figure(v, (n) => (unit === 'usd' ? `${fmtUsd(n * 3_600)}/h` : hourlyShare(n * 3_600)), label);
        add('Started by', request, session, agent === null ? null : plain(agent, 'agent'));
        add('Reading', pct(d.utilization, w), pct(d.threshold, 'threshold'));
        add('Unit', text(d.unit));
        add('Burn window', seconds(d.burn_window_seconds));
        add('Share', pct(d.shed_share, 'share'), perHour(d.burn_sum, 'burn'), perHour(d.target, 'to shed'));
        add('Warm sessions', text(d.warm_sessions));
        const legacyPins = list(d.pins).map(rec);
        if (legacyPins.length > 0) {
          pairs.push({
            chips: [],
            label: 'Pins',
            table: {
              head: ['Session', 'Agent', 'Warmth', 'Burn', 'Last prompt', 'Mark'],
              rows: legacyPins.map((p) => {
                const last = num(p.last_input_tokens);
                return [
                  sessionChip(str(p.session_id) ?? ''),
                  plain(str(p.agent_id) ?? '-'),
                  plain(p.cold === true ? 'cold' : 'warm'),
                  perHour(p.burn) ?? plain('-'),
                  plain(last === null ? '-' : fmtCompact(last)),
                  plain(p.marked === true ? 'marked' : (str(p.note) ?? '-')),
                ];
              }),
            },
          });
        }
        add('Marks kept until', time(d.dwell_until));
        add('Dwell', seconds(d.dwell_seconds));
        if (typeof d.persisted === 'boolean') add('Survive a restart', plain(d.persisted ? 'persisted' : 'not persisted'));
        break;
      }
      const at = atOf(row);
      const projection = num(d.projection);
      const horizon = num(d.horizon);
      const ceiling = num(d.ceiling);
      const rate = (v: unknown, label?: string) => figure(v, hourlyShare, label);
      add('Started by', request, session, agent === null ? null : plain(agent, 'agent'));
      // A weekly window's horizon is one 5-hour length ahead, which reads as the span, and the reset reads as its time.
      const when = horizon === null ? '' : horizon - at === 5 * HOUR ? ` in ${span(horizon - at)}` : ` at ${formatFact(horizon, 'time', now)}`;
      const trigger = str(d.trigger);
      const stretch = num(d.stretch_ms);
      const threshold = num(d.threshold_ms);
      const utilization = num(d.utilization);
      const reservation = num(d.fable_reservation);
      const reserved = trigger === 'fable_reservation';
      if (trigger === 'slow_down' && stretch !== null && threshold !== null) {
        // The stretch's round moves its horizon earlier by the threshold and reads the demand, so the reset is the
        // horizon put back and the burn the round weighed is the demand.
        add('Trigger', plain(`${w} stretch ${span(stretch)} over the threshold ${span(threshold)}`));
        const demand = num(d.burn_sum);
        add('Demand', demand === null ? null : plain(`${hourlyShare(demand)}${horizon === null ? '' : ` to the reset at ${formatFact(horizon + threshold, 'time', now)}`}`));
      } else if (reserved) {
        // The reservation's test reads the room the reading leaves, floored at 0 as the router floors it and capped at
        // the whole share, and no projection.
        add('Trigger', utilization === null || reservation === null ? null : plain(`${w} room ${fmtPct(shareLeft(utilization))} under the Fable reservation ${fmtPct(reservation)}`));
      } else {
        // A round that tested the wall records a ceiling of 1, the window's whole share.
        add('Trigger', projection === null || ceiling === null ? null : plain(`${w} projection ${fmtPct(projection)}${when} over ${lineWords(ceiling)}`));
      }
      add('Fired by', trigger === null ? null : plain(`the ${wordOf(LABELS, trigger)}`));
      add('Fable reservation', pct(reservation));
      // The reservation's test names no rate, so the router records the burn of the pins the round walked in its place.
      add(reserved ? 'Walked burn' : 'Excess', rate(d.excess));
      add('Cap', pct(d.shed_cap), rate(d.target, 'target'), rate(d.removed, 'removed'));
      add('Mark share', pct(d.mark_share));
      add('Unit', text(d.unit));
      add('Burn average', seconds(d.burn_average_seconds));
      add('Warm sessions', text(d.warm_sessions));
      const pins = list(d.pins).map(rec);
      const rewriteUnit = str(d.rewrite_unit);
      // A round stored before the router weighed the dropped thinking carries no such key, so it keeps its columns. A
      // null is a pair the round read as kept or learned nothing of, or a pin with no destination, so it draws a dash
      // where a count would claim a figure.
      const thinking = pins.some((p) => 'dropped_thinking_tokens' in p);
      if (pins.length > 0) {
        pairs.push({
          chips: [],
          label: 'Pins',
          table: {
            head: ['Session', 'Agent', 'Warmth', 'Burn', 'Last prompt', ...(thinking ? ['Thinking dropped'] : []), 'Rewrite', 'Destination', 'Mark'],
            rows: pins.map((p) => {
              const id = str(p.session_id) ?? '';
              const b = num(p.burn);
              const last = num(p.last_input_tokens);
              const dropped = num(p.dropped_thinking_tokens);
              const rewrite = num(p.rewrite);
              const rewriteWindow = str(p.rewrite_window);
              const planned = str(p.planned);
              const skip = str(p.skip);
              return [
                sessionChip(id),
                plain(str(p.agent_id) ?? '-'),
                plain(p.cold === true ? 'cold' : 'warm'),
                plain(b === null ? '-' : hourlyShare(b)),
                plain(last === null ? '-' : fmtCompact(last)),
                ...(thinking ? [plain(dropped === null ? '-' : fmtCompact(dropped))] : []),
                rewriteCell(rewrite, rewriteWindow, rewriteUnit, week),
                planned === null ? plain('-') : accountChip(planned),
                plain(p.marked === true ? 'marked' : skip !== null ? wordOf(LABELS, skip) : (str(p.note) ?? '-')),
              ];
            }),
          },
        });
      }
      add('Skipped', ...pins.flatMap((p) => (str(p.skip) === null ? [] : [skipChip(str(p.session_id) ?? '', str(p.skip) ?? '', str(p.session_id) ?? '')])));
      add('Planned destinations', ...[...new Set(pins.flatMap((p) => (str(p.planned) === null ? [] : [str(p.planned) ?? ''])))].map((id) => accountChip(id)));
      const outcome = str(d.outcome);
      const reason = str(d.reason);
      add('Outcome', outcome === null ? null : plain(outcome === 'nothing_to_mark' ? `nothing to mark${reason === null ? '' : `: ${lookup(ROUND_REASON_WORD, reason) ?? wordOf(LABELS, reason)}`}` : words(outcome)));
      add('Next round after', time(d.dwell_until));
      add('Dwell', seconds(d.dwell_seconds));
      if (typeof d.persisted === 'boolean') add('Survive a restart', plain(d.persisted ? 'persisted' : 'not persisted'));
      break;
    }
    case 'mark_cleared': {
      const reason = str(d.reason) ?? '';
      const marked = num(d.marked_at);
      const projection = num(d.projection);
      const markedWindow = str(d.marked_window);
      const stretch = num(d.stretch_ms);
      const threshold = num(d.threshold_ms);
      const limit = num(d.limit);
      // A stored event carries no limit and keeps the words it read.
      const under = limit === null ? 'under the ceiling' : `under ${lineWords(limit)}`;
      pin();
      // A mark heals when the account's projection reads under its line again on the window that fired, or under the
      // slow-down when its stretch reads under the threshold. The threshold is the detail's own, since the pool's
      // setting may have moved since.
      const healed =
        stretch !== null && threshold !== null
          ? `healed: the stretch read ${span(stretch)} under the threshold ${span(threshold)}`
          : projection !== null
            ? `healed: the ${markedWindow === null ? 'window' : windowTagWord(markedWindow)} projection read ${fmtPct(projection)} ${under}`
            : null;
      add('Reason', plain(reason === 'healed' && healed !== null ? healed : words(reason)));
      add('Marked', time(marked), str(d.marked_on) === null ? null : accountChip(str(d.marked_on) ?? '', 'on'));
      add('Standing for', marked === null ? null : plain(span(atOf(row) - marked)));
      break;
    }
    case 'reservation_expired': {
      add('Request', request);
      add('Attempt', count(d.attempt));
      add('Cause', text(d.cause));
      add('Reserved', ...shareChips(rec(d.shares)));
      add('Held for', figure(d.age_ms, span));
      add('Effect', plain("the booked share left the account's prediction"));
      break;
    }
    case 'slow_down_ended': {
      add('Lasted', figure(d.duration_ms, span));
      add('Slowest', figure(d.factor_min, speedWord));
      // Held sums the waits of the span's holds, which can overlap and pass Lasted, so the count says how many it sums.
      add('Held', figure(d.held_ms, span), figure(d.holds, fmtInt, 'holds'));
      add('Effect', plain("requests of the account's sessions pass unheld again"));
      break;
    }
    case 'slow_down_started': {
      const stretch = num(d.stretch_ms);
      const threshold = num(d.threshold_ms);
      add('Speed', figure(d.factor, speedWord));
      add('Stretch', stretch === null || threshold === null ? null : plain(`${span(stretch)} of ${span(threshold)}`));
      add('Demand', figure(d.demand, hourlyShare));
      add('Reset', time(d.reset));
      add('Effect', plain("requests of the account's sessions wait their cycle times the slow-down up to the hold"));
      break;
    }
    case 'stream_interrupted': {
      const code = str(d.code);
      const upstream = str(d.upstream_request_id);
      add('Request', request);
      add('Code', code === null ? null : { mono: true, value: code });
      add('Text', text(d.text));
      add('Bytes relayed', count(d.bytes_relayed));
      // The upstream's id keeps its underscores, which `text` would read as spaces, and prints as the request drawer's.
      add('Upstream request', upstream === null ? null : { mono: true, value: upstream });
      add('Attempt', figure(d.attempt, String));
      break;
    }
    case 'warm_prefix_expired': {
      const deepest = rec(d.deepest);
      add('Boundary', text(deepest.tier));
      add('Depth', count(deepest.depth));
      add('Size', figure(deepest.prefix_tokens, fmtCompact));
      add('Tier', figure(deepest.ttl_ms, ttlWord));
      add('Written', time(deepest.written_at));
      add('Entries', count(d.entries));
      add('Listed', plain('when the kind is chosen'));
      break;
    }
    case 'wind_down_started': {
      add('Subscription ends', time(d.ends_at));
      add('Wind-down', figure(d.wind_down_minutes, (m) => span(m * MINUTE)));
      add('Effect', plain('no fresh session starts on this account'));
      add('Cold pins', plain('drain to a sibling'));
      add('End from', text(d.end_source));
      break;
    }
    // A dropped row's plan and tier price its use's or its plan change's row on the allowance read and draw no row
    // here, as on a reset.
    case 'window_cleared':
    case 'window_dropped':
      add('Window', window === null ? null : plain(windowTagWord(window)));
      add('Before', pct(d.utilization_before));
      add('After', pct(d.utilization));
      add('Reset', time(d.reset));
      if (d.pair_changed === true) add('Plan changed', plain('yes'));
      if (row.kind === 'window_cleared') add('Read from', text(d.source, SOURCE_WORD));
      break;
    case 'window_reset':
      add('Window', window === null ? null : plain(windowTagWord(window)));
      add('Before the reset', pct(d.utilization_before));
      add('Read from', text(d.observed, SOURCE_WORD));
      add('Prediction', plain('0% until the next head'));
      add('Next reset', time(d.next_reset));
      break;
    case 'window_spent':
      add('Window', window === null ? null : plain(windowTagWord(window)));
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
  /** The row's full chip list behind a control: `chips` is the compact row, `all` the full row, `count` the entries. */
  more?: { all: Chip[]; count: number };
  title?: string;
}
/** The compact row draws from this many entries, the latest three kept as chips. */
export const CACHE_MAP_COMPACT_FROM = 4;

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
    { chips: [r.sessionId === null ? plain('none') : sessionChip(r.sessionId)], label: 'Session' },
    { chips: [plain(r.agentId)], label: 'Agent' },
    { chips: [plain(r.requestClass ?? words(r.clientClass))], label: 'Class' },
    { chips: [modelChip(r.canonicalModel)], label: 'Model resolved' },
    ...(upstream !== null && upstream !== r.canonicalModel ? [{ chips: [modelChip(upstream)], label: 'Upstream model' }] : []),
    { chips: laneChipsOf(r.lane), label: 'Lane' },
    { chips: flags.length > 0 ? flags : [plain('none')], label: 'Flags' },
    ...(r.attempted.length > 0 ? [{ chips: r.attempted.map((id) => accountChip(id)), label: 'Attempted' }] : []),
  ];
}

/**
 * The account a walk fact names: its value where the step wrote it about that same account, as the pin step writes a
 * credit's issuer and a judged turn's account, else null.
 */
function namedAccount(f: WalkFact): string | null {
  return typeof f.value === 'string' && f.accountId === f.value ? f.value : null;
}

/**
 * The Pin fold of a classifier call, read off its `pin` step's facts: the call holds no pin and follows the turn it
 * judges. A call a fallback credit directs carries the issuer in its `credit` fact and no `direction`, so its "Sent
 * to" row names the issuer. A fact the step lacks draws no row. An account reads by the fact's own account, as the
 * strip reads it, and a fact that names none reads the word `none`.
 */
function classifierPinRows(record: DecisionRecord): CardRow[] {
  const facts = record.walk.find((s) => s.step === 'pin')?.facts ?? [];
  const factOf = (key: string) => facts.find((f) => f.key === key);
  const text = (key: string): string | null => str(factOf(key)?.value);
  const account = (f: WalkFact, label?: string): Chip => {
    const id = namedAccount(f);
    return id === null ? plain('none', label) : accountChip(id, label);
  };
  const [agent, model, direction] = [text('judged_agent'), text('judged_model'), text('direction')];
  const [judged, credit] = [factOf('judged'), factOf('credit')];
  const sentTo = direction !== null ? [plain(lookup(DIRECTION_WORDS, direction) ?? direction)] : credit !== undefined ? [account(credit, keyWord('credit'))] : null;
  return [
    { chips: [plain(PIN_REASON_LABEL.classifier)], label: 'Pin' },
    ...(agent === null ? [] : [{ chips: [plain(agent), ...(model === null ? [] : [modelChip(model)])], label: 'Judged turn' }]),
    ...(judged === undefined ? [] : [{ chips: [account(judged)], label: 'Judged account' }]),
    ...(sentTo === null ? [] : [{ chips: sentTo, label: 'Sent to' }]),
  ];
}

/**
 * The Pin and warmth fold. A record that pins nothing reads one row, Pin, with why, and a classifier call's fold goes
 * on to the turn it judged and where the call went.
 */
export function pinRows(record: DecisionRecord, now: number): CardRow[] {
  const p = record.pin;
  if (p === null && record.pinReason === 'classifier') return classifierPinRows(record);
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
  // The cycle a slow-down hold is sized from, and the deadline a second request of the pin joins.
  if (p.cycleMs !== null) rows.push({ chips: [plain(span(p.cycleMs))], label: 'Cycle' });
  if (p.slowDownUntil !== null) rows.push({ chips: [plain(formatFact(p.slowDownUntil, 'time', now))], label: 'Slowed until' });
  if (p.marked || p.shed !== null) {
    // A version 1 record carried the mark alone, with no account, instant or window.
    const trigger = p.markedWindow === null ? [] : [plain(triggerPhrase(p.markedWindow))];
    const marked = p.markedOn !== null && p.markedAt !== null ? [accountChip(p.markedOn, 'marked on'), plain(formatFact(p.markedAt, 'time', now), 'at'), ...trigger] : [plain('marked by a round')];
    rows.push({ chips: [...(p.marked ? marked : []), ...(p.shed !== null ? [accountChip(p.shed.from, shedOrigin(p.shed.reason)), plain(formatFact(p.shed.at, 'time', now), 'at')] : [])], label: 'Shed mark' });
  }
  return [...rows, ...released];
}

/**
 * The Settings in force grid. A refusal draws none, since no setting entered it. A version 1 record reads the
 * switch and the ceiling as not recorded and its three removed settings after the list. A record stored before the
 * router wrote the Fable reservation option draws no row for it, where `settingValue` would read the null as off.
 */
export function settingsRows(record: DecisionRecord): CardRow[] | null {
  const s = record.settings;
  if (s === null || record.outcome.kind === 'refused') return null;
  const rows = SETTINGS_ROWS.filter((r) => !(UNWRITTEN.includes(r.key) && s[r.key] === null)).map((r) => ({ chips: [plain(UNRECORDED.includes(r.key) && s[r.key] === null ? 'not recorded' : settingValue(r.key, s))], label: r.label }));
  const share = (v: number | null) => (v === null ? 'off' : fmtPct(v));
  const legacy = s.legacy === null ? [] : [
    { chips: [plain(share(s.legacy.shedAheadOfPace))], label: 'Shed above (then)' },
    { chips: [plain(share(s.legacy.selectBelowUtilization))], label: 'Select below (then)' },
    { chips: [plain(fmtPct(s.legacy.paceFloor))], label: 'Pace floor (then)' },
  ];
  return [...rows, ...legacy];
}
