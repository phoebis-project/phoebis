import type { AccountPatch, KeyInput, PoolEngineFields, PoolInput } from '@/api/client';
import { iso } from '@/api/decode';
import { slug } from '@/api/http';
import type { Account, ApiKey, GuardrailRef, ModelRule, ModelRulesMode, Pool, PoolKeepalive, PoolWarning } from '@/api/types';
import { AFFINITY_MODE_VALUES, RETURN_TO_PLAN_VALUES, SESSION_PLACEMENT_VALUES, SUBAGENT_PLACEMENT_VALUES, WARM_SESSIONS_VALUES } from '@/api/types';
import { byteOrder, fmtInt, toLocalInput, viewingZone } from './format';
import { checkPattern } from './patterns';
import { canceledEndOf } from './quota';
import { DAY } from './time';

export type NumericKeepaliveKey = { [K in keyof PoolKeepalive]-?: PoolKeepalive[K] extends number | null ? K : never }[keyof PoolKeepalive];

/**
 * The router's inclusive bound on each numeric keepalive field. A null ceiling is the recipe's break-even and has
 * none. The form holds a save to them so the operator sees the field that is out of range rather than the policy
 * check's refusal of the whole save.
 */
export const KEEPALIVE_BOUNDS: Record<NumericKeepaliveKey, { max: number; min: number }> = {
  maxIdleSeconds: { max: 31_536_000, min: 0 },
  maxParkedSeconds: { max: 604_800, min: 60 },
  scheduledHorizonSeconds: { max: 604_800, min: 60 },
};

/** The most entries one level's list holds, the route's own bound. */
const MAX_MODEL_RULES = 256;

/**
 * What the pool and key routes would answer a model rule list with, or null: the entry bound first, then the
 * first entry whose pattern `checkPattern` refuses, named by its index as the route names it.
 */
export function modelRulesRefusal(rules: ModelRule[]): string | null {
  if (rules.length > MAX_MODEL_RULES) return `model_rules holds at most ${MAX_MODEL_RULES} entries`;
  for (const [i, rule] of rules.entries()) {
    const refusal = checkPattern(rule.pattern);
    if (refusal) return `model_rules[${i}]: ${refusal}`;
  }
  return null;
}

type WholeEngineKey = { [K in keyof PoolEngineFields]-?: PoolEngineFields[K] extends number ? K : never }[keyof PoolEngineFields];

/** The pool routes' inclusive bound on each whole-number engine field. The cap and the ceiling are fractions. */
export const ENGINE_BOUNDS: Record<Exclude<WholeEngineKey, 'shedCap' | 'windowCeiling'>, { max: number; min: number }> = {
  burnAverageSeconds: { max: 7_200, min: 600 },
  inFlightCap: { max: 4_294_967_295, min: 1 },
  overageMoveGraceMinutes: { max: 300, min: 0 },
  retryInPlaceSeconds: { max: 300, min: 0 },
  returnToPlanDwellSeconds: { max: 86_400, min: 0 },
  shedDwellSeconds: { max: 7_200, min: 300 },
  slowDownHoldSeconds: { max: 300, min: 1 },
  slowDownStretchSeconds: { max: 18_000, min: 60 },
  windDownMinutes: { max: 10_080, min: 0 },
};

const whole = (v: number, min: number, max: number) => Number.isInteger(v) && v >= min && v <= max;
const ratio = (v: number, min: number) => v >= min && v <= 1;

/** A number field's value: null where it reads empty, NaN where the browser could not parse what was typed. */
export const typedNumber = (typed: string, badInput: boolean): number | null => (badInput ? Number.NaN : typed.trim() === '' ? null : Number(typed));

/** Why the member write would refuse a budget as it stands, or null. A null budget is none. */
function budgetRefusal(value: number | null): string | null {
  if (value === null) return null;
  return Number.isFinite(value) && value > 0 ? null : 'A budget is a dollar amount above 0. Leave it empty for no budget.';
}

/** Why the pool routes would refuse the prefix threshold as it stands, or null. */
function prefixThresholdRefusal(value: number): string | null {
  return whole(value, 0, Number.MAX_SAFE_INTEGER) ? null : 'The minimum shared prefix is a whole number of tokens, 0 or more.';
}

