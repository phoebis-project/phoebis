import type { AccountPatch, KeyInput, PoolEngineFields, PoolInput } from '@/api/client';
import { WARM_SESSIONS_VALUES } from '@/api/decode';
import type { Account, ApiKey, ModelRule, ModelRulesMode, PoolKeepalive } from '@/api/types';
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

/** Whether the pool routes would take `input` as it stands, with every member one of `accountIds`. */
export function validatePoolInput(input: PoolInput, accountIds: ReadonlySet<string>): boolean {
  const k = input.keepalive;
  const tuned = (Object.keys(KEEPALIVE_BOUNDS) as NumericKeepaliveKey[]).every((key) => whole(k[key], KEEPALIVE_BOUNDS[key].min, KEEPALIVE_BOUNDS[key].max));
  const affinity = input.affinityTtlSeconds;
  return (
    input.name.length > 0 &&
    input.members.length > 0 &&
    input.members.every((m) => accountIds.has(m.accountId) && whole(m.priority, 0, Number.MAX_SAFE_INTEGER)) &&
    whole(affinity, 0, Number.MAX_SAFE_INTEGER) &&
    (affinity === 0 || affinity >= 60) &&
    // Migration keeps the form's floor of 50 %, above the route's 0.
    ratio(input.migrateAboveUtilization, 0.5) &&
    ratio(input.selectBelowUtilization, 0) &&
    bandRefusal(input) === null &&
    ratio(input.shedShare, 0) &&
    ratio(input.paceFloor, 0) &&
    (Object.keys(ENGINE_BOUNDS) as (keyof typeof ENGINE_BOUNDS)[]).every((key) => whole(input[key], ENGINE_BOUNDS[key].min, ENGINE_BOUNDS[key].max)) &&
    WARM_SESSIONS_VALUES.some((mode) => mode === input.warmSessions) &&
    tuned &&
    k.retryMaxSeconds >= k.retrySeconds &&
    (k.costCapUsdPerDay === null || (Number.isFinite(k.costCapUsdPerDay) && k.costCapUsdPerDay > 0)) &&
    (!k.enabled || affinity >= 300) &&
    modelRulesRefusal(input.modelRules) === null
  );
}

export type CaptureMode = 'inherit' | 'off' | 'override';

/** The key dialog's fields as the operator leaves them. */
export interface KeyForm {
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

/** A `datetime-local` value in the browser's zone, which is how the field reads and writes a date. */
const localInput = (at: string) => {
  const d = new Date(at);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
};

export function subscriptionEndFormOf(account: Pick<Account, 'cancelled' | 'subscription'>): SubscriptionEndForm {
  const end = account.cancelled ? (account.subscription?.endsAt ?? null) : null;
  return { cancelled: account.cancelled, endsAt: end ? localInput(end) : '' };
}

/**
 * Ticking fills an empty field with the derived end, so the form sends an end whenever the box is ticked; unticking
 * clears the field, since the route keeps no end without the flag.
 */
export function tickCancelled(form: SubscriptionEndForm, cancelled: boolean, derivedEndsAt: string | null): SubscriptionEndForm {
  if (!cancelled) return { cancelled: false, endsAt: '' };
  return { cancelled: true, endsAt: form.endsAt || (derivedEndsAt ? localInput(derivedEndsAt) : '') };
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
