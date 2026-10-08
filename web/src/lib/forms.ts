import type { AccountPatch, KeyInput, PoolEngineFields, PoolInput } from '@/api/client';
import { AFFINITY_MODE_VALUES, SESSION_PLACEMENT_VALUES, SUBAGENT_PLACEMENT_VALUES, WARM_SESSIONS_VALUES } from '@/api/decode';
import { slug } from '@/api/http';
import type { Account, ApiKey, ModelRule, ModelRulesMode, PoolKeepalive } from '@/api/types';
import { fmtInt, toLocalInput } from './format';
import { checkPattern } from './patterns';

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
export const POOL_GROUPS = ['identity', 'members', 'keys', 'models', 'autoMode', 'selection', 'placement', 'pinning', 'utilization', 'shedding', 'failover', 'overage', 'cacheTtl', 'keepalive', 'bodyCapture', 'cacheDiagnostics'] as const;
export type PoolGroup = (typeof POOL_GROUPS)[number];

export interface PoolRefusal {
  /** The refused control's accessible name; `priority:<id>`, `budget_daily:<id>`, `budget_monthly:<id>`, `member:<id>` for a member row; `members` for the missing member or an account list that did not load. */
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
  capture?: { maxHours: number; on: boolean; typed: string };
  /** On create the id derives from the name, so a taken one is held before the route's 409; a rename keeps its id. */
  identity?: { isNew: boolean; poolIds: ReadonlySet<string> };
}

/**
 * The id a create derives from a name, and whether the name gave it nothing: `slug` keeps ASCII letters and digits
 * alone, so a name without one falls back to the id `item`.
 */
function derivedId(name: string): { id: string; unnamed: boolean } {
  const id = slug(name);
  return { id, unnamed: id === 'item' && !/[a-z0-9]/i.test(name) };
}

/**
 * Every check of the pool routes the input fails, in rail order and in each group's order below; empty when the routes
 * would take it. `accountIds` null means the account list did not load: one refusal holds the save, and no member is
 * refused for an id the list might hold.
 */
export function poolRefusals(input: PoolInput, accountIds: ReadonlySet<string> | null, checks: PoolChecks = {}): PoolRefusal[] {
  const out: PoolRefusal[] = [];
  const refuse = (group: PoolGroup, field: string, text: string | null) => {
    if (text !== null) out.push({ field, group, text });
  };
  const k = input.keepalive;
  const affinity = input.affinityTtlSeconds;

  refuse('identity', 'Name', input.name.length > 0 ? null : 'the pool needs a name.');
  if (checks.identity?.isNew && input.name.length > 0) {
    const { id, unnamed } = derivedId(input.name);
    refuse('identity', 'Name', unnamed ? 'The pool needs a name with a letter or a digit.' : checks.identity.poolIds.has(id) ? `Another pool already has the id ${id}.` : null);
  }

  refuse('members', 'members', accountIds !== null ? null : 'the account list did not load, so the members cannot be checked.');
  refuse('members', 'members', input.members.length > 0 ? null : 'the pool needs a member.');
  for (const m of input.members) {
    const id = m.accountId;
    if (accountIds !== null) refuse('members', `member:${id}`, accountIds.has(id) ? null : `${id} is not an account the console lists.`);
    refuse('members', `priority:${id}`, whole(m.priority, 0, Number.MAX_SAFE_INTEGER) ? null : 'Priority is a whole number of 0 or more.');
    // A member written without a budget field has no budget over that period, as the member write reads it.
    refuse('members', `budget_daily:${id}`, m.budgetDailyUsd == null ? null : budgetRefusal(String(m.budgetDailyUsd)));
    refuse('members', `budget_monthly:${id}`, m.budgetMonthlyUsd == null ? null : budgetRefusal(String(m.budgetMonthlyUsd)));
  }

  refuse('models', 'Model rules', modelRulesRefusal(input.modelRules));

  refuse('placement', 'Subagent placement', SUBAGENT_PLACEMENT_VALUES.some((placement) => placement === input.subagentPlacement) ? null : listed('Subagent placement'));
  refuse('placement', 'Session placement', SESSION_PLACEMENT_VALUES.some((placement) => placement === input.sessionPlacement) ? null : listed('Session placement'));
  refuse('placement', 'Minimum shared prefix, tokens', prefixThresholdRefusal(String(input.minSharedPrefixTokens)));

  refuse('pinning', 'Session affinity in seconds', whole(affinity, 0, Number.MAX_SAFE_INTEGER) && (affinity === 0 || affinity >= 60) ? null : 'Session affinity is off or at least 60 whole seconds.');
  refuse('pinning', 'Pin sessions', AFFINITY_MODE_VALUES.some((mode) => mode === input.affinityMode) ? null : listed('Pin sessions'));

  // NaN is a switched-on field left empty, held here so a save never reads it as a margin of 0.
  const shedAhead = input.shedAheadOfPace;
  refuse('utilization', 'Shed ahead of pace (%)', Number.isNaN(shedAhead) ? 'Shed ahead of pace needs a value from 0 to 100.' : ratio(shedAhead, 0) ? null : 'Shed ahead of pace is from 0 to 100.');
  refuse('utilization', 'Select below utilization (%)', ratio(input.selectBelowUtilization, 0) ? null : 'Select below utilization is from 0 to 100.');

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

  refuse('keepalive', 'Keep prompt caches warm', !k.enabled || affinity >= 300 ? null : 'Keep prompt caches warm needs Session affinity of at least 5m.');
  for (const key of ['maxIdleSeconds', 'maxParkedSeconds', 'scheduledHorizonSeconds'] as const) {
    const value = k[key];
    refuse('keepalive', KEEPALIVE_FIELD_LABELS[key], value === null || whole(value, KEEPALIVE_BOUNDS[key].min, KEEPALIVE_BOUNDS[key].max) ? null : boundText(KEEPALIVE_FIELD_LABELS[key], KEEPALIVE_BOUNDS[key]));
  }

  refuse('bodyCapture', 'Capture retention in hours', checks.capture?.on && !whole(Number(checks.capture.typed), 1, checks.capture.maxHours) ? `Retention is a whole number of hours from 1 to ${checks.capture.maxHours}.` : null);
  return out;
}