/** The pool dialog's groups in rail order; a refusal names the one its control sits in. */
export const POOL_GROUPS = ['identity', 'members', 'keys', 'models', 'guardrails', 'autoMode', 'selection', 'placement', 'pinning', 'utilization', 'shedding', 'slowDown', 'failover', 'overage', 'cacheTtl', 'keepalive', 'bodyCapture', 'cacheDiagnostics'] as const;
export type PoolGroup = (typeof POOL_GROUPS)[number];

/** A member row's refused control. */
export type MemberColumn = 'budget_daily' | 'budget_monthly' | 'member' | 'priority';
/** A member row's control as a refusal's `field` names it, so the dialog's lookups and the refusals agree. */
export const memberField = (column: MemberColumn, accountId: string): string => `${column}:${accountId}`;

export interface PoolRefusal {
  /** The refused control's accessible name; `priority:<id>`, `budget_daily:<id>`, `budget_monthly:<id>`, `member:<id>` for a member row; `members` for the missing member or an account list that did not load. */
  field: string;
  group: PoolGroup;
  /** The member row and the control a member row's refusal names. */
  member?: { accountId: string; column: MemberColumn };
  /** The words the control's callout and the footer read. */
  text: string;
}

/** The labels the dialog draws over each bounded engine field, as the refusals name them. */
export const ENGINE_FIELD_LABELS: Record<keyof typeof ENGINE_BOUNDS, string> = {
  burnAverageSeconds: 'Burn average (s)',
  inFlightCap: 'In flight',
  overageMoveGraceMinutes: 'Overage grace (min)',
  retryInPlaceSeconds: 'Retry in place (s)',
  returnToPlanDwellSeconds: 'Return dwell (s)',
  shedDwellSeconds: 'Shed dwell (s)',
  slowDownHoldSeconds: 'Hold (s)',
  slowDownStretchSeconds: 'Stretch (s)',
  windDownMinutes: 'Wind down (min)',
};

/** The labels the dialog draws over each numeric keepalive field, as the refusals name them. */
export const KEEPALIVE_FIELD_LABELS: Record<NumericKeepaliveKey, string> = {
  maxIdleSeconds: 'Max idle (s)',
  maxParkedSeconds: 'Max parked (s)',
  scheduledHorizonSeconds: 'Scheduled horizon (s)',
};

/** A bound's sentence: a field with no practical ceiling states its floor alone. */
const boundText = (label: string, { max, min }: { max: number; min: number }) =>
  max === Number.MAX_SAFE_INTEGER ? `${label} is a whole number of at least ${fmtInt(min)}.` : `${label} is a whole number from ${fmtInt(min)} to ${fmtInt(max)}.`;
const listed = (label: string) => `${label} is one of its listed choices.`;

/** What the pool dialog knows beyond the input it sends. */
export interface PoolChecks {
  /** The input sends 0 for an emptied retention as for capture off, so the field is read as typed while capture is on. */
  capture?: {
    /** The bootstrap's limit, absent until the settings read answers and after it fails, when the floor of 1 alone is checked. */
    maxHours?: number;
    on: boolean;
    typed: string;
  };
  /** On create the id derives from the name, so a taken one is held before the route's 409; a rename keeps its id. */
  identity?: { isNew: boolean; poolIds: ReadonlySet<string> };
}

/**
 * The id a create derives from a name, and whether the name gave it nothing: `slug` keeps ASCII letters and digits
 * alone, so a name without one falls back to the id `item`, and only a name without a letter or digit in any script
 * is no name.
 */
export function derivedId(name: string): { id: string; unnamed: boolean } {
  const id = slug(name);
  return { id, unnamed: id === 'item' && !/[\p{L}\p{N}]/u.test(name) };
}

/** `value` with every object's keys in sorted order, so a discard question's compare reads values alone. */
export const sortKeys = (value: unknown): unknown =>
  Array.isArray(value) ? value.map(sortKeys) : value !== null && typeof value === 'object' ? Object.fromEntries(Object.keys(value as object).sort().map((k) => [k, sortKeys((value as Record<string, unknown>)[k])])) : value;
