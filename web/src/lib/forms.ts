import type { KeyInput, PoolInput } from '@/api/client';
import type { ApiKey, ModelRule, ModelRulesMode, PoolKeepalive } from '@/api/types';
import { checkPattern } from './patterns';

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

const whole = (v: number, min: number, max: number) => Number.isInteger(v) && v >= min && v <= max;
const ratio = (v: number | null, min: number) => v === null || (v >= min && v <= 1);

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
    keepaliveOptOut: form.optOut,
    longContextTwins: form.longContextTwins,
    modelRules: form.modelRules,
    modelRulesMode: form.modelRulesMode,
    name: form.name.trim(),
    notes: form.notes.trim() || null,
    poolId: form.poolId,
  };
}