/** Whether the pool routes would take `input` as it stands, with every member one of `accountIds`; null holds it. */
export function validatePoolInput(input: PoolInput, accountIds: ReadonlySet<string> | null, checks: PoolChecks = {}): boolean {
  return poolRefusals(input, accountIds, checks).length === 0;
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
    autoModeServer: existing?.autoModeServer == null ? 'inherit' : existing.autoModeServer ? 'on' : 'off',
    captureHours: String(existing?.bodyCaptureHours || 6),
    captureMode: existing ? (existing.bodyCaptureHours === null ? 'inherit' : existing.bodyCaptureHours === 0 ? 'off' : 'override') : 'inherit',
    // The stored switch rather than `status`: every edit sends it, and an expired or revoked key's status would
    // say nothing about it.
    enabled: existing?.enabled ?? true,
    expiry: existing ? 'keep' : 'never',
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
    autoModeServer: form.autoModeServer === 'inherit' ? null : form.autoModeServer === 'on',
    bodyCaptureHours: form.captureMode === 'inherit' ? null : form.captureMode === 'off' ? 0 : Number(form.captureHours),
    enabled: form.enabled,
    expiresAt: form.expiry === 'keep' ? undefined : form.expiry === 'never' ? null : new Date(now + Number(form.expiry) * 86_400_000).toISOString(),
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
export const KEY_GROUPS = ['key', 'access', 'models', 'sessions', 'capture'] as const;
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
    const { id, unnamed } = derivedId(name);
    if (unnamed) out.push({ field: 'Name', group: 'key', text: 'The key needs a name with a letter or a digit.' });
    else if (checks.identity.existingIds.has(id)) out.push({ field: 'Name', group: 'key', text: `Another key already has the id ${id}.` });
  }
  if (!form.poolId) out.push({ field: 'Pool', group: 'key', text: 'the key needs a pool.' });
  const rules = modelRulesRefusal(form.modelRules);
  if (rules) out.push({ field: 'Model rules', group: 'models', text: rules });
  // An emptied field would send 0, which the route stores as capture off while the radio still says Override.
  if (form.captureMode === 'override') {
    const { maxHours } = checks;
    if (!whole(Number(form.captureHours), 1, maxHours ?? Number.MAX_SAFE_INTEGER)) {
      const text = maxHours === undefined ? 'Capture hours is a whole number of at least 1.' : `Capture hours is a whole number from 1 to ${maxHours}.`;
      out.push({ field: 'Capture hours for this key', group: 'capture', text });
    }
  }
  return out;
}

/** The identity card's subscription end as the operator leaves it; `endsAt` is a `datetime-local` value, empty for none. */
export interface SubscriptionEndForm {
  cancelled: boolean;
  endsAt: string;
  /** The revision of the row the editor opened with, which the save sends as its fence. */
  revision: number;
}

export function subscriptionEndFormOf(
  account: Pick<Account, 'cancelled' | 'revision' | 'subscription'>,
): SubscriptionEndForm {
  const end = account.cancelled ? (account.subscription?.endsAt ?? null) : null;
  return { cancelled: account.cancelled, endsAt: end ? toLocalInput(Date.parse(end)) : '', revision: account.revision };
}

/**
 * Ticking fills an empty field with the derived end, so the form sends an end whenever the box is ticked; unticking
 * clears the field, since the route keeps no end without the flag.
 */
export function tickCancelled(form: SubscriptionEndForm, cancelled: boolean, derivedEndsAt: string | null): SubscriptionEndForm {
  if (!cancelled) return { ...form, cancelled: false, endsAt: '' };
  return { ...form, cancelled: true, endsAt: form.endsAt || (derivedEndsAt ? toLocalInput(Date.parse(derivedEndsAt)) : '') };
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

/** An account's edit token carries its revision in the low 32 bits and its incarnation above them. */
const ACCOUNT_INCARNATION = 2 ** 32;

/** Whether the editor holding `other` advances to `answered` after a save that sent `sent`: `other` equals `sent`
 *  (both editors opened on the same row state) and `answered` is the token an operator's own write produces from
 *  it, the same incarnation and the revision one higher. An editor whose token differs keeps it, since the row it
 *  opened on is not the row the save moved, and its own save meets the fence as before. */
export const advancesOwnToken = (sent: number, answered: number, other: number): boolean =>
  other === sent && answered === sent + 1 && Math.floor(answered / ACCOUNT_INCARNATION) === Math.floor(sent / ACCOUNT_INCARNATION);