/** The discard question's compare: the two values alike, whatever order their keys were written in. */
export const sameValues = (a: unknown, b: unknown): boolean => JSON.stringify(sortKeys(a)) === JSON.stringify(sortKeys(b));

/** A new pool's or key's name whose derived id is no name or one `taken` holds, as the sentence that holds the save. */
const derivedIdRefusal = (noun: 'key' | 'pool', name: string, taken: ReadonlySet<string>): string | null => {
  const { id, unnamed } = derivedId(name);
  if (unnamed) return `The ${noun} needs a name with a letter or a digit.`;
  return taken.has(id) ? `Another ${noun} already has the id ${id}.` : null;
};

/** Typed capture hours outside 1 to `maxHours`, or under 1 with no ceiling, as the sentence `subject` leads. */
const hoursRefusal = (subject: string, typed: string, maxHours: number | undefined): string | null => {
  if (whole(Number(typed), 1, maxHours ?? Number.MAX_SAFE_INTEGER)) return null;
  return maxHours === undefined ? `${subject} of at least 1.` : `${subject} from 1 to ${maxHours}.`;
};

/**
 * Every check of the pool routes the input fails, in rail order and in each group's order below; empty when the routes
 * would take it. `accountIds` null means the account list did not load: one refusal holds the save, and no member is
 * refused for an id the list might hold.
 */
export function poolRefusals(input: PoolInput, accountIds: ReadonlySet<string> | null, checks: PoolChecks = {}): PoolRefusal[] {
  const out: PoolRefusal[] = [];
  const refuse = (group: PoolGroup, field: string, text: string | null, member?: PoolRefusal['member']) => {
    if (text !== null) out.push(member === undefined ? { field, group, text } : { field, group, member, text });
  };
  const k = input.keepalive;
  const affinity = input.affinityTtlSeconds;

  refuse('identity', 'Name', input.name.length > 0 ? null : 'the pool needs a name.');
  if (checks.identity?.isNew && input.name.length > 0) refuse('identity', 'Name', derivedIdRefusal('pool', input.name, checks.identity.poolIds));

  refuse('members', 'members', accountIds !== null ? null : 'the account list did not load, so the members cannot be checked.');
  refuse('members', 'members', input.members.length > 0 ? null : 'the pool needs a member.');
  for (const m of input.members) {
    const id = m.accountId;
    const refuseMember = (column: MemberColumn, text: string | null) => refuse('members', memberField(column, id), text, { accountId: id, column });
    if (accountIds !== null) refuseMember('member', accountIds.has(id) ? null : `${id} is not an account the console lists.`);
    refuseMember('priority', whole(m.priority, 0, Number.MAX_SAFE_INTEGER) ? null : 'Priority is a whole number of 0 or more.');
    refuseMember('budget_daily', budgetRefusal(m.budgetDailyUsd));
    refuseMember('budget_monthly', budgetRefusal(m.budgetMonthlyUsd));
  }

  refuse('models', 'Model rules', modelRulesRefusal(input.modelRules));

  const choice = (group: PoolGroup, label: string, values: readonly string[], value: string) => refuse(group, label, values.some((v) => v === value) ? null : listed(label));
  choice('placement', 'Subagent placement', SUBAGENT_PLACEMENT_VALUES, input.subagentPlacement);
  choice('placement', 'Session placement', SESSION_PLACEMENT_VALUES, input.sessionPlacement);
  refuse('placement', 'Minimum shared prefix, tokens', prefixThresholdRefusal(input.minSharedPrefixTokens));

  refuse('pinning', 'Session affinity in seconds', whole(affinity, 0, Number.MAX_SAFE_INTEGER) && (affinity === 0 || affinity >= 60) ? null : 'Session affinity is off or at least 60 whole seconds.');
  choice('pinning', 'Pin sessions', AFFINITY_MODE_VALUES, input.affinityMode);

  // NaN is a field left empty, which fails the range and so holds the save.
  refuse('utilization', 'Window ceiling (%)', ratio(input.windowCeiling, 0.5) ? null : 'Window ceiling (%) is from 50 to 100.');

  const engine = (group: PoolGroup, key: keyof typeof ENGINE_BOUNDS) =>
    refuse(group, ENGINE_FIELD_LABELS[key], whole(input[key], ENGINE_BOUNDS[key].min, ENGINE_BOUNDS[key].max) ? null : boundText(ENGINE_FIELD_LABELS[key], ENGINE_BOUNDS[key]));
  engine('utilization', 'burnAverageSeconds');

  refuse('shedding', 'Shed cap (%)', ratio(input.shedCap, 0.1) ? null : 'Shed cap (%) is from 10 to 100.');
  engine('shedding', 'shedDwellSeconds');
  choice('shedding', 'Warm sessions', WARM_SESSIONS_VALUES, input.warmSessions);

  engine('slowDown', 'slowDownStretchSeconds');
  engine('slowDown', 'slowDownHoldSeconds');

  engine('failover', 'inFlightCap');
  engine('failover', 'retryInPlaceSeconds');

  engine('overage', 'overageMoveGraceMinutes');
  choice('overage', 'Return to plan', RETURN_TO_PLAN_VALUES, input.returnToPlan);
  engine('overage', 'returnToPlanDwellSeconds');
  engine('overage', 'windDownMinutes');

  refuse('keepalive', 'Keep prompt caches warm', !k.enabled || affinity >= 300 ? null : 'Keep prompt caches warm needs Session affinity of at least 5m.');
  for (const key of ['maxIdleSeconds', 'maxParkedSeconds', 'scheduledHorizonSeconds'] as const) {
    const value = k[key];
    refuse('keepalive', KEEPALIVE_FIELD_LABELS[key], value === null || whole(value, KEEPALIVE_BOUNDS[key].min, KEEPALIVE_BOUNDS[key].max) ? null : boundText(KEEPALIVE_FIELD_LABELS[key], KEEPALIVE_BOUNDS[key]));
  }

  if (checks.capture?.on) refuse('bodyCapture', 'Capture retention in hours', hoursRefusal('Retention is a whole number of hours', checks.capture.typed, checks.capture.maxHours));
  return out;
}

