import type { KeyInput, PoolInput } from '@/api/client';
import type { ApiKey, PoolKeepalive } from '@/api/types';

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
    (!k.enabled || affinity >= 300)
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
  modelMode: 'pool' | 'selected';
  name: string;
  notes: string;
  optOut: boolean;
  poolId: string;
  selected: string[];
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
    modelMode: existing?.allowedModels ? 'selected' : 'pool',
    name: existing?.name ?? '',
    notes: existing?.notes ?? '',
    optOut: existing?.keepaliveOptOut ?? false,
    poolId: existing?.poolId ?? poolId,
    selected: existing?.allowedModels ?? [],
  };
}

export function keyInputOf(form: KeyForm, now: number): KeyInput {
  return {
    allowedModels: form.modelMode === 'selected' ? form.selected : null,
    bodyCaptureHours: form.captureMode === 'inherit' ? null : form.captureMode === 'off' ? 0 : form.captureHours,
    enabled: form.enabled,
    expiresAt: form.expiry === 'keep' ? undefined : form.expiry === 'never' ? null : new Date(now + Number(form.expiry) * 86_400_000).toISOString(),
    keepaliveOptOut: form.optOut,
    name: form.name.trim(),
    notes: form.notes.trim() || null,
    poolId: form.poolId,
  };
}
