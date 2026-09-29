import type { AccountPatch, KeyInput, PoolEngineFields, PoolInput } from '@/api/client';
import { AFFINITY_MODE_VALUES, BUDGET_PERIOD_VALUES, QUOTA_HEADERS_VALUES, SESSION_PLACEMENT_VALUES, SUBAGENT_PLACEMENT_VALUES, WARM_SESSIONS_VALUES } from '@/api/decode';
import type { Account, AffinitySource, ApiKey, ModelRule, ModelRulesMode, PoolKeepalive } from '@/api/types';
import { fmtInt, toLocalInput } from './format';
import { checkPattern } from './patterns';
import { MIGRATE_BAND } from './presets';

export type NumericKeepaliveKey = { [K in keyof PoolKeepalive]-?: PoolKeepalive[K] extends number ? K : never }[keyof PoolKeepalive];

/**
 * The router's inclusive bound on each numeric keepalive field. The form holds a save to them so the operator
 * sees the field that is out of range rather than the policy check's refusal of the whole save.
 */
export const KEEPALIVE_BOUNDS: Record<NumericKeepaliveKey, { max: number; min: number }> = {
  coldProbeLimit: { max: 20, min: 1 },
  jitterSeconds1h: { max: 3_599, min: 0 },
  jitterSeconds5m: { max: 299, min: 0 },
  leadSeconds1h: { max: 3_599, min: 0 },
  leadSeconds5m: { max: 299, min: 0 },
  maxIdleSeconds: { max: 31_536_000, min: 0 },
  maxIdleTicks: { max: 1_000_000, min: 0 },
  netNegativeGraceProbes: { max: 100, min: 0 },
  retryMaxSeconds: { max: 604_800, min: 1 },
  retrySeconds: { max: 3_600, min: 1 },
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

/** The pool routes' inclusive bound on each whole-number engine field; the two shares are ratios from 0 to 1. */
export const ENGINE_BOUNDS: Record<Exclude<WholeEngineKey, 'paceFloor' | 'shedShare'>, { max: number; min: number }> = {
  burnWindowSeconds: { max: Number.MAX_SAFE_INTEGER, min: 60 },
  inFlightCap: { max: 4_294_967_295, min: 1 },
  overageMoveGraceMinutes: { max: 300, min: 0 },
  retryInPlaceSeconds: { max: 300, min: 0 },
  sourceDwellSeconds: { max: Number.MAX_SAFE_INTEGER, min: 60 },
  windDownMinutes: { max: 10_080, min: 0 },
};

const whole = (v: number, min: number, max: number) => Number.isInteger(v) && v >= min && v <= max;
const ratio = (v: number | null, min: number) => v === null || (v >= min && v <= 1);

/**
 * Why the two thresholds cannot stand together, or null. A shed session lands only on a member under migrate
 * above minus the band, so a select-below inside the band would prefer members a shed could never reach. The
 * route checks each threshold alone; the band is the form's to hold.
 */
export function bandRefusal(input: Pick<PoolInput, 'migrateAboveUtilization' | 'selectBelowUtilization'>): string | null {
  const { migrateAboveUtilization: migrate, selectBelowUtilization: select } = input;
  // The tolerance keeps a percent the form divided by 100 on the edge of the band rather than a rounding under it.
  if (migrate === null || select === null || select <= migrate - MIGRATE_BAND + 1e-9) return null;
  return `Select below must sit at or under migrate above minus ${Math.round(MIGRATE_BAND * 100)}%, the band a shed session lands under.`;
}

/** Why the member write would refuse a budget field as it stands, or null; empty is no budget. */
export function budgetRefusal(value: string): string | null {
  if (value.trim() === '') return null;
  const n = Number(value);
  return Number.isFinite(n) && n > 0 ? null : 'A budget is a dollar amount above 0; leave it empty for no budget.';
}

/** Why the pool routes would refuse the prefix threshold field as it stands, or null. */
export function prefixThresholdRefusal(value: string): string | null {
  return value.trim() !== '' && whole(Number(value), 0, Number.MAX_SAFE_INTEGER) ? null : 'The minimum shared prefix is a whole number of tokens, 0 or more.';
}

/** The pool dialog's groups in rail order; a refusal names the one its control sits in. */
export const POOL_GROUPS = ['identity', 'members', 'keys', 'models', 'selection', 'placement', 'pinning', 'utilization', 'shedding', 'failover', 'overage', 'cacheTtl', 'keepalive', 'probeTuning', 'responseHeaders', 'bodyCapture', 'cacheDiagnostics'] as const;
export type PoolGroup = (typeof POOL_GROUPS)[number];

export interface PoolRefusal {
  /** The refused control's accessible name; `priority:<id>`, `budget:<id>`, `period:<id>`, `member:<id>` for a member row; `members` for the missing member. */
  field: string;
  group: PoolGroup;
  /** The words the control's callout and the footer read. */
  text: string;
}

/** The labels the dialog draws over each bounded engine field, as the refusals name them. */
export const ENGINE_FIELD_LABELS: Record<keyof typeof ENGINE_BOUNDS, string> = {
  burnWindowSeconds: 'Burn window (s)',
  inFlightCap: 'In flight',
  overageMoveGraceMinutes: 'Overage grace (min)',
  retryInPlaceSeconds: 'Retry in place (s)',
  sourceDwellSeconds: 'Source dwell (s)',
  windDownMinutes: 'Wind down (min)',
};

/** The labels the dialog draws over each numeric keepalive field, as the refusals name them. */
export const KEEPALIVE_FIELD_LABELS: Record<NumericKeepaliveKey, string> = {
  coldProbeLimit: 'Cold probe limit',
  jitterSeconds1h: 'Jitter, 1h cache (s)',
  jitterSeconds5m: 'Jitter, 5m cache (s)',
  leadSeconds1h: 'Lead time, 1h cache (s)',
  leadSeconds5m: 'Lead time, 5m cache (s)',
  maxIdleSeconds: 'Max idle seconds',
  maxIdleTicks: 'Max idle ticks',
  netNegativeGraceProbes: 'Grace probes after the net turns negative',
  retryMaxSeconds: 'Retry backoff max (s)',
  retrySeconds: 'Retry backoff base (s)',
};

/** The Probe tuning group's fields in the order it lays them out, so its refusals read in that order too. */
export const PROBE_TUNING_KEYS = ['leadSeconds5m', 'leadSeconds1h', 'jitterSeconds5m', 'jitterSeconds1h', 'coldProbeLimit', 'retrySeconds', 'retryMaxSeconds', 'maxIdleSeconds', 'maxIdleTicks'] as const satisfies readonly NumericKeepaliveKey[];

/** A bound's sentence: a field with no practical ceiling states its floor alone. */
const boundText = (label: string, { max, min }: { max: number; min: number }) =>
  max === Number.MAX_SAFE_INTEGER ? `${label} is a whole number of at least ${fmtInt(min)}.` : `${label} is a whole number from ${fmtInt(min)} to ${fmtInt(max)}.`;
const listed = (label: string) => `${label} is one of its listed choices.`;

/** Every check of the pool routes the input fails, in rail order and in each group's order below; empty when the routes would take it. */
export function poolRefusals(input: PoolInput, accountIds: ReadonlySet<string>): PoolRefusal[] {
  const out: PoolRefusal[] = [];
  const refuse = (group: PoolGroup, field: string, text: string | null) => {
    if (text !== null) out.push({ field, group, text });
  };
  const k = input.keepalive;
  const affinity = input.affinityTtlSeconds;

  refuse('identity', 'Name', input.name.length > 0 ? null : 'the pool needs a name.');

  refuse('members', 'members', input.members.length > 0 ? null : 'the pool needs a member.');
  for (const m of input.members) {
    const id = m.accountId;
    refuse('members', `member:${id}`, accountIds.has(id) ? null : `${id} is not an upstream the console lists.`);
    refuse('members', `priority:${id}`, whole(m.priority, 0, Number.MAX_SAFE_INTEGER) ? null : 'Priority is a whole number of 0 or more.');
    // A member written without the budget fields has no budget and a month, as the member write reads it.
    refuse('members', `budget:${id}`, m.budgetUsd == null ? null : budgetRefusal(String(m.budgetUsd)));
    refuse('members', `period:${id}`, BUDGET_PERIOD_VALUES.some((period) => period === (m.budgetPeriod ?? 'month')) ? null : 'A budget period is a day or a month.');
  }

  refuse('models', 'Model rules', modelRulesRefusal(input.modelRules));

  refuse('placement', 'Subagent placement', SUBAGENT_PLACEMENT_VALUES.some((placement) => placement === input.subagentPlacement) ? null : listed('Subagent placement'));
  refuse('placement', 'Session placement', SESSION_PLACEMENT_VALUES.some((placement) => placement === input.sessionPlacement) ? null : listed('Session placement'));
  refuse('placement', 'Minimum shared prefix, tokens', prefixThresholdRefusal(String(input.minSharedPrefixTokens)));

  refuse('pinning', 'Session affinity in seconds', whole(affinity, 0, Number.MAX_SAFE_INTEGER) && (affinity === 0 || affinity >= 60) ? null : 'Session affinity is off or at least 60 whole seconds.');
  refuse('pinning', 'Pin sessions', AFFINITY_MODE_VALUES.some((mode) => mode === input.affinityMode) ? null : listed('Pin sessions'));

  // Migration keeps the form's floor of 50 %, above the route's 0.
  refuse('utilization', 'Migrate above utilization (%)', ratio(input.migrateAboveUtilization, 0.5) ? null : 'Migrate above utilization is from 50 to 100.');
  refuse('utilization', 'Select below utilization (%)', ratio(input.selectBelowUtilization, 0) ? null : 'Select below utilization is from 0 to 100.');
  refuse('utilization', 'Select below utilization (%)', bandRefusal(input));

  const engine = (group: PoolGroup, key: keyof typeof ENGINE_BOUNDS) =>
    refuse(group, ENGINE_FIELD_LABELS[key], whole(input[key], ENGINE_BOUNDS[key].min, ENGINE_BOUNDS[key].max) ? null : boundText(ENGINE_FIELD_LABELS[key], ENGINE_BOUNDS[key]));
  refuse('shedding', 'Share (%)', ratio(input.shedShare, 0) ? null : 'Share (%) is from 0 to 100.');
  engine('shedding', 'burnWindowSeconds');
  engine('shedding', 'sourceDwellSeconds');
  refuse('shedding', 'Warm sessions', WARM_SESSIONS_VALUES.some((mode) => mode === input.warmSessions) ? null : listed('Warm sessions'));

  refuse('failover', 'Pace floor (%)', ratio(input.paceFloor, 0) ? null : 'Pace floor (%) is from 0 to 100.');
  engine('failover', 'inFlightCap');
  engine('failover', 'retryInPlaceSeconds');

  engine('overage', 'overageMoveGraceMinutes');
  engine('overage', 'windDownMinutes');

  const keepalive = (group: PoolGroup, key: NumericKeepaliveKey) =>
    refuse(group, KEEPALIVE_FIELD_LABELS[key], whole(k[key], KEEPALIVE_BOUNDS[key].min, KEEPALIVE_BOUNDS[key].max) ? null : boundText(KEEPALIVE_FIELD_LABELS[key], KEEPALIVE_BOUNDS[key]));
  keepalive('keepalive', 'netNegativeGraceProbes');
  refuse('keepalive', 'Daily probe cost cap in dollars', k.costCapUsdPerDay === null || (Number.isFinite(k.costCapUsdPerDay) && k.costCapUsdPerDay > 0) ? null : 'Daily probe cost cap ($) is above 0.');
  refuse('keepalive', 'Keep prompt caches warm', !k.enabled || affinity >= 300 ? null : 'Keep prompt caches warm needs Session affinity of at least 5m.');

  for (const key of PROBE_TUNING_KEYS) keepalive('probeTuning', key);
  refuse('probeTuning', KEEPALIVE_FIELD_LABELS.retryMaxSeconds, k.retryMaxSeconds >= k.retrySeconds ? null : 'Retry backoff max (s) is at or above Retry backoff base (s).');

  refuse('responseHeaders', 'Quota headers', QUOTA_HEADERS_VALUES.some((headers) => headers === input.quotaHeaders) ? null : listed('Quota headers'));
  return out;
}

/** Whether the pool routes would take `input` as it stands, with every member one of `accountIds`. */
export function validatePoolInput(input: PoolInput, accountIds: ReadonlySet<string>): boolean {
  return poolRefusals(input, accountIds).length === 0;
}

export type CaptureMode = 'inherit' | 'off' | 'override';

/** The key dialog's fields as the operator leaves them. */
export interface KeyForm {
  affinitySource: AffinitySource;
  captureHours: number;
  captureMode: CaptureMode;
  enabled: boolean;
  /** `keep` leaves the stored expiry alone, `never` clears it, and a number is days from the save. */
  expiry: string;
  /** The number field's text; empty is no cap. */
  inFlightCap: string;
  longContextTwins: boolean;
  modelRules: ModelRule[];
  modelRulesMode: ModelRulesMode;
  name: string;
  notes: string;
  optOut: boolean;
  poolId: string;
}

/** The dialog's starting fields: the stored key's, or a new key's in `poolId`. */
export function keyFormOf(existing: ApiKey | null, poolId: string): KeyForm {
  return {
    affinitySource: existing?.affinitySource ?? 'auto',
    captureHours: existing?.bodyCaptureHours || 6,
    captureMode: existing ? (existing.bodyCaptureHours === null ? 'inherit' : existing.bodyCaptureHours === 0 ? 'off' : 'override') : 'inherit',
    // The stored switch rather than `status`: every edit sends it, and an expired or revoked key's status would
    // say nothing about it.
    enabled: existing?.enabled ?? true,
    expiry: existing ? 'keep' : 'never',
    inFlightCap: existing?.inFlightCap == null ? '' : String(existing.inFlightCap),
    longContextTwins: existing?.longContextTwins ?? true,
    modelRules: existing?.modelRules ?? [],
    modelRulesMode: existing?.modelRulesMode ?? 'extend',
    name: existing?.name ?? '',
    notes: existing?.notes ?? '',
    optOut: existing?.keepaliveOptOut ?? false,
    poolId: existing?.poolId ?? poolId,
  };
}

export function keyInputOf(form: KeyForm, now: number): KeyInput {
  return {
    affinitySource: form.affinitySource,
    bodyCaptureHours: form.captureMode === 'inherit' ? null : form.captureMode === 'off' ? 0 : form.captureHours,
    enabled: form.enabled,
    expiresAt: form.expiry === 'keep' ? undefined : form.expiry === 'never' ? null : new Date(now + Number(form.expiry) * 86_400_000).toISOString(),
    inFlightCap: form.inFlightCap.trim() === '' ? null : Number(form.inFlightCap),
    keepaliveOptOut: form.optOut,
    longContextTwins: form.longContextTwins,
    modelRules: form.modelRules,
    modelRulesMode: form.modelRulesMode,
    name: form.name.trim(),
    notes: form.notes.trim() || null,
    poolId: form.poolId,
  };
}

/** The key route's bound on `in_flight_cap`, a u32 of at least 1. */
const KEY_CAP_MAX = 4_294_967_295;

/** Why the key route would refuse the cap field as it stands, or null; empty is no cap. */
export function inFlightCapRefusal(value: string): string | null {
  if (value.trim() === '') return null;
  const n = Number(value);
  return whole(n, 1, KEY_CAP_MAX) ? null : 'Requests in flight is a whole number of at least 1; leave it empty for no cap.';
}

/** The identity card's subscription end as the operator leaves it; `endsAt` is a `datetime-local` value, empty for none. */
export interface SubscriptionEndForm {
  cancelled: boolean;
  endsAt: string;
}

export function subscriptionEndFormOf(account: Pick<Account, 'cancelled' | 'subscription'>): SubscriptionEndForm {
  const end = account.cancelled ? (account.subscription?.endsAt ?? null) : null;
  return { cancelled: account.cancelled, endsAt: end ? toLocalInput(Date.parse(end)) : '' };
}

/**
 * Ticking fills an empty field with the derived end, so the form sends an end whenever the box is ticked; unticking
 * clears the field, since the route keeps no end without the flag.
 */
export function tickCancelled(form: SubscriptionEndForm, cancelled: boolean, derivedEndsAt: string | null): SubscriptionEndForm {
  if (!cancelled) return { cancelled: false, endsAt: '' };
  return { cancelled: true, endsAt: form.endsAt || (derivedEndsAt ? toLocalInput(Date.parse(derivedEndsAt)) : '') };
}

/** Why the account route would refuse the pair, or null. */
export function subscriptionEndRefusal(form: SubscriptionEndForm): string | null {
  if (form.cancelled && form.endsAt === '') return 'A cancelled plan needs the date it ends.';
  if (!form.cancelled && form.endsAt !== '') return 'An end is saved only with Cancelled ticked.';
  return null;
}

export function accountPatchOf(form: SubscriptionEndForm): AccountPatch {
  return { cancelled: form.cancelled, subscriptionEndsAt: form.endsAt === '' ? null : new Date(form.endsAt).toISOString() };
}