/** The settings the pool routes' static checks read, on a saved pool or a dialog's input alike. */
export type WarnedSettings = Pick<
  Pool,
  | 'affinityTtlSeconds'
  | 'cacheTtl'
  | 'inFlightCap'
  | 'loadShedding'
  | 'migrateWhenSpent'
  | 'returnToPlan'
  | 'returnToPlanDwellSeconds'
  | 'shedCap'
  | 'slowDown'
  | 'slowDownHoldSeconds'
  | 'warmSessions'
  | 'windowCeiling'
>;

const TTL_SECONDS = { '1h': 3_600, '5m': 300 } as const;

/**
 * The pool routes' ten static checks, the mirror of the route's own as `ENGINE_BOUNDS` mirrors its bounds: each with
 * the fields it reads and its sentence, sorted by code. `cohort` is the most agents of one session among the pins
 * under the pool's keys. A warning blocks nothing.
 */
export function poolWarnings(pool: WarnedSettings, cohort: number): PoolWarning[] {
  const out: PoolWarning[] = [];
  const warn = (code: string, fields: string[], text: string) => out.push({ code, fields, text });
  const ttl = pool.cacheTtl;
  // The route reads the policy's four main and subagent TTLs, and an injection switch on a policy that is off injects
  // nothing.
  const longest = Math.max(...[ttl.plan.main, ttl.plan.subagent, ttl.metered.main, ttl.metered.subagent].map((t) => TTL_SECONDS[t]));
  if (ttl.enabled && ttl.injectWhenAbsent && pool.affinityTtlSeconds < longest) {
    warn('affinity_under_cache_ttl', ['affinity_ttl_seconds', 'cache_ttl'], 'The session affinity ends before the cache TTL the pool injects, so a pin can expire while its cache is warm and the next request can land cold elsewhere.');
  }
  if (pool.inFlightCap < cohort) {
    warn('cap_under_cohort', ['in_flight_cap'], `The in-flight cap is under the largest live cohort of ${cohort} agents, so subagents of one session wait for a slot they share.`);
  }
  if (pool.shedCap < 0.25) {
    warn('cap_under_quarter', ['shed_cap'], 'A shed cap under 25 percent removes at most a quarter of the burn per round, so a window far over the ceiling needs several rounds and dwells to come under it.');
  }
  if (pool.windowCeiling < 0.8) {
    warn('ceiling_low', ['window_ceiling'], 'A ceiling under 80 percent leaves a fifth of every window unused and moves sessions that would have finished the window warm.');
  }
  if (pool.windowCeiling >= 1) {
    warn('ceiling_without_margin', ['window_ceiling'], 'A ceiling at 100 percent leaves no margin for a stale reading or a large turn, so a session can reach the wall before the round or the gate reads it.');
  }
  if (pool.slowDownHoldSeconds > 150) {
    warn('hold_over_window', ['slow_down_hold_seconds'], "A hold over 150 seconds leaves under 30 seconds of the client's first-byte window for the upstream's head, so a slowed request can time out at the client and be sent again.");
  }
  if (pool.returnToPlan === 'always' && pool.returnToPlanDwellSeconds === 0) {
    warn('return_at_once', ['return_to_plan_dwell_seconds'], "A return dwell of zero returns every warm session on its first request at the metered member, so the member serves one turn per session and the move's rewrite is paid each time.");
  }
  // With the slow-down on, a session over the ceiling is slowed toward the reset rather than left to spend its window.
  if (!pool.loadShedding && !pool.slowDown) {
    warn('shedding_off', ['load_shedding', 'slow_down'], 'With load shedding off no round marks sessions on a member whose window would pass the ceiling, so a session stays until its window is spent or its account refuses it.');
  }
  if (!pool.migrateWhenSpent) {
    warn('spent_moves_off', ['migrate_when_spent'], 'With spent moves off a session stays on a spent account until the upstream refuses it, so the destination gate never runs for that move.');
  }
  if (pool.warmSessions === 'never') {
    warn('warm_never', ['warm_sessions'], 'With warm sessions never a round stops after the cold sessions, so the excess is covered only while cold sessions carry it.');
  }
  return out.sort((a, b) => byteOrder(a.code, b.code));
}

export type AutoModeOverride = 'inherit' | 'off' | 'on';

export type CaptureMode = 'inherit' | 'off' | 'override';

/** The key dialog's fields as the operator leaves them. */
export interface KeyForm {
  autoModeServer: AutoModeOverride;
  /** The override field's text, kept as typed so an emptied field reads empty rather than 0. */
  captureHours: string;
  captureMode: CaptureMode;
  enabled: boolean;
  /** `keep` leaves the stored expiry alone, `never` clears it, and a number is days from the save. */
  expiry: string;
  /** The key's own guardrail ids in the order they run, after its pool's. */
  guardrails: string[];
  longContextTwins: boolean;
  modelRules: ModelRule[];
  modelRulesMode: ModelRulesMode;
  name: string;
  notes: string;
  optOut: boolean;
  poolId: string;
}

/** A pool's or a key's attached guardrail ids in the order the subject runs them. */
export const guardrailIdsOf = (refs: readonly Pick<GuardrailRef, 'id' | 'position'>[]): string[] => [...refs].sort((a, b) => a.position - b.position).map((r) => r.id);

/**
 * The `guardrails` member an edit sends: the whole list where it differs from the one the dialog opened with, order
 * included, else nothing, so an untouched list leaves in place an attachment made meanwhile from the guardrail's side,
 * which moves no revision of the pool or the key.
 */
export const guardrailsChange = (list: readonly string[], opened: readonly string[]): { guardrails?: string[] } =>
  list.length === opened.length && list.every((id, i) => id === opened[i]) ? {} : { guardrails: [...list] };

/**
 * Whether `a` holds exactly the members of `b`, a list without repeats, in any order. Compared by the set's size, since
 * a list that repeats one member and leaves another out is as long as `b`.
 */
export const sameMembers = (a: readonly string[], b: readonly string[]): boolean => new Set(a).size === b.length && a.every((x) => b.includes(x));

/** The dialog's starting fields: the stored key's, or a new key's in `poolId`. */
export function keyFormOf(existing: ApiKey | null, poolId: string): KeyForm {
  return {
    autoModeServer: existing?.autoModeServer == null ? 'inherit' : existing.autoModeServer ? 'on' : 'off',
    captureHours: String(existing?.bodyCaptureHours || 6),
    captureMode: existing ? (existing.bodyCaptureHours === null ? 'inherit' : existing.bodyCaptureHours === 0 ? 'off' : 'override') : 'inherit',
    // The stored switch rather than `status`: every edit sends it, and an expired or revoked key's status would
    // say nothing about it.
    enabled: existing?.enabled ?? true,
    expiry: existing ? 'keep' : 'never',
    guardrails: guardrailIdsOf(existing?.guardrails ?? []),
    longContextTwins: existing?.longContextTwins ?? true,
    modelRules: existing?.modelRules ?? [],
    modelRulesMode: existing?.modelRulesMode ?? 'extend',
    name: existing?.name ?? '',
    notes: existing?.notes ?? '',
    optOut: existing?.keepaliveOptOut ?? false,
    poolId: existing?.poolId ?? poolId,
  };
}

export function keyInputOf(form: KeyForm, now: number): KeyInput & { guardrails?: never } {
  return {
    autoModeServer: form.autoModeServer === 'inherit' ? null : form.autoModeServer === 'on',
    bodyCaptureHours: form.captureMode === 'inherit' ? null : form.captureMode === 'off' ? 0 : Number(form.captureHours),
    enabled: form.enabled,
    expiresAt: form.expiry === 'keep' ? undefined : form.expiry === 'never' ? null : iso(now + Number(form.expiry) * DAY),
    keepaliveOptOut: form.optOut,
    longContextTwins: form.longContextTwins,
    modelRules: form.modelRules,
    modelRulesMode: form.modelRulesMode,
    name: form.name.trim(),
    notes: form.notes.trim() || null,
    poolId: form.poolId,
  };
}

/** The key dialog's groups in rail order; a refusal names the one its control sits in. */
export const KEY_GROUPS = ['key', 'access', 'models', 'guardrails', 'sessions', 'capture'] as const;
export type KeyGroup = (typeof KEY_GROUPS)[number];

export interface KeyRefusal {
  /** The refused control's accessible name: Name, Pool, Model rules, Capture hours for this key. */
  field: string;
  group: KeyGroup;
  text: string;
}

/** What the key dialog knows beyond the fields it sends. */
export interface KeyChecks {
  /** On create the id derives from the name, so a taken one is held before the route's 409; an edit keeps its id. */
  identity?: { existingIds: ReadonlySet<string>; isNew: boolean };
  /** The bootstrap's ceiling on capture hours; without it only the floor of 1 is checked. */
  maxHours?: number;
}

/**
 * The key dialog's holds in rail order: a missing name, a new key's name whose id is taken or that holds no ASCII
 * letter or digit, a missing pool, the model rules, an override's hours; empty when Save may go.
 */
export function keyRefusals(form: Pick<KeyForm, 'captureHours' | 'captureMode' | 'modelRules' | 'name' | 'poolId'>, checks: KeyChecks = {}): KeyRefusal[] {
  const out: KeyRefusal[] = [];
  // Trimmed, as `keyInputOf` sends it: a name of spaces reaches the route empty.
  const name = form.name.trim();
  if (!name) out.push({ field: 'Name', group: 'key', text: 'the key needs a name.' });
  if (checks.identity?.isNew && name) {
    const text = derivedIdRefusal('key', name, checks.identity.existingIds);
    if (text) out.push({ field: 'Name', group: 'key', text });
  }
  if (!form.poolId) out.push({ field: 'Pool', group: 'key', text: 'the key needs a pool.' });
  const rules = modelRulesRefusal(form.modelRules);
  if (rules) out.push({ field: 'Model rules', group: 'models', text: rules });
  // An emptied field would send 0, which the route stores as capture off while the radio still says Override.
  if (form.captureMode === 'override') {
    const text = hoursRefusal('Capture hours is a whole number', form.captureHours, checks.maxHours);
    if (text) out.push({ field: 'Capture hours for this key', group: 'capture', text });
  }
  return out;
}

/** A grouped dialog's refusals as its controls read them: the first, a field's mark and text, and the refused groups. */
export function refusalsView<R extends { field: string; group: string; text: string }>(
  refusals: readonly R[],
): { first: R | null; invalid: (field: string) => true | undefined; refusalOf: (field: string) => string | null; refusedFields: ReadonlySet<string>; refusedGroups: Set<string> } {
  const refusedFields = new Set(refusals.map((r) => r.field));
  return {
    first: refusals[0] ?? null,
    invalid: (field) => (refusedFields.has(field) ? true : undefined),
    refusalOf: (field) => refusals.find((r) => r.field === field)?.text ?? null,
    refusedFields,
    refusedGroups: new Set<string>(refusals.map((r) => r.group)),
  };
}

/**
 * A grouped dialog's status line, the group its Save holds on, and whether it holds. A row that moved under the dialog
 * comes first, since only Reload clears it, then a refusal of the form's own, which beats a server message left from
 * before the edit. `subject` names the row in the stale sentence, and a create passes null, which skips it.
 */
export function saveStatus<G extends string>({ error, first, stale, subject }: { error: string | null; first: { group: G; text: string } | null; stale: string | null; subject: string | null }): { held: boolean; hold: G | null; status: string } {
  return {
    held: first !== null || error !== null || stale !== null,
    hold: stale ? null : (first?.group ?? null),
    status: stale && subject ? `Save is held: ${subject} changed since you opened it. Reload to edit the current version.` : first ? `Save is held: ${first.text}` : (error ?? 'Every group saves together.'),
  };
}

/** The identity card's subscription end as the operator leaves it; `endsAt` is a `datetime-local` value, empty for none. */
export interface SubscriptionEndForm {
  canceled: boolean;
  endsAt: string;
  /** The revision of the row the editor opened with, which the save sends as its fence. */
  revision: number;
}

export function subscriptionEndFormOf(
  account: Pick<Account, 'canceled' | 'revision' | 'subscription'>,
): SubscriptionEndForm {
  const end = canceledEndOf(account);
  return { canceled: account.canceled, endsAt: end ? toLocalInput(Date.parse(end)) : '', revision: account.revision };
}

/** The viewer's zone as the end editor names it beside its label, `Ends (Europe/Berlin)`. */
export const endZoneLabel = (zone = viewingZone()): string => `Ends (${zone})`;

/**
 * Ticking leaves the field as it stands, since the end is the date the provider's billing page shows and nothing the
 * console can work out. Unticking clears it, since the route keeps no end without the flag.
 */
export function tickCanceled(form: SubscriptionEndForm, canceled: boolean): SubscriptionEndForm {
  if (!canceled) return { ...form, canceled: false, endsAt: '' };
  return { ...form, canceled: true };
}

/** Why the account route would refuse the pair, or null. */
export function subscriptionEndRefusal(form: SubscriptionEndForm): string | null {
  if (form.canceled && form.endsAt === '') return 'A canceled plan needs the date it ends.';
  if (!form.canceled && form.endsAt !== '') return 'An end is saved only with Canceled ticked.';
  return null;
}

export function accountPatchOf(form: SubscriptionEndForm): AccountPatch {
  return { canceled: form.canceled, subscriptionEndsAt: form.endsAt === '' ? null : new Date(form.endsAt).toISOString() };
}

/** An account's edit token carries its revision in the low 32 bits and its incarnation above them. */
const ACCOUNT_INCARNATION = 2 ** 32;

/** Whether the editor holding `other` advances to `answered` after a save that sent `sent`: `other` equals `sent`
 *  (both editors opened on the same row state) and `answered` is the token an operator's own write produces from
 *  it, the same incarnation and the revision one higher. An editor whose token differs keeps it, since the row it
 *  opened on is not the row the save moved, and its own save meets the fence as before. */
export const advancesOwnToken = (sent: number, answered: number, other: number): boolean =>
  other === sent && answered === sent + 1 && Math.floor(answered / ACCOUNT_INCARNATION) === Math.floor(sent / ACCOUNT_INCARNATION);
