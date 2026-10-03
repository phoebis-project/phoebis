import { Fragment, useEffect, useMemo, useState, type ReactNode } from 'react';
import { Link } from 'react-router-dom';
import { Bar, ComposedChart, Line, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts';
import { api, ApiError, CACHE_TTL_DEFAULTS, KEEPALIVE_TUNING_DEFAULTS, type PoolInput } from '@/api';
import { POOL_AFFINITY_DEFAULTS, POOL_ENGINE_DEFAULTS, type PoolEngineFields } from '@/api/client';
import type {
  Account, AffinityMode, ApiKey, CacheTtl, CreatedApiKey, KeyStatus, MemberEligibility, ModelRule, OveragePolicy, Pool, PoolCacheTtl, PoolKeepalive, PoolMember, PoolStrategy, QuotaHeaders, Session, SessionPlacement,
  SubagentPlacement, TtlRules, WarmSessions,
} from '@/api/types';
import { useSession } from '@/app/session';
import { useAppSettings } from '@/app/settings';
import { useChartInk } from '@/components/charts/useChartInk';
import { ChartTooltip, Legend } from '@/components/charts/Tooltip';
import { PatternEditor, useCatalogSuggestions } from '@/components/PatternEditor';
import { CostReferenceLink } from '@/components/ui/CostReference';
import { SelectionOrder } from '@/components/ui/SelectionOrder';
import { Tile } from '@/components/ui/Tile';
import { UtilizationBar } from '@/components/UtilizationBar';
import { IconPencil, IconPlus, IconWarning } from '@/components/ui/icons';
import { Badge, Button, Card, ConfirmDialog, CopyButton, Dialog, ErrorState, Field, GroupedDialog, GroupLink, Input, Menu, PageLink, Select, Skeleton, SwitchedNumber, Toggle, type DialogGroup } from '@/components/ui/primitives';
import { sizedAccounts } from '@/lib/analytics';
import { fmtAbsolute, fmtCompact, fmtHourTick, fmtInt, fmtPct, fmtPeriodEnd, fmtRelative, fmtUsd } from '@/lib/format';
import { ENGINE_BOUNDS, ENGINE_FIELD_LABELS, KEEPALIVE_BOUNDS, KEEPALIVE_FIELD_LABELS, POOL_GROUPS, PROBE_TUNING_ROWS, bandRefusal, poolRefusals, type PoolGroup } from '@/lib/forms';
import { useAsync, useNow } from '@/lib/hooks';
import { POOL_RAIL } from '@/lib/poolRail';
import { FAILOVER_PRESETS, SHEDDING_PRESETS, failoverPresetOf, sheddingPresetOf, shedPreview, shedPreviewLine, type FailoverPreset, type SheddingPreset } from '@/lib/presets';
import { windowOf } from '@/lib/quota';
import { STRATEGY_LABEL, kindOrdered } from '@/lib/selection';
import { KeyDialog } from './KeysPage';

export const OVERAGE_LABEL: Record<OveragePolicy, string> = { allow: 'Allow overage', never: 'Never use overage', prefer_plan: 'Prefer plan quota' };
const KEY_STATUS_TONE: Record<KeyStatus, 'critical' | 'good' | 'neutral' | 'warning'> = { active: 'good', disabled: 'warning', expired: 'neutral', revoked: 'critical' };
const POOL_STATUS_TONE = { active: 'good', degraded: 'warning', disabled: 'neutral', exhausted: 'critical' } as const;
const fmtAffinity = (seconds: number) => (seconds === 0 ? 'off' : seconds % 3_600 === 0 ? `${seconds / 3_600}h` : seconds % 60 === 0 ? `${seconds / 60}m` : `${seconds}s`);
// A scale rather than an alphabetical list.
const AFFINITY_PICKS = [
  { label: 'off', seconds: 0 },
  { label: '5m', seconds: 300 },
  { label: '10m', seconds: 600 },
  { label: '1h', seconds: 3_600 },
  { label: '4h', seconds: 14_400 },
];
// Ordered best-first: the policy a new pool should usually take comes first.
const OVERAGE_CHOICES: { body: string; title: string; value: OveragePolicy }[] = [
  { value: 'prefer_plan', title: 'Prefer plan quota', body: 'move sessions to a sibling with plan quota when this account starts billing extra usage' },
  { value: 'allow', title: 'Allow', body: 'ignore extra usage' },
  { value: 'never', title: 'Never', body: 'treat an account in extra usage as unavailable until its window resets' },
];
// A pair's hint sits below both fields and a lone field carries its own, keyed by the row's first field.
const PROBE_HINT: Record<(typeof PROBE_TUNING_ROWS)[number][0], ReactNode> = {
  coldProbeLimit: (
    <>
      Probes in a row that find the cache already gone, after which the session&apos;s probes pause until its next real request. A cold probe rewrites the entry, so a streak also pulls the <GroupLink id="keepalive">net</GroupLink> down; a probe that moved the session does not count.
    </>
  ),
  jitterSeconds5m: 'Random seconds taken off each due time, so sessions that started together do not probe at the same instant; wider for 1-hour entries, whose due times cluster on the hour.',
  leadSeconds5m: 'How long before an entry expires its probe is due. The lead covers the round trip and the jitter; the 1-hour lead also leaves room for a retry, since a missed refresh costs a full 1-hour write.',
  maxIdleSeconds: (
    <>
      Stop probing once this long, or this many probes, passes without a real request: for the main agent while its companion reports, for every agent once the companion falls silent. 0 turns a limit off. The <PageLink to="/sessions">Sessions page</PageLink> shows each companion&apos;s state.
    </>
  ),
  retrySeconds: 'After a failed probe the wait starts at the base and doubles up to the max, so a long outage settles into a steady retry.',
};
const labelOf = (accountById: Map<string, Account>, id: string) => accountById.get(id)?.label ?? id;
/** Members first, lowest priority number first, then alphabetically; non-members trail alphabetically. */
const orderedAccountIds = (accounts: Account[], members: Record<string, number>): string[] =>
  [...accounts]
    .sort((a, b) => {
      const pa = members[a.id];
      const pb = members[b.id];
      if ((pa === undefined) !== (pb === undefined)) return pa === undefined ? 1 : -1;
      if (pa !== undefined && pb !== undefined && pa !== pb) return pa - pb;
      return a.label.localeCompare(b.label);
    })
    .map((a) => a.id);
// The card's tooltips carry the dialog's words; a title attribute holds no link, so they read without them.
const KEEPALIVE_SUMMARY_HINT =
  "Cold limit: probes in a row that find the cache already gone, after which the session's probes pause until its next real request. Idle: how long, or how many probes, without a real request before probing stops. Net-negative stop: probes a session still gets after its probes have cost more than the cache reads they saved.";
const AFFINITY_HINT = 'How long a session stays on its account after its last request; off keeps no pins, and a custom value is at least 60 seconds.';
const QUOTA_HEADERS_HINT =
  "Hide drops the anthropic-ratelimit-unified-* headers from every answer but a 429, so an organization's quota never reads as the key holder's own; a 429 keeps them so the client can resume after the reset. request-id passes either way.";
// The order the pills read in: the default, the two directions away from it, then a hand-set mix.
const PRESET_PILLS = ['balanced', 'fast', 'conservative', 'custom'] as const;
const PRESET_LABEL: Record<SheddingPreset, string> = { balanced: 'Balanced', conservative: 'Conservative', custom: 'Custom', fast: 'Fast' };
const SHEDDING_HINT: Record<SheddingPreset, string> = {
  balanced: 'The default.',
  conservative: 'Moves less and waits longer between rounds: fewer cache rewrites, slower relief.',
  custom: 'Set the share, the burn window, and the dwell by hand.',
  fast: 'Reads a shorter burn window and sheds again sooner: faster relief, more cache rewrites.',
};
const FAILOVER_HINT: Record<FailoverPreset, string> = {
  balanced: 'The default.',
  conservative: 'Waits out a longer throttle before moving on.',
  custom: 'Set the cap, the retry bound, and the floor by hand.',
  fast: 'Moves on sooner and spreads placements wider.',
};
// A scale from the fewest warm sessions moved to the most.
const WARM_CHOICES: { body: string; title: string; value: WarmSessions }[] = [
  { value: 'never', title: 'Never', body: 'a round moves cold sessions only, whatever the share still asks' },
  { value: 'when_needed', title: 'When needed', body: 'cold sessions first, then warm ones by burn over the prompt they would rewrite' },
  { value: 'always', title: 'Always', body: 'every session by burn over the prompt it would rewrite, warm or cold alike' },
];
const percentOf = (share: number) => Math.round(share * 10_000) / 100;

/** The two groups name their presets alike, so one row of pills serves both. */
function PresetPills({ label, value, onPick, hints }: { hints: Record<SheddingPreset | FailoverPreset, string>; label: string; onPick: (name: SheddingPreset | FailoverPreset) => void; value: SheddingPreset | FailoverPreset }) {
  return (
    <span className="chips" role="group" aria-label={label}>
      {PRESET_PILLS.map((name) => (
        <button key={name} type="button" className={`pill ${value === name ? 'is-active' : ''}`} aria-pressed={value === name} title={hints[name]} onClick={() => onPick(name)}>
          {PRESET_LABEL[name]}
        </button>
      ))}
    </span>
  );
}
const AFFINITY_MODE_LABEL: Record<AffinityMode, string> = { always: 'always', when_cached: 'when cached' };
const SUBAGENT_PLACEMENT_LABEL: Record<SubagentPlacement, string> = { by_cohort: 'by cohort', by_strategy: 'by strategy', with_main: 'with the main conversation' };
const fmtTokens = (n: number) => fmtCompact(n).toLowerCase();
/** The Strategy hint follows the chosen strategy; its first sentence is the card's rule, `STRATEGY_RULE`. */
const STRATEGY_HINT: Record<PoolStrategy, ReactNode> = {
  pace: (
    <>
      Within the first <GroupLink id="members">priority tier</GroupLink> that has an eligible account, the one with the most weekly allowance left per hour until its window resets or its subscription ends; an unsampled account counts as untouched, and ties are random. Each account&apos;s windows are on the <PageLink to="/accounts">Accounts page</PageLink>.
    </>
  ),
  priority: (
    <>
      Within the first <GroupLink id="members">priority tier</GroupLink> that has an eligible account, any of them at random.
    </>
  ),
  round_robin: (
    <>
      Within the first <GroupLink id="members">priority tier</GroupLink> that has an eligible account, each in turn; a restart starts the round again.
    </>
  ),
};

export function PoolsPage() {
  const pools = useAsync(() => api.listPools(), []);
  const accounts = useAsync(() => api.listAccounts(), []);
  const keys = useAsync(() => api.listKeys(), []);
  // Feeds the cache TTL per-model override picker's options.
  const catalog = useAsync(() => api.listCatalog(), []);
  const sessions = useAsync(() => api.listSessions(), []);
  const now = useNow();
  const { canMutate } = useAppSettings();
  const [dialog, setDialog] = useState<{ pool: Pool | null } | null>(null);
  const [deleting, setDeleting] = useState<Pool | null>(null);
  const { notify } = useSession();

  const accountById = useMemo(() => new Map((accounts.data ?? []).map((a) => [a.id, a])), [accounts.data]);
  const keysByPool = useMemo(() => {
    const m = new Map<string, ApiKey[]>();
    for (const k of keys.data ?? []) m.set(k.poolId, [...(m.get(k.poolId) ?? []), k]);
    return m;
  }, [keys.data]);
  // A session reaches a pool through its key, which is the only link the sessions route carries.
  const sessionsByPool = useMemo(() => {
    const poolOf = new Map((keys.data ?? []).map((k) => [k.id, k.poolId]));
    const m = new Map<string, Session[]>();
    for (const s of sessions.data ?? []) {
      const poolId = poolOf.get(s.keyId);
      if (poolId !== undefined) m.set(poolId, [...(m.get(poolId) ?? []), s]);
    }
    return m;
  }, [keys.data, sessions.data]);

  const removePool = async (p: Pool) => {
    try {
      await api.deletePool(p.id);
      notify('Pool deleted');
    } catch (e) {
      notify(e instanceof ApiError && e.status === 409 ? 'Move or revoke its keys first' : e instanceof Error ? e.message : String(e));
    }
    pools.reload();
    keys.reload();
  };
  const totals = pools.data
    ? {
        pools: pools.data.length,
        accounts: new Set(pools.data.flatMap((p) => p.members.map((m) => m.accountId))).size,
        keys: (keys.data ?? []).length,
        requests: pools.data.reduce((s, p) => s + p.requests24h, 0),
        cost: pools.data.reduce((s, p) => s + p.cost24h, 0),
      }
    : null;

  return (
    <div className="stack">
      <div className="page-head">
        <p className="page-head__meta">A pool groups accounts behind a set of keys. Requests pick a member by priority, stick to it per session, and fail over when a member hits a rate limit.</p>
        {canMutate && (
          <Button variant="primary" icon={<IconPlus size={14} />} onClick={() => setDialog({ pool: null })}>
            Create pool
          </Button>
        )}
      </div>
      <div className="stat-grid stat-grid--5">
        <Tile label="Pools" value={totals ? fmtInt(totals.pools) : null} />
        <Tile label="Accounts in pools" value={totals ? fmtInt(totals.accounts) : null} />
        <Tile label="Keys" value={totals ? fmtInt(totals.keys) : null} />
        <Tile label="Requests, 24h" value={totals ? fmtInt(totals.requests) : null} />
        <Tile label="Cost, 24h" value={totals ? fmtUsd(totals.cost) : null} />
      </div>
      <div className="pool-grid">
        {pools.data
          ? pools.data.map((p) => (
              <PoolCard
                key={p.id}
                pool={p}
                keys={keysByPool.get(p.id) ?? []}
                accounts={accounts.data ?? []}
                accountById={accountById}
                sessions={sessionsByPool.get(p.id) ?? []}
                now={now}
                canMutate={canMutate}
                onEdit={() => setDialog({ pool: p })}
                onDelete={() => setDeleting(p)}
              />
            ))
          : pools.error
            ? <ErrorState title="Could not load the pools" body={pools.error} action={<Button size="sm" onClick={pools.reload}>Try again</Button>} />
            : [0, 1].map((i) => <Skeleton key={i} height={360} />)}
      </div>
      <PoolDialog
        open={dialog !== null}
        pool={dialog?.pool ?? null}
        onClose={(reload) => {
          setDialog(null);
          if (reload) {
            pools.reload();
            keys.reload();
          }
        }}
        accounts={accounts.data ?? []}
        accountsFailed={accounts.data === null && accounts.error !== null ? accounts.error : null}
        accountsLoaded={accounts.data !== null || accounts.error !== null}
        onRetryAccounts={accounts.reload}
        keys={keys.data ?? []}
        pools={pools.data ?? []}
        sessions={sessions.data ?? []}
        models={(catalog.data ?? []).map((m) => ({ label: m.id, value: m.id }))}
        onSaved={(created) => {
          setDialog(null);
          notify(created ? 'Pool created' : 'Pool saved');
          pools.reload();
          keys.reload();
        }}
        // The pool route lists its keys, so a key created, moved or revoked from the dialog changes the card's
        // warning too.
        onKeyCreated={() => {
          keys.reload();
          pools.reload();
        }}
        onKeysChanged={() => {
          keys.reload();
          pools.reload();
        }}
      />
      <ConfirmDialog
        open={deleting !== null}
        onClose={() => setDeleting(null)}
        title={deleting ? `Delete pool ${deleting.name}` : 'Delete pool'}
        confirmLabel="Delete"
        danger
        body="Deletes the pool and its memberships. Its keys must be moved or revoked first."
        onConfirm={() => {
          if (deleting) void removePool(deleting);
        }}
      />
    </div>
  );
}

/** The two rule sets as one grid, the plan and the metered choice of each row side by side. */
function CacheTtlGrid({ disabled, hint, metered, onChange, plan }: { disabled: boolean; hint: ReactNode; metered: TtlRules; onChange: (name: 'metered' | 'plan', rules: TtlRules) => void; plan: TtlRules }) {
  const rows = [
    { key: 'main', label: 'Main conversation', name: 'main conversation' },
    { key: 'subagent', label: 'Subagents', name: 'subagent' },
  ] as const;
  const sets = [
    { label: 'Plan writes', name: 'plan', rules: plan },
    { label: 'Metered writes', name: 'metered', rules: metered },
  ] as const;
  return (
    <div className="field">
      <div className="table-wrap">
        <table className="table ttl-grid">
          <thead>
            <tr>
              <th />
              {sets.map((s) => (
                <th key={s.name}>{s.label}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => (
              <tr key={row.key}>
                <th scope="row">{row.label}</th>
                {sets.map((s) => (
                  <td key={s.name}>
                    <Select value={s.rules[row.key]} disabled={disabled} onChange={(e) => onChange(s.name, { ...s.rules, [row.key]: e.target.value as CacheTtl })} aria-label={`${s.label} ${row.name} TTL`}>
                      <option value="5m">5 minutes</option>
                      <option value="1h">1 hour</option>
                    </Select>
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <p className="field__hint">{hint}</p>
    </div>
  );
}

/** One rule set's per-model overrides; the picker's state is its own, so each set adds overrides independently. */
function CacheTtlOverrides({
  disabled,
  label,
  models,
  name,
  onChange,
  rules,
}: {
  disabled: boolean;
  label: string;
  models: { label: string; value: string }[];
  name: 'metered' | 'plan';
  onChange: (rules: TtlRules) => void;
  rules: TtlRules;
}) {
  const [addModelId, setAddModelId] = useState('');
  const setModelOverride = (model: string, key: 'main' | 'subagent', value: CacheTtl) =>
    onChange({ ...rules, models: rules.models.map((m) => (m.model === model ? { ...m, [key]: value } : m)) });
  const removeModelOverride = (model: string) => onChange({ ...rules, models: rules.models.filter((m) => m.model !== model) });
  const addModelOverride = () => {
    if (!addModelId) return;
    onChange({ ...rules, models: [...rules.models, { model: addModelId, main: rules.main, subagent: rules.subagent }] });
    setAddModelId('');
  };
  return (
    <Field plain label={label} data-rule-set={name} hint="A model absent here takes the rule above.">
      {rules.models.length > 0 && (
        <div className="table-wrap">
          <table className="table">
            <thead>
              <tr>
                <th>Model</th>
                <th>Main conversation</th>
                <th>Subagents</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {rules.models.map((m) => (
                <tr key={m.model}>
                  <td className="mono">{m.model}</td>
                  <td>
                    <Select value={m.main} disabled={disabled} onChange={(e) => setModelOverride(m.model, 'main', e.target.value as CacheTtl)} aria-label={`${label} main conversation TTL for ${m.model}`}>
                      <option value="5m">5 minutes</option>
                      <option value="1h">1 hour</option>
                    </Select>
                  </td>
                  <td>
                    <Select value={m.subagent} disabled={disabled} onChange={(e) => setModelOverride(m.model, 'subagent', e.target.value as CacheTtl)} aria-label={`${label} subagent TTL for ${m.model}`}>
                      <option value="5m">5 minutes</option>
                      <option value="1h">1 hour</option>
                    </Select>
                  </td>
                  <td>
                    <Button size="sm" variant="ghost" disabled={disabled} onClick={() => removeModelOverride(m.model)}>
                      Remove
                    </Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <div className="row">
        <Select value={addModelId} disabled={disabled} onChange={(e) => setAddModelId(e.target.value)} aria-label={`Model to add a ${label.toLowerCase()} cache TTL override for`}>
          <option value="">Choose a model…</option>
          {models
            .filter((m) => !rules.models.some((x) => x.model === m.value))
            .map((m) => (
              <option key={m.value} value={m.value}>
                {m.label}
              </option>
            ))}
        </Select>
        <Button size="sm" icon={<IconPlus size={13} />} disabled={disabled || !addModelId} onClick={addModelOverride}>
          Add
        </Button>
      </div>
    </Field>
  );
}

function PoolCard({
  pool,
  keys,
  accounts,
  accountById,
  sessions,
  now,
  canMutate,
  onEdit,
  onDelete,
}: {
  accountById: Map<string, Account>;
  accounts: Account[];
  canMutate: boolean;
  keys: ApiKey[];
  now: number;
  onDelete: () => void;
  onEdit: () => void;
  pool: Pool;
  sessions: Session[];
}) {
  const ink = useChartInk();
  const data = pool.traffic24h.map((b, i, all) => {
    const window = all.slice(Math.max(0, i - 2), i + 1);
    return { at: Date.parse(b.at), requests: b.requests, tokens: b.tokens, average: window.reduce((s, x) => s + x.requests, 0) / window.length };
  });
  const tokens24h = pool.traffic24h.reduce((s, b) => s + b.tokens, 0);
  const moves = Object.entries(pool.movesPerHour)
    .filter(([, rate]) => rate > 0)
    .sort(([a], [b]) => a.localeCompare(b));
  return (
    <Card padded={false} className="pool">
      <header className="pool__head">
        <div className="pool__title">
          <h2 className="card__title mono">{pool.name}</h2>
          <Badge tone={POOL_STATUS_TONE[pool.status]} dot>
            {pool.status}
          </Badge>
          {canMutate && (
            <>
              <Button size="sm" variant="ghost" icon={<IconPencil size={13} />} onClick={onEdit} className="pool__edit" aria-label={`Edit pool ${pool.name}`}>
                Edit
              </Button>
              <Button size="sm" variant="ghost" onClick={onDelete} aria-label={`Delete pool ${pool.name}`}>
                Delete
              </Button>
            </>
          )}
        </div>
        <dl className="pool__policy">
          <div>
            <dt>Strategy</dt>
            <dd>
              {STRATEGY_LABEL[pool.strategy]}
              {kindOrdered(pool) && ' · subscription first'}
            </dd>
          </div>
          <div>
            <dt>Sessions</dt>
            <dd className="num" title={AFFINITY_HINT}>
              Affinity {fmtAffinity(pool.affinityTtlSeconds)}
              {pool.affinityTtlSeconds > 0 && `, ${AFFINITY_MODE_LABEL[pool.affinityMode]}`}
            </dd>
          </div>
          <div>
            <dt>Overage</dt>
            <dd>{OVERAGE_LABEL[pool.overagePolicy]}</dd>
          </div>
          <div>
            <dt>Migration</dt>
            <dd className="num" title={`Share ${fmtPct(pool.shedShare)}, burn window ${pool.burnWindowSeconds} s, source dwell ${pool.sourceDwellSeconds} s`}>
              {pool.migrateAboveUtilization === null ? <span className="muted">Migrate off</span> : `Shed above ${fmtPct(pool.migrateAboveUtilization)}, ${sheddingPresetOf(pool)}`}
            </dd>
          </div>
          <div>
            <dt>Selection</dt>
            <dd className="num">{pool.selectBelowUtilization === null ? <span className="muted">Select-below off</span> : `Select below ${fmtPct(pool.selectBelowUtilization)} utilization`}</dd>
          </div>
          <div>
            <dt>Failover</dt>
            <dd title={`In flight ${pool.inFlightCap}, retry in place ${pool.retryInPlaceSeconds} s, Pace floor ${fmtPct(pool.paceFloor)}`}>{failoverPresetOf(pool)}</dd>
          </div>
          {pool.quotaHeaders === 'hide' && (
            <div>
              <dt>Quota headers</dt>
              <dd title={QUOTA_HEADERS_HINT}>hidden</dd>
            </div>
          )}
          {/* The badge is wider than any column's value, so on a column of its own it would push the grid past a
              narrow card. */}
          {!pool.autoModeServer && (
            <div className="pool__policy-wide">
              <dt>Auto mode</dt>
              <dd>
                <Badge tone="neutral">server-side auto mode off</Badge>
              </dd>
            </div>
          )}
          <div className="pool__policy-wide">
            <dt>Placement</dt>
            <dd>
              subagents {SUBAGENT_PLACEMENT_LABEL[pool.subagentPlacement]}, sessions{' '}
              {pool.sessionPlacement === 'by_warm_prefix' ? `by warm prefix from ${fmtTokens(pool.minSharedPrefixTokens)} tokens` : 'by strategy'}
            </dd>
          </div>
          {/* A list of rates, one per reason, so it takes a row of its own rather than wrapping in a column. */}
          <div className="pool__policy-wide">
            <dt>Moves</dt>
            <dd className="num">{moves.length === 0 ? <span className="muted">none in the last hour</span> : moves.map(([reason, rate]) => `${rate.toFixed(1)}/h ${reason.replace(/_/g, ' ')}`).join(', ')}</dd>
          </div>
          <div className="pool__policy-wide">
            <dt>Keep prompt caches warm</dt>
            <dd>
              {pool.keepalive.enabled ? (
                <span className="chips" title={KEEPALIVE_SUMMARY_HINT}>
                  <Badge tone="good">on</Badge>
                  <Badge tone="neutral">cold limit {pool.keepalive.coldProbeLimit}</Badge>
                  <Badge tone="neutral">{pool.keepalive.maxIdleSeconds === 0 ? 'no idle limit' : `idle ${fmtAffinity(pool.keepalive.maxIdleSeconds)}`}</Badge>
                  <Badge tone="neutral">{pool.keepalive.stopWhenNetNegative ? `net-negative stop ${pool.keepalive.netNegativeGraceProbes}` : 'no net-negative stop'}</Badge>
                  <Badge tone="neutral">{pool.keepalive.costCapUsdPerDay !== null ? `${fmtUsd(pool.keepalive.costCapUsdPerDay)}/day cap` : 'uncapped'}</Badge>
                </span>
              ) : (
                <span className="muted">off</span>
              )}
            </dd>
          </div>
          <div className="pool__policy-wide">
            <dt>Cache TTL</dt>
            <dd>
              <span className="chips">
                {pool.cacheTtl.enabled ? (
                  <>
                    <span className="mono">
                      plan {pool.cacheTtl.plan.main} / {pool.cacheTtl.plan.subagent}, metered {pool.cacheTtl.metered.main} / {pool.cacheTtl.metered.subagent}
                    </span>
                    {pool.cacheTtl.injectWhenAbsent && <Badge tone="neutral">adds markers</Badge>}
                    {(pool.cacheTtl.plan.models.length + pool.cacheTtl.metered.models.length) > 0 && (
                      <Badge tone="neutral">
                        {pool.cacheTtl.plan.models.length + pool.cacheTtl.metered.models.length} model override
                        {pool.cacheTtl.plan.models.length + pool.cacheTtl.metered.models.length === 1 ? '' : 's'}
                      </Badge>
                    )}
                  </>
                ) : (
                  <span className="muted">off</span>
                )}
                {/* Outside the branch above: a pool that rewrites no breakpoint can still force diagnostics. */}
                {pool.cacheDiagnostics !== null && <Badge tone="neutral">diagnostics {pool.cacheDiagnostics ? 'on' : 'off'}</Badge>}
              </span>
            </dd>
          </div>
        </dl>
      </header>

      <div className="pool__chart">
        <div className="pool__chart-head">
          <span className="secondary">Traffic, 24h</span>
          <span className="num">
            <b>{fmtCompact(tokens24h)}</b> tokens, <b>{fmtInt(pool.requests24h)}</b> requests
          </span>
        </div>
        {pool.requests24h === 0 ? (
          <div className="empty" style={{ padding: 30 }}>
            No traffic in the last 24h
          </div>
        ) : (
          <ResponsiveContainer width="100%" height={170}>
            <ComposedChart data={data} margin={{ top: 6, right: 4, bottom: 0, left: 4 }} barCategoryGap={2}>
              <XAxis dataKey="at" tickFormatter={(v: number) => fmtHourTick(v)} tickLine={false} axisLine={{ stroke: ink.axis }} interval={5} minTickGap={24} />
              <YAxis width={32} tickLine={false} axisLine={false} tick={{ fill: ink.ink2, fontSize: 11 }} allowDecimals={false} />
              <Tooltip
                cursor={{ fill: ink.grid, opacity: 0.6 }}
                content={({ payload }) => {
                  const p = payload?.[0]?.payload as (typeof data)[number] | undefined;
                  if (!p) return null;
                  return (
                    <ChartTooltip
                      title={fmtAbsolute(new Date(p.at).toISOString())}
                      rows={[
                        { color: ink.series[0], label: 'requests', value: fmtInt(p.requests) },
                        { color: ink.ink2, label: '3-hour average', value: p.average.toFixed(1) },
                        { label: 'tokens', value: fmtCompact(p.tokens) },
                      ]}
                    />
                  );
                }}
              />
              <Bar dataKey="requests" fill={ink.series[0]} radius={[3, 3, 0, 0]} maxBarSize={14} isAnimationActive={false} />
              <Line type="monotone" dataKey="average" stroke={ink.ink2} strokeWidth={2} dot={false} isAnimationActive={false} />
            </ComposedChart>
          </ResponsiveContainer>
        )}
        <Legend
          items={[
            { color: ink.series[0], label: 'Requests per hour' },
            { color: ink.ink2, label: '3-hour average', shape: 'line' },
          ]}
        />
      </div>

      <dl className="pool__stats">
        <div>
          <dt>Members</dt>
          <dd className="num">{pool.members.length}</dd>
        </div>
        <div>
          <dt>Keys</dt>
          <dd className="num">{keys.length}</dd>
        </div>
        <div>
          <dt>Requests 24h</dt>
          <dd className="num">{fmtInt(pool.requests24h)}</dd>
        </div>
        <div>
          <dt>Cost 24h</dt>
          <dd className="num">{fmtUsd(pool.cost24h)}</dd>
        </div>
        <div>
          <dt>Active pins</dt>
          <dd className="num">{pool.activePins}</dd>
        </div>
      </dl>

      <div className="pool__members">
        <h3 className="section-title">Members</h3>
        <ul>
          {pool.members
            .slice()
            .sort((a, b) => a.priority - b.priority || labelOf(accountById, a.accountId).localeCompare(labelOf(accountById, b.accountId)))
            .map((m) => {
              const a = accountById.get(m.accountId);
              return (
                <li key={m.accountId}>
                  <span className="pool__prio num" title="Priority">
                    P{m.priority}
                  </span>
                  <Link to={`/accounts/${m.accountId}`}>{a?.label ?? m.accountId}</Link>
                  <span className="pool__eligibility">
                    <MemberBudget member={m} />
                    <Eligibility verdict={m.eligible} account={a} now={now} periodEnd={m.budgetUntil} />
                  </span>
                </li>
              );
            })}
        </ul>
        <h3 className="section-title" style={{ marginTop: 12 }}>
          Selection order
        </h3>
        <SelectionOrder pool={pool} accounts={accounts} sessions={sessions} />
        <h3 className="section-title" style={{ marginTop: 12 }}>
          Keys
        </h3>
        <div className="row">
          {keys.map((k) => (
            <Link key={k.id} to="/keys" className="badge badge--neutral mono">
              {k.name}
            </Link>
          ))}
          {keys.length === 0 && <span className="muted">No keys yet</span>}
        </div>
      </div>
    </Card>
  );
}

/**
 * A member's spend against each budget it carries, an Anthropic-key member's whole spend and a subscription member's
 * metered spend (overage and fast answers), or that it has none.
 */
function MemberBudget({ member }: { member: PoolMember }) {
  // The word leads, so the operator reads at once that the plan's own traffic is not in the figure.
  const metered = member.kind === 'oauth' ? 'metered ' : '';
  const budgets = (
    [
      ['day', member.budgetDailyUsd],
      ['month', member.budgetMonthlyUsd],
    ] as const
  ).flatMap(([period, budget]) => (budget === null ? [] : [{ budget, period }]));
  if (budgets.length === 0) return <span className="muted">no budget</span>;
  return (
    <>
      {budgets.map(({ budget, period }) => {
        const spent = member.spends.find((s) => s.period === period)?.spentUsd;
        return (
          <span key={period} className="num">
            {metered}
            {spent === undefined ? '-' : fmtUsd(spent)} of {fmtUsd(budget)} {period === 'day' ? 'today' : 'this month'}
          </span>
        );
      })}
      {/* The route's budget verdict says this itself; the badge stands for a member the route names an earlier
          obstacle for. */}
      {member.budgetUntil !== null && member.eligible !== 'budget' && <AtBudget periodEnd={member.budgetUntil} />}
    </>
  );
}

function AtBudget({ periodEnd }: { periodEnd: string | null }) {
  return <Badge tone="warning">{periodEnd ? `at budget until ${fmtPeriodEnd(periodEnd)}` : 'at budget'}</Badge>;
}

/** The route's verdict, the one the selection-order strip reads; the account only adds when and why. */
function Eligibility({ verdict, account, now, periodEnd }: { account: Account | undefined; now: number; periodEnd: string | null; verdict: MemberEligibility }) {
  if (verdict === 'eligible') return <Badge tone="good">eligible</Badge>;
  if (verdict === 'budget') return <AtBudget periodEnd={periodEnd} />;
  if (verdict === 'overage') return <Badge tone="serious">overage</Badge>;
  if (verdict === 'unavailable') return <Badge tone="critical" title={account ? account.status.replace(/_/g, ' ') : 'not a known account'}>unavailable</Badge>;
  const until = account?.cooldowns.find((c) => c.scope === 'account')?.until;
  return until ? (
    <Badge tone="warning" title={fmtAbsolute(until)}>
      cooldown ends {fmtRelative(until, now)}
    </Badge>
  ) : (
    <Badge tone="warning">cooldown</Badge>
  );
}

type ProbeTuning = Omit<PoolKeepalive, 'costCapUsdPerDay' | 'enabled' | 'warmSubagents'>;
type NumericTuningKey = { [K in keyof ProbeTuning]-?: ProbeTuning[K] extends number ? K : never }[keyof ProbeTuning];

interface PoolForm {
  affinity: number;
  affinityMode: AffinityMode;
  autoModeServer: boolean;
  /** Each member's daily and monthly budget fields as typed, empty for none; kept for an unticked member too. */
  budgets: Record<string, { daily: string; monthly: string }>;
  cacheDiagnostics: boolean | null;
  cacheTtl: PoolCacheTtl;
  capEnabled: boolean;
  capture: boolean;
  /** The retention field as typed, so an emptied field stays empty rather than reading as 0. */
  captureHours: string;
  capUsd: number;
  customAffinity: boolean;
  engine: PoolEngineFields;
  /** The Custom pill is held apart from the values, so its inputs stay open while an edit passes through a preset. */
  failoverCustom: boolean;
  keepalive: boolean;
  members: Record<string, number>;
  migrate: number;
  migrateOn: boolean;
  modelRules: ModelRule[];
  name: string;
  overage: OveragePolicy;
  preferSubscription: boolean;
  /** The threshold field as typed. */
  prefixTokens: string;
  quotaHeaders: QuotaHeaders;
  selectBelow: number;
  selectBelowOn: boolean;
  sessionPlacement: SessionPlacement;
  sheddingCustom: boolean;
  status: 'active' | 'disabled';
  strategy: PoolStrategy;
  subagentPlacement: SubagentPlacement;
  tuning: ProbeTuning;
  warmSubagents: boolean;
}

const isPreset = (seconds: number) => AFFINITY_PICKS.some((p) => p.seconds === seconds);

const blankForm = (): PoolForm => ({
  affinity: 3_600,
  affinityMode: POOL_AFFINITY_DEFAULTS.affinityMode,
  autoModeServer: true,
  budgets: {},
  cacheDiagnostics: null,
  cacheTtl: { ...CACHE_TTL_DEFAULTS },
  capEnabled: false,
  capture: false,
  captureHours: '24',
  capUsd: 5,
  customAffinity: false,
  engine: { ...POOL_ENGINE_DEFAULTS },
  failoverCustom: false,
  keepalive: true,
  members: {},
  migrate: 90,
  migrateOn: false,
  modelRules: [],
  name: '',
  overage: 'prefer_plan',
  preferSubscription: true,
  prefixTokens: String(POOL_AFFINITY_DEFAULTS.minSharedPrefixTokens),
  quotaHeaders: POOL_AFFINITY_DEFAULTS.quotaHeaders,
  selectBelow: 80,
  selectBelowOn: false,
  sessionPlacement: POOL_AFFINITY_DEFAULTS.sessionPlacement,
  sheddingCustom: false,
  status: 'active',
  strategy: 'pace',
  subagentPlacement: POOL_AFFINITY_DEFAULTS.subagentPlacement,
  tuning: { ...KEEPALIVE_TUNING_DEFAULTS },
  warmSubagents: true,
});

const formFrom = (pool: Pool): PoolForm => ({
  affinity: pool.affinityTtlSeconds,
  affinityMode: pool.affinityMode,
  autoModeServer: pool.autoModeServer,
  budgets: Object.fromEntries(pool.members.map((m) => [m.accountId, { daily: m.budgetDailyUsd === null ? '' : String(m.budgetDailyUsd), monthly: m.budgetMonthlyUsd === null ? '' : String(m.budgetMonthlyUsd) }])),
  cacheDiagnostics: pool.cacheDiagnostics,
  cacheTtl: {
    ...pool.cacheTtl,
    metered: { ...pool.cacheTtl.metered, models: pool.cacheTtl.metered.models.map((m) => ({ ...m })) },
    plan: { ...pool.cacheTtl.plan, models: pool.cacheTtl.plan.models.map((m) => ({ ...m })) },
  },
  capEnabled: pool.keepalive.costCapUsdPerDay !== null,
  capture: pool.bodyCaptureHours > 0,
  captureHours: String(pool.bodyCaptureHours || 24),
  capUsd: pool.keepalive.costCapUsdPerDay ?? 5,
  customAffinity: !isPreset(pool.affinityTtlSeconds),
  engine: {
    burnWindowSeconds: pool.burnWindowSeconds,
    inFlightCap: pool.inFlightCap,
    migrateWhenSpent: pool.migrateWhenSpent,
    overageMoveGraceMinutes: pool.overageMoveGraceMinutes,
    paceFloor: pool.paceFloor,
    retryInPlaceSeconds: pool.retryInPlaceSeconds,
    shedShare: pool.shedShare,
    sourceDwellSeconds: pool.sourceDwellSeconds,
    warmSessions: pool.warmSessions,
    windDownMinutes: pool.windDownMinutes,
  },
  failoverCustom: failoverPresetOf(pool) === 'custom',
  keepalive: pool.keepalive.enabled,
  members: Object.fromEntries(pool.members.map((m) => [m.accountId, m.priority])),
  migrate: pool.migrateAboveUtilization === null ? 90 : Math.round(pool.migrateAboveUtilization * 100),
  migrateOn: pool.migrateAboveUtilization !== null,
  modelRules: pool.modelRules,
  name: pool.name,
  overage: pool.overagePolicy,
  // The stored answer, never the effective one, so reopening a pool shows what the operator chose.
  preferSubscription: pool.preferSubscription,
  prefixTokens: String(pool.minSharedPrefixTokens),
  quotaHeaders: pool.quotaHeaders,
  // Two decimals, so a threshold the API set finer than a percent survives an unrelated edit.
  selectBelow: pool.selectBelowUtilization === null ? 80 : Math.round(pool.selectBelowUtilization * 10_000) / 100,
  selectBelowOn: pool.selectBelowUtilization !== null,
  sessionPlacement: pool.sessionPlacement,
  sheddingCustom: sheddingPresetOf(pool) === 'custom',
  // Only the two settable states reach the form; a degraded or exhausted pool is still routing.
  status: pool.status === 'disabled' ? 'disabled' : 'active',
  strategy: pool.strategy,
  subagentPlacement: pool.subagentPlacement,
  tuning: {
    capWindowSharePerDay: pool.keepalive.capWindowSharePerDay,
    coldProbeLimit: pool.keepalive.coldProbeLimit,
    jitterSeconds1h: pool.keepalive.jitterSeconds1h,
    jitterSeconds5m: pool.keepalive.jitterSeconds5m,
    leadSeconds1h: pool.keepalive.leadSeconds1h,
    leadSeconds5m: pool.keepalive.leadSeconds5m,
    maxIdleSeconds: pool.keepalive.maxIdleSeconds,
    maxIdleTicks: pool.keepalive.maxIdleTicks,
    netNegativeGraceProbes: pool.keepalive.netNegativeGraceProbes,
    retryMaxSeconds: pool.keepalive.retryMaxSeconds,
    retrySeconds: pool.keepalive.retrySeconds,
    stopWhenNetNegative: pool.keepalive.stopWhenNetNegative,
  },
  warmSubagents: pool.keepalive.warmSubagents,
});

export function PoolDialog({
  open,
  pool,
  onClose,
  onSaved,
  onKeyCreated,
  onKeysChanged,
  onRetryAccounts,
  accounts,
  accountsFailed,
  accountsLoaded,
  keys,
  pools,
  models,
  sessions,
}: {
  accounts: Account[];
  /** The accounts read's error while it has never answered; the Members group names it in place of the table. */
  accountsFailed: string | null;
  /** Whether the accounts read has answered or failed; before that no stored member is refused or marked as unlisted. */
  accountsLoaded: boolean;
  keys: ApiKey[];
  models: { label: string; value: string }[];
  /** `reload` is true after a save the route answered with a refusal: it may have stored the settings before refusing. */
  onClose: (reload: boolean) => void;
  onKeyCreated: (c: CreatedApiKey) => void;
  onKeysChanged: () => void;
  onRetryAccounts: () => void;
  onSaved: (created: boolean) => void;
  open: boolean;
  pool: Pool | null;
  pools: Pool[];
  /** Every session the router holds: a shedding round weighs all of a member's pins, whichever pool's key made them. */
  sessions: Session[];
}) {
  const now = useNow();
  const [form, setForm] = useState<PoolForm>(blankForm);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [attempted, setAttempted] = useState(false);
  const [chosen, setChosen] = useState<PoolGroup>('identity');
  const settings = useAsync(() => api.getSettings(), []);
  const captureMaxHours = settings.data?.retention.bodyCaptureMaxHours ?? 72;
  const [keyOpen, setKeyOpen] = useState(false);
  const [created, setCreated] = useState<CreatedApiKey | null>(null);
  // Keys created from inside the dialog appear in the list before the parent reloads.
  const [localKeys, setLocalKeys] = useState<ApiKey[]>([]);
  const [moved, setMoved] = useState<Set<string>>(new Set());
  const [moveBusy, setMoveBusy] = useState(false);
  const [moveError, setMoveError] = useState<string | null>(null);
  const [moveTo, setMoveTo] = useState('');
  const [moving, setMoving] = useState<ApiKey | null>(null);
  const [revoked, setRevoked] = useState<Set<string>>(new Set());
  const [revoking, setRevoking] = useState<ApiKey | null>(null);
  // Held apart from the form so a priority keystroke cannot move the row under the cursor.
  const [memberOrder, setMemberOrder] = useState<string[]>([]);
  useEffect(() => {
    if (!open) return;
    const next = pool ? formFrom(pool) : blankForm();
    setForm(next);
    setMemberOrder(orderedAccountIds(accounts, next.members));
    setLocalKeys([]);
    setMoved(new Set());
    setRevoked(new Set());
    setBusy(false);
    setError(null);
    setAttempted(false);
    setChosen('identity');
    // `accounts` is read but not tracked: the parent hands down a fresh array on
    // every render, and reacting to that would wipe the form mid-edit.
  }, [open, pool]);
  // The server's refusal describes the form it was sent; every control writes through `setForm`, so the next edit
  // takes it away, while a failed save changes no field and leaves it standing.
  useEffect(() => setError(null), [form]);
  // The reset effect seeds the order once per open from the list it found, so a list that lands later, a slow first
  // read or Try again after a failed one, seeds an order still empty. Keyed on the list alone: every member edit
  // re-sorts the order through its own handler. The updater reads the order the reset effect queued in the same
  // commit, since this render's form is still the one from before the open.
  useEffect(() => {
    if (open && accounts.length > 0) setMemberOrder((order) => (order.length > 0 ? order : orderedAccountIds(accounts, form.members)));
  }, [open, accounts]);

  const set = <K extends keyof PoolForm>(key: K, value: PoolForm[K]) => setForm((f) => ({ ...f, [key]: value }));
  const poolKeys = (pool ? [...localKeys, ...keys.filter((k) => k.poolId === pool.id && !localKeys.some((l) => l.id === k.id))] : localKeys).filter((k) => !moved.has(k.id));
  const otherPools = pools.filter((p) => p.id !== pool?.id);
  // Keys created before the pool is saved use the id the pool will get from its name.
  const draftPoolId = form.name.trim().toLowerCase().replace(/[^a-z0-9]+/g, '-') || 'new';
  const draftName = form.name.trim() || 'new pool';
  // Memoized so the key dialog's reset effect does not fire on every keystroke in this form.
  const draftCaptureHours = form.capture ? Number(form.captureHours) || 0 : 0;
  const keyDialogPools = useMemo(
    () =>
      pool || pools.some((p) => p.id === draftPoolId)
        ? pools
        : [...pools, { autoModeServer: form.autoModeServer, bodyCaptureHours: draftCaptureHours, id: draftPoolId, members: Object.keys(form.members).map((accountId) => ({ accountId })), modelRules: form.modelRules, name: draftName }],
    [pool, pools, draftCaptureHours, draftPoolId, draftName, form.autoModeServer, form.members, form.modelRules],
  );
  const suggestions = useCatalogSuggestions(Object.keys(form.members), open);
  // Below five minutes a pin expires before the probe it scheduled can help.
  const keepaliveBlocked = form.affinity < 300;
  const shortAffinity = form.affinity >= 300 && form.affinity < 3_600;
  const warming = form.keepalive && !keepaliveBlocked;
  // Sessions by warm prefix and subagents by cohort both place by the shared prefix, so either one reads the threshold.
  const prefixEnabled = form.sessionPlacement === 'by_warm_prefix' || form.subagentPlacement === 'by_cohort';
  const budgetOf = (accountId: string) => form.budgets[accountId] ?? { daily: '', monthly: '' };
  const input: PoolInput = {
    ...form.engine,
    affinityMode: form.affinityMode,
    affinityTtlSeconds: form.affinity,
    autoModeServer: form.autoModeServer,
    bodyCaptureHours: form.capture ? Number(form.captureHours) : 0,
    cacheDiagnostics: form.cacheDiagnostics,
    cacheTtl: form.cacheTtl,
    // With warming off the block is disabled, so what it holds goes back as stored: an edit left in it can
    // neither reach the policy nor hold up the save.
    keepalive: warming
      ? { ...form.tuning, costCapUsdPerDay: form.capEnabled ? form.capUsd : null, enabled: true, warmSubagents: form.warmSubagents }
      : { ...(pool?.keepalive ?? { ...KEEPALIVE_TUNING_DEFAULTS, costCapUsdPerDay: null }), enabled: false },
    members: Object.entries(form.members).map(([accountId, priority]) => {
      const budget = budgetOf(accountId);
      // A field that reads no number goes on as NaN, which the refusals below hold the save for.
      const usd = (typed: string) => (typed.trim() === '' ? null : Number(typed));
      return { accountId, budgetDailyUsd: usd(budget.daily), budgetMonthlyUsd: usd(budget.monthly), priority };
    }),
    migrateAboveUtilization: form.migrateOn ? form.migrate / 100 : null,
    // With neither placement reading the threshold it goes back as stored, so an edit left in it holds nothing.
    minSharedPrefixTokens: prefixEnabled
      ? form.prefixTokens.trim() === '' ? Number.NaN : Number(form.prefixTokens)
      : (pool?.minSharedPrefixTokens ?? POOL_AFFINITY_DEFAULTS.minSharedPrefixTokens),
    modelRules: form.modelRules,
    name: form.name.trim(),
    overagePolicy: form.overage,
    preferSubscription: form.preferSubscription,
    quotaHeaders: form.quotaHeaders,
    selectBelowUtilization: form.selectBelowOn ? form.selectBelow / 100 : null,
    sessionPlacement: form.sessionPlacement,
    status: form.status,
    strategy: form.strategy,
    subagentPlacement: form.subagentPlacement,
  };
  const listed = new Set(accounts.map((a) => a.id));
  // Until the list answers every stored member stands, so nothing is refused for an id the list may yet hold.
  const refusals = poolRefusals(input, accountsFailed !== null ? null : accountsLoaded ? listed : new Set(Object.keys(form.members)), {
    capture: { maxHours: captureMaxHours, on: form.capture, typed: form.captureHours },
  });
  const refusedFields = new Set(refusals.map((r) => r.field));
  const refusedGroups = new Set<PoolGroup>(refusals.map((r) => r.group));
  const invalid = (field: string) => (refusedFields.has(field) ? true : undefined);
  const refusalOf = (field: string) => refusals.find((r) => r.field === field)?.text ?? null;
  const first = refusals[0] ?? null;
  const valid = first === null;

  const orderIndex = new Map(memberOrder.map((id, i) => [id, i]));
  const orderedAccounts = [...accounts].sort((a, b) => (orderIndex.get(a.id) ?? accounts.length) - (orderIndex.get(b.id) ?? accounts.length) || a.label.localeCompare(b.label));
  // A stored member the list does not hold gets a row of its own, the only place it can be unticked.
  const unlisted = accountsFailed === null ? Object.keys(form.members).filter((id) => !listed.has(id)).sort() : [];

  const setTuning = <K extends NumericTuningKey>(key: K, value: number) => setForm((f) => ({ ...f, tuning: { ...f.tuning, [key]: value } }));
  const setEngine = <K extends keyof PoolEngineFields>(key: K, value: PoolEngineFields[K]) => setForm((f) => ({ ...f, engine: { ...f.engine, [key]: value } }));
  const sheddingPill: SheddingPreset = form.sheddingCustom ? 'custom' : sheddingPresetOf(form.engine);
  const failoverPill: FailoverPreset = form.failoverCustom ? 'custom' : failoverPresetOf(form.engine);
  const pickShedding = (name: SheddingPreset) =>
    setForm((f) => (name === 'custom' ? { ...f, sheddingCustom: true } : { ...f, engine: { ...f.engine, ...SHEDDING_PRESETS[name] }, sheddingCustom: false }));
  const pickFailover = (name: FailoverPreset) =>
    setForm((f) => (name === 'custom' ? { ...f, failoverCustom: true } : { ...f, engine: { ...f.engine, ...FAILOVER_PRESETS[name] }, failoverCustom: false }));
  const rulesRefusal = refusalOf('Model rules');
  // The band's own sentence alone: a select-below out of its range reads in the footer and on the rail.
  const band = bandRefusal(input);
  const prefixRefusal = refusalOf('Minimum shared prefix, tokens');
  const budgetRefusal = refusals.find((r) => r.field.startsWith('budget_daily:') || r.field.startsWith('budget_monthly:'));
  const budgetRefused = budgetRefusal && {
    label: accounts.find((a) => budgetRefusal.field === `budget_daily:${a.id}` || budgetRefusal.field === `budget_monthly:${a.id}`)?.label ?? budgetRefusal.field,
    refusal: budgetRefusal.text,
  };
  const memberAccounts = accounts.filter((a) => a.id in form.members);
  const setBudget = (accountId: string, patch: Partial<{ daily: string; monthly: string }>) =>
    setForm((f) => ({ ...f, budgets: { ...f.budgets, [accountId]: { ...(f.budgets[accountId] ?? { daily: '', monthly: '' }), ...patch } } }));
  const readings = memberAccounts.map((a) => ({ fiveHour: windowOf(a, '5h')?.utilization ?? null, id: a.id, label: a.label, sevenDay: windowOf(a, '7d')?.utilization ?? null }));
  // The route's verdict per stored member, so a member in cooldown is no destination for a shed session.
  const verdict = new Map((pool?.members ?? []).map((m) => [m.accountId, m.eligible]));
  const sized = useMemo(() => sizedAccounts(accounts, settings.data?.planTables.cost.tiers ?? {}), [accounts, settings.data]);
  const preview = shedPreview({ ...input, members: input.members.map((m) => ({ ...m, eligible: verdict.get(m.accountId) })) }, accounts, sessions, now, sized);
  const setNetNegative = (value: boolean) => setForm((f) => ({ ...f, tuning: { ...f.tuning, stopWhenNetNegative: value } }));
  const setCacheTtl = <K extends keyof PoolCacheTtl>(key: K, value: PoolCacheTtl[K]) => setForm((f) => ({ ...f, cacheTtl: { ...f.cacheTtl, [key]: value } }));

  const startMove = (k: ApiKey) => {
    setMoveTo(otherPools[0]?.id ?? '');
    setMoveError(null);
    setMoving(k);
  };

  const moveKey = async () => {
    if (!moving || !moveTo) return;
    setMoveBusy(true);
    setMoveError(null);
    try {
      await api.updateKey(moving.id, {
        affinitySource: moving.affinitySource,
        autoModeServer: moving.autoModeServer,
        bodyCaptureHours: moving.bodyCaptureHours,
        enabled: moving.enabled,
        keepaliveOptOut: moving.keepaliveOptOut,
        longContextTwins: moving.longContextTwins,
        modelRules: moving.modelRules,
        modelRulesMode: moving.modelRulesMode,
        name: moving.name,
        notes: moving.notes,
        poolId: moveTo,
      });
      setMoved((prev) => new Set(prev).add(moving.id));
      setMoving(null);
      onKeysChanged();
    } catch (e) {
      setMoveError(e instanceof Error ? e.message : String(e));
    } finally {
      setMoveBusy(false);
    }
  };

  const revokeKey = async (k: ApiKey) => {
    try {
      await api.revokeKey(k.id);
      setRevoked((prev) => new Set(prev).add(k.id));
      onKeysChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      if (pool) await api.updatePool(pool.id, input);
      else await api.createPool(input);
    } catch (e) {
      // Stated inside the dialog, which stays open: it sits in the browser's top layer, where a toast raised
      // behind its backdrop would never be seen, and the edit that was refused is still on screen.
      setBusy(false);
      setError(e instanceof Error ? e.message : String(e));
      if (e instanceof ApiError && e.status >= 400) setAttempted(true);
      return;
    }
    setBusy(false);
    onSaved(pool === null);
  };

  const refusalCallout = (text: ReactNode) => (
    <div className="callout callout--critical" role="alert">
      <IconWarning size={16} />
      <div>{text}</div>
    </div>
  );
  const probeField = (key: NumericTuningKey, className?: string, hint?: ReactNode) => (
    <Field key={key} label={KEEPALIVE_FIELD_LABELS[key]} className={className} hint={hint}>
      <Input
        type="number"
        min={KEEPALIVE_BOUNDS[key].min}
        max={KEEPALIVE_BOUNDS[key].max}
        value={form.tuning[key]}
        onChange={(e) => setTuning(key, Number(e.target.value))}
        disabled={!warming}
        aria-invalid={invalid(KEEPALIVE_FIELD_LABELS[key])}
      />
    </Field>
  );

  // Keyed by group, so a group without a body fails to compile; `POOL_GROUPS` sets the rail order.
  const bodies: Record<PoolGroup, ReactNode> = {
    identity: (
      <div className="cells">
        <Field label="Name" className="cell--half" hint="The pool's id is made from this name when the pool is created, and stays when it is renamed.">
          <Input value={form.name} onChange={(e) => set('name', e.target.value)} placeholder="research" className="mono" aria-invalid={invalid('Name')} />
        </Field>
        <Field label="Status" className="cell--half" hint="A disabled pool keeps its keys and members but stops accepting requests.">
          <Select value={form.status} onChange={(e) => set('status', e.target.value as 'active' | 'disabled')}>
            <option value="active">Active</option>
            <option value="disabled">Disabled</option>
          </Select>
        </Field>
      </div>
    ),
    members: (
      <div className="cells">
        <p className="field__hint">
          Lower numbers are tried first: the strategy picks inside the first tier that has an eligible account, after the kind order that <GroupLink id="selection">Prioritize subscription accounts</GroupLink> sets. Each account&apos;s windows and state are on the{' '}
          <PageLink to="/accounts">Accounts page</PageLink>.
        </p>
        <p className="field__hint">
          A member whose spend reaches its daily or its monthly budget is skipped until that UTC day or month ends, priced at API list rates: an Anthropic-key member&apos;s whole spend counts, a subscription member&apos;s metered spend alone (overage and fast answers); set one, both, or neither.
        </p>
        {accountsFailed !== null ? (
          <ErrorState
            title="The account list did not load"
            body={accountsFailed}
            action={
              <Button size="sm" onClick={onRetryAccounts}>
                Try again
              </Button>
            }
          />
        ) : (
          <div className="table-wrap">
            <table className="table members-table">
              <thead>
                <tr>
                  <th>Account</th>
                  <th className="right">Priority</th>
                  <th className="right">Daily budget, USD</th>
                  <th className="right">Monthly budget, USD</th>
                </tr>
              </thead>
              <tbody>
                {orderedAccounts.map((a) => {
                  const on = a.id in form.members;
                  return (
                    <tr key={a.id}>
                      <td>
                        <label>
                          <input
                            type="checkbox"
                            checked={on}
                            onChange={(e) => {
                              const next = { ...form.members };
                              if (e.target.checked) next[a.id] = 0;
                              else delete next[a.id];
                              set('members', next);
                              setMemberOrder(orderedAccountIds(accounts, next));
                            }}
                          />
                          <span>{a.label}</span>
                          {a.email && <span className="muted mono">{a.email}</span>}
                        </label>
                      </td>
                      <td className="right">
                        <Input
                          type="number"
                          min={0}
                          max={9}
                          value={on ? form.members[a.id] : ''}
                          disabled={!on}
                          onChange={(e) => set('members', { ...form.members, [a.id]: Number(e.target.value) })}
                          onBlur={() => setMemberOrder(orderedAccountIds(accounts, form.members))}
                          onKeyDown={(e) => {
                            if (e.key === 'Enter') setMemberOrder(orderedAccountIds(accounts, form.members));
                          }}
                          aria-label={`Priority for ${a.label}`}
                          aria-invalid={invalid(`priority:${a.id}`)}
                        />
                      </td>
                      <td className="right">
                        <Input
                          type="number"
                          min={0.01}
                          step="any"
                          value={budgetOf(a.id).daily}
                          disabled={!on}
                          placeholder="none"
                          onChange={(e) => setBudget(a.id, { daily: e.target.value })}
                          aria-label={`Daily budget for ${a.label}, USD`}
                          aria-invalid={invalid(`budget_daily:${a.id}`)}
                        />
                      </td>
                      <td className="right">
                        <Input
                          type="number"
                          min={0.01}
                          step="any"
                          value={budgetOf(a.id).monthly}
                          disabled={!on}
                          placeholder="none"
                          onChange={(e) => setBudget(a.id, { monthly: e.target.value })}
                          aria-label={`Monthly budget for ${a.label}, USD`}
                          aria-invalid={invalid(`budget_monthly:${a.id}`)}
                        />
                      </td>
                    </tr>
                  );
                })}
                {unlisted.map((id) => (
                  <tr key={id}>
                    <td>
                      <label>
                        <input
                          type="checkbox"
                          checked
                          onChange={() => {
                            const next = { ...form.members };
                            delete next[id];
                            set('members', next);
                          }}
                          aria-invalid={invalid(`member:${id}`)}
                        />
                        <span>{id}</span>
                        {accountsLoaded && <Badge tone="neutral">not listed</Badge>}
                      </label>
                    </td>
                    <td className="right">
                      <Input type="number" value={form.members[id]} disabled aria-label={`Priority for ${id}`} />
                    </td>
                    <td className="right">
                      <Input type="number" value={budgetOf(id).daily} disabled placeholder="none" aria-label={`Daily budget for ${id}, USD`} />
                    </td>
                    <td className="right">
                      <Input type="number" value={budgetOf(id).monthly} disabled placeholder="none" aria-label={`Monthly budget for ${id}, USD`} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        {budgetRefused &&
          refusalCallout(
            <>
              {budgetRefused.label}: {budgetRefused.refusal}
            </>,
          )}
      </div>
    ),
    keys: (
      <>
        <div className="cells">
          <div className="row">
            {/* A key is posted against a pool id, so there is nothing to create one on until the pool exists. */}
            <Button size="sm" icon={<IconPlus size={13} />} onClick={() => setKeyOpen(true)} disabled={!pool}>
              Create key
            </Button>
          </div>
          {poolKeys.length === 0 ? (
            <p className="muted">{pool ? 'No keys belong to this pool yet.' : 'Keys can be created once the pool is saved.'}</p>
          ) : (
            <div className="table-wrap">
              <table className="table keys-table">
                <thead>
                  <tr>
                    <th>Key</th>
                    <th>Prefix</th>
                    <th>Status</th>
                    <th />
                  </tr>
                </thead>
                <tbody>
                  {poolKeys.map((k) => {
                    const status = revoked.has(k.id) ? 'revoked' : k.status;
                    return (
                      <tr key={k.id}>
                        <td>{k.name}</td>
                        <td className="mono muted">{k.prefix}…</td>
                        <td>
                          <Badge tone={KEY_STATUS_TONE[status]} dot>
                            {status}
                          </Badge>
                        </td>
                        <td>
                          <Menu
                            label={`Actions for key ${k.name}`}
                            items={[
                              { label: 'Move to…', disabled: otherPools.length === 0, onClick: () => startMove(k) },
                              { label: 'Revoke', danger: true, disabled: status !== 'active', onClick: () => setRevoking(k) },
                            ]}
                          />
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
          <p className="field__hint">
            A key reaches only this pool&apos;s members. Create key, Move to and Revoke apply at once, without Save pool; rotation and expiry are on the <PageLink to="/keys">Keys page</PageLink>.
          </p>
        </div>
      </>
    ),
    models: (
      <div className="cells">
        {/* Field's frame without its label element: a label forwards a click on its text to the first control
            inside, which here would press Add rule or flip the first row. */}
        <Field
          label="Model rules"
          plain
          hint={
            <>
              Top to bottom, first match wins; a model no rule names is allowed, and a closing Deny * allows only what the list names. Write an undated id with a trailing *. A key extends or replaces these rules; the{' '}
              <PageLink to="/models">Models page</PageLink> shows the list a key ends up with.
            </>
          }
        >
          <PatternEditor label="Model rules" value={form.modelRules} onChange={(rules) => set('modelRules', rules)} suggestions={suggestions} />
        </Field>
        {rulesRefusal && refusalCallout(rulesRefusal)}
      </div>
    ),
    autoMode: (
      <div className="cells">
        <Field
          label="Support server-side auto mode"
          beside
          hint={
            <>
              Claude Code asks the upstream to review each tool use on the model&apos;s own turn. Off, the router drops that request from every message of this pool&apos;s keys, and the client reviews with classifier requests of its own, billed to the account&apos;s windows; a key can override this. The Client page of the documentation explains what the client does in each state.
            </>
          }
        >
          <Toggle checked={form.autoModeServer} onChange={(v) => set('autoModeServer', v)} label="Support server-side auto mode" />
        </Field>
      </div>
    ),
    selection: (
      <div className="cells">
        <Field label="Strategy" hint={STRATEGY_HINT[form.strategy]}>
          <Select value={form.strategy} onChange={(e) => set('strategy', e.target.value as PoolStrategy)} aria-label="Strategy">
            {(Object.keys(STRATEGY_LABEL) as PoolStrategy[]).map((v) => (
              <option key={v} value={v}>
                {STRATEGY_LABEL[v]}
              </option>
            ))}
          </Select>
        </Field>
        <Field
          label="Prioritize subscription accounts"
          beside
          hint={
            <>
              Plan quota first, then accounts in <GroupLink id="overage">overage</GroupLink>, then Anthropic keys; <GroupLink id="members">priority</GroupLink> orders each of those groups.
            </>
          }
        >
          <Toggle checked={form.preferSubscription} onChange={(v) => set('preferSubscription', v)} label="Prioritize subscription accounts" />
        </Field>
      </div>
    ),
    placement: (
      <div className="cells">
        <Field
          label="Subagent placement"
          hint={
            <>
              Where a subagent&apos;s first request lands. By cohort: with the subagents that share its static prefix. With the main conversation: on its parent&apos;s account. By strategy: wherever the <GroupLink id="selection">strategy</GroupLink> picks.
            </>
          }
        >
          <Select value={form.subagentPlacement} onChange={(e) => set('subagentPlacement', e.target.value as SubagentPlacement)} aria-label="Subagent placement" aria-invalid={invalid('Subagent placement')}>
            <option value="by_cohort">By cohort</option>
            <option value="by_strategy">By strategy</option>
            <option value="with_main">With the main conversation</option>
          </Select>
        </Field>
        <Field
          label="Session placement"
          hint={
            <>
              By warm prefix lands a fresh session where its static prefix is already cached, when the prefix reaches the minimum below; By strategy leaves it to the <GroupLink id="selection">strategy</GroupLink>.
            </>
          }
        >
          <Select value={form.sessionPlacement} onChange={(e) => set('sessionPlacement', e.target.value as SessionPlacement)} aria-label="Session placement" aria-invalid={invalid('Session placement')}>
            <option value="by_strategy">By strategy</option>
            <option value="by_warm_prefix">By warm prefix</option>
          </Select>
        </Field>
        <Field
          label="Minimum shared prefix, tokens"
          hint={
            <>
              The smallest shared prefix that places a fresh session, or a subagent by cohort, by warmth; below it the <GroupLink id="selection">strategy</GroupLink> decides. Estimated from the request&apos;s bytes, not counted by the upstream.
            </>
          }
        >
          <Input
            type="number"
            min={0}
            step={1_000}
            value={form.prefixTokens}
            onChange={(e) => set('prefixTokens', e.target.value)}
            disabled={!prefixEnabled}
            aria-label="Minimum shared prefix, tokens"
            aria-invalid={invalid('Minimum shared prefix, tokens')}
          />
        </Field>
        {prefixRefusal && refusalCallout(prefixRefusal)}
      </div>
    ),
    pinning: (
      <div className="cells">
        <Field
          label="Session affinity"
          plain
          hint={
            <>
              {AFFINITY_HINT} <GroupLink id="keepalive">Keepalive</GroupLink> needs at least 5m and warns under 1h.
            </>
          }
        >
          <span className="chips" role="group" aria-label="Session affinity">
            {AFFINITY_PICKS.map((pick) => {
              const on = !form.customAffinity && form.affinity === pick.seconds;
              return (
                <button key={pick.label} type="button" className={`pill ${on ? 'is-active' : ''}`} aria-pressed={on} onClick={() => setForm((f) => ({ ...f, affinity: pick.seconds, customAffinity: false }))}>
                  {pick.label}
                </button>
              );
            })}
            <button type="button" className={`pill ${form.customAffinity ? 'is-active' : ''}`} aria-pressed={form.customAffinity} onClick={() => set('customAffinity', true)}>
              custom
            </button>
          </span>
          <Input
            type="number"
            min={60}
            step={30}
            value={form.affinity}
            onChange={(e) => set('affinity', Math.max(0, Number(e.target.value)))}
            disabled={!form.customAffinity}
            aria-label="Session affinity in seconds"
            aria-invalid={invalid('Session affinity in seconds')}
          />
        </Field>
        <Field
          label="Pin sessions"
          hint={
            <>
              Only when the request caches: a request with no cache marker, and none added by the <GroupLink id="cacheTtl">cache TTL policy</GroupLink>, lands by the <GroupLink id="selection">strategy</GroupLink> and takes no pin. A request bound to its account keeps its pin either way.
            </>
          }
        >
          <Select value={form.affinityMode} onChange={(e) => set('affinityMode', e.target.value as AffinityMode)} aria-label="Pin sessions" aria-invalid={invalid('Pin sessions')}>
            <option value="always">Always</option>
            <option value="when_cached">Only when the request caches</option>
          </Select>
        </Field>
      </div>
    ),
    utilization: (
      <div className="cells">
        <div className="cell--half">
          <SwitchedNumber
            label="Migrate above utilization"
            numberLabel="Utilization (%)"
            checked={form.migrateOn}
            onChange={(v) => set('migrateOn', v)}
            input={{ min: 50, max: 100, value: form.migrate, onChange: (e) => set('migrate', Number(e.target.value)), 'aria-label': 'Migrate above utilization (%)', 'aria-invalid': invalid('Migrate above utilization (%)') }}
            hint={
              <>
                A member at or above this line on its 5-hour or 7-day window starts a <GroupLink id="shedding">shedding round</GroupLink>; a sibling counts as cool below the line minus 5%.
              </>
            }
          />
        </div>
        <div className="cell--half">
          <SwitchedNumber
            label="Select below utilization"
            numberLabel="Utilization (%)"
            checked={form.selectBelowOn}
            onChange={(v) => set('selectBelowOn', v)}
            input={{ min: 0, max: 100, value: form.selectBelow, onChange: (e) => set('selectBelow', Number(e.target.value)), 'aria-label': 'Select below utilization (%)', 'aria-invalid': invalid('Select below utilization (%)') }}
            hint={
              <>
                A <GroupLink id="selection">fresh pick</GroupLink> passes over members at or above this 5-hour utilization, unless every member is. With both lines on, it sits at least 5% under the migrate line.
              </>
            }
          />
        </div>
        <Field
          label="Member utilization"
          plain
          hint={
            <>
              Each tick is a member&apos;s latest 5-hour or 7-day reading, as on the <PageLink to="/quota">Quota page</PageLink>. Drag a threshold&apos;s handle, or focus it and use the arrow keys.
            </>
          }
        >
          <UtilizationBar
            readings={readings}
            migrate={form.migrateOn ? { onChange: (v) => set('migrate', v), value: form.migrate } : null}
            migrateMin={50}
            selectBelow={form.selectBelowOn ? { onChange: (v) => set('selectBelow', v), value: form.selectBelow } : null}
          />
        </Field>
        {band && refusalCallout(band)}
      </div>
    ),
    shedding: (
      <>
        <p className="dialog-group__intro">
          A member at or above the <GroupLink id="utilization">migrate line</GroupLink> runs a round that marks some of its sessions; each moves to a cooler sibling at its next request.
        </p>
        <div className="cells">
          <Field label="Preset" plain hint={SHEDDING_HINT[sheddingPill]}>
            <PresetPills label="Load shedding preset" value={sheddingPill} onPick={pickShedding} hints={SHEDDING_HINT} />
          </Field>
          <Field label="Share (%)" className="cell--half" hint="The part of the member's recent burn one round marks to move.">
            <Input type="number" min={0} max={100} value={percentOf(form.engine.shedShare)} onChange={(e) => setEngine('shedShare', Number(e.target.value) / 100)} disabled={sheddingPill !== 'custom'} aria-invalid={invalid('Share (%)')} />
          </Field>
          <Field label={ENGINE_FIELD_LABELS.burnWindowSeconds} className="cell--half" hint="The span each session's burn is averaged over.">
            <Input
              type="number"
              min={ENGINE_BOUNDS.burnWindowSeconds.min}
              step={60}
              value={form.engine.burnWindowSeconds}
              onChange={(e) => setEngine('burnWindowSeconds', Number(e.target.value))}
              disabled={sheddingPill !== 'custom'}
              aria-invalid={invalid(ENGINE_FIELD_LABELS.burnWindowSeconds)}
            />
          </Field>
          <Field label={ENGINE_FIELD_LABELS.sourceDwellSeconds} className="cell--half" hint="The least time between two rounds on one member, and how long a moved session stays off it.">
            <Input
              type="number"
              min={ENGINE_BOUNDS.sourceDwellSeconds.min}
              step={60}
              value={form.engine.sourceDwellSeconds}
              onChange={(e) => setEngine('sourceDwellSeconds', Number(e.target.value))}
              disabled={sheddingPill !== 'custom'}
              aria-invalid={invalid(ENGINE_FIELD_LABELS.sourceDwellSeconds)}
            />
          </Field>
          <Field label="Migrate when spent" beside hint="Moves a session off a member whose window for its model is spent, to a sibling with allowance left, without waiting for a round or the upstream's 429.">
            <Toggle checked={form.engine.migrateWhenSpent} onChange={(v) => setEngine('migrateWhenSpent', v)} label="Migrate when spent" />
          </Field>
          <Field
            label="Warm sessions"
            plain
            hint={
              <>
                A warm session rewrites its cache where it lands; the <CostReferenceLink /> shows what that write costs.
              </>
            }
          >
            <div className="radio-list" role="radiogroup" aria-label="Warm sessions" aria-invalid={invalid('Warm sessions')}>
              {WARM_CHOICES.map((c) => (
                <label key={c.value} className={`radio-list__item ${form.engine.warmSessions === c.value ? 'is-active' : ''}`}>
                  <input type="radio" name={`warm-${pool?.id ?? 'new'}`} checked={form.engine.warmSessions === c.value} onChange={() => setEngine('warmSessions', c.value)} />
                  <span>
                    <b>{c.title}:</b> {c.body}
                  </span>
                </label>
              ))}
            </div>
          </Field>
          <Field label="Preview" plain>
            <p className="shed-preview num" role="status">
              {form.migrateOn ? (
                shedPreviewLine(preview, true)
              ) : (
                <>
                  No round runs while <GroupLink id="utilization">migrate above utilization</GroupLink> is off.
                </>
              )}
            </p>
          </Field>
        </div>
      </>
    ),
    failover: (
      <>
        <p className="dialog-group__intro">
          How long a request waits out a throttle on its member, how many requests one member serves at once, and how far <GroupLink id="selection">Pace</GroupLink> spends a member before it is the last resort.
        </p>
        <div className="cells">
          <Field label="Preset" plain hint={FAILOVER_HINT[failoverPill]}>
            <PresetPills label="Failover preset" value={failoverPill} onPick={pickFailover} hints={FAILOVER_HINT} />
          </Field>
          <Field
            label={ENGINE_FIELD_LABELS.inFlightCap}
            className="cell--half"
            hint={
              <>
                Requests one member serves at once. A member at the cap takes no fresh session while a sibling has room; with every member full, a request waits up to Retry in place for a slot. A key&apos;s own cap is on the{' '}
                <PageLink to="/keys">Keys page</PageLink>.
              </>
            }
          >
            <Input
              type="number"
              min={ENGINE_BOUNDS.inFlightCap.min}
              value={form.engine.inFlightCap}
              onChange={(e) => setEngine('inFlightCap', Number(e.target.value))}
              disabled={failoverPill !== 'custom'}
              aria-invalid={invalid(ENGINE_FIELD_LABELS.inFlightCap)}
            />
          </Field>
          <Field label={ENGINE_FIELD_LABELS.retryInPlaceSeconds} className="cell--half" hint="The longest throttle a request waits out on its member before failing over, and the longest wait for a free slot; 0 fails over at once.">
            <Input
              type="number"
              min={ENGINE_BOUNDS.retryInPlaceSeconds.min}
              max={ENGINE_BOUNDS.retryInPlaceSeconds.max}
              value={form.engine.retryInPlaceSeconds}
              onChange={(e) => setEngine('retryInPlaceSeconds', Number(e.target.value))}
              disabled={failoverPill !== 'custom'}
              aria-invalid={invalid(ENGINE_FIELD_LABELS.retryInPlaceSeconds)}
            />
          </Field>
          <Field
            label="Pace floor (%)"
            className="cell--half"
            hint={
              <>
                Under <GroupLink id="selection">Pace</GroupLink>, a member with this share of its window left or less serves only as the last resort.
              </>
            }
          >
            <Input type="number" min={0} max={100} value={percentOf(form.engine.paceFloor)} onChange={(e) => setEngine('paceFloor', Number(e.target.value) / 100)} disabled={failoverPill !== 'custom'} aria-invalid={invalid('Pace floor (%)')} />
          </Field>
        </div>
      </>
    ),
    overage: (
      <div className="cells">
        <Field
          label="Overage policy"
          plain
          hint={
            <>
              For subscription accounts billing extra usage. A daily or monthly budget in <GroupLink id="members">Members</GroupLink> caps a member&apos;s spend in dollars: an Anthropic-key member&apos;s whole spend, a subscription member&apos;s overage and fast answers. The <PageLink to="/accounts">Accounts page</PageLink> shows which account is in overage.
            </>
          }
        >
          <div className="radio-list" role="radiogroup" aria-label="Overage policy">
            {OVERAGE_CHOICES.map((c) => (
              <label key={c.value} className={`radio-list__item ${form.overage === c.value ? 'is-active' : ''}`}>
                <input type="radio" name={`overage-${pool?.id ?? 'new'}`} checked={form.overage === c.value} onChange={() => set('overage', c.value)} />
                <span>
                  <b>{c.title}:</b> {c.body}
                </span>
              </label>
            ))}
          </div>
        </Field>
        <Field label={ENGINE_FIELD_LABELS.overageMoveGraceMinutes} hint="Under Prefer plan quota, a session stays on an account in overage while its window resets within this many minutes; 0 moves it at once.">
          <Input
            type="number"
            min={ENGINE_BOUNDS.overageMoveGraceMinutes.min}
            max={ENGINE_BOUNDS.overageMoveGraceMinutes.max}
            value={form.engine.overageMoveGraceMinutes}
            onChange={(e) => setEngine('overageMoveGraceMinutes', Number(e.target.value))}
            aria-invalid={invalid(ENGINE_FIELD_LABELS.overageMoveGraceMinutes)}
          />
        </Field>
        <Field
          label={ENGINE_FIELD_LABELS.windDownMinutes}
          hint={
            <>
              This long before a cancelled subscription ends, its account takes new sessions only as the last resort and cold sessions move off. The end is set on the <PageLink to="/accounts">Accounts page</PageLink>.
            </>
          }
        >
          <Input
            type="number"
            min={ENGINE_BOUNDS.windDownMinutes.min}
            max={ENGINE_BOUNDS.windDownMinutes.max}
            value={form.engine.windDownMinutes}
            onChange={(e) => setEngine('windDownMinutes', Number(e.target.value))}
            aria-invalid={invalid(ENGINE_FIELD_LABELS.windDownMinutes)}
          />
        </Field>
      </div>
    ),
    cacheTtl: (
      <div className="cells">
        <Field label="Set the cache TTL for this pool" beside hint="Rewrites every cache marker a request carries to the TTL below, on every account of this pool, so the client's own TTL setting has no effect through it.">
          <Toggle checked={form.cacheTtl.enabled} onChange={(v) => setCacheTtl('enabled', v)} label="Set the cache TTL for this pool" />
        </Field>
        <Field
          label="Add a cache marker to a request without one"
          beside
          hint={
            <>
              Adds a marker to a request that carries none; off leaves such a request as sent. <GroupLink id="pinning">Pin sessions</GroupLink> counts an added marker.
            </>
          }
        >
          <Toggle checked={form.cacheTtl.injectWhenAbsent} onChange={(v) => setCacheTtl('injectWhenAbsent', v)} label="Add a cache marker to a request without one" disabled={!form.cacheTtl.enabled} />
        </Field>
        <Field
          label="Write cost"
          plain
          hint="Plan writes apply while an account spends plan quota, where a 1-hour write costs the same as a 5-minute one. Metered writes apply while a write costs money, on an Anthropic-key account or a subscription account in overage: 2x input for an hour against 1.25x for five minutes. Reads cost the same either way."
        >
          <div className="row">
            <CostReferenceLink />
          </div>
        </Field>
        <CacheTtlGrid
          disabled={!form.cacheTtl.enabled}
          plan={form.cacheTtl.plan}
          metered={form.cacheTtl.metered}
          onChange={(name, rules) => setCacheTtl(name, rules)}
          hint={
            <>
              The TTL each turn writes with, by agent. A <GroupLink id="keepalive">keepalive probe</GroupLink> replays the turn under its agent&apos;s TTL, so the first probe after a turn and every later one follow the same rule.
            </>
          }
        />
        <CacheTtlOverrides
          key={`plan-${open}-${pool?.id ?? 'new'}`}
          name="plan"
          label="Plan writes"
          disabled={!form.cacheTtl.enabled}
          rules={form.cacheTtl.plan}
          onChange={(rules) => setCacheTtl('plan', rules)}
          models={models}
        />
        <CacheTtlOverrides
          key={`metered-${open}-${pool?.id ?? 'new'}`}
          name="metered"
          label="Metered writes"
          disabled={!form.cacheTtl.enabled}
          rules={form.cacheTtl.metered}
          onChange={(rules) => setCacheTtl('metered', rules)}
          models={models}
        />
      </div>
    ),
    keepalive: (
      <>
        <div className="cells">
          <Field
            label="Keep prompt caches warm"
            beside
            hint={
              <>
                Probes an idle session shortly before its cache expires, so its next turn reads instead of writes. A probe follows the session&apos;s pin, so <GroupLink id="pinning">Session affinity</GroupLink> must be at least 5m. Subscription accounts also wait for OAuth prewarm verified on the{' '}
                <PageLink to="/settings">Settings page</PageLink>.
                {keepaliveBlocked && (
                  <p className="field__hint">
                    Needs Session affinity of at least 5 minutes: a probe follows the session&apos;s pin, and a shorter pin cannot keep the session on the account it warmed. Set it under <GroupLink id="pinning">Pinning</GroupLink>.
                  </p>
                )}
              </>
            }
          >
            <Toggle checked={warming} onChange={(v) => set('keepalive', v)} label="Keep prompt caches warm" disabled={keepaliveBlocked} />
          </Field>
          <Field label="Also warm subagent caches" beside hint="Off probes the main conversation only.">
            <Toggle checked={form.warmSubagents} onChange={(v) => set('warmSubagents', v)} label="Also warm subagent caches" disabled={!warming} />
          </Field>
          <Field
            label="Stop when probes cost more than they save"
            beside
            className="cell--half"
            hint={
              <>
                Pauses a session&apos;s probes once they have cost more than the cache reads they saved; a real request that lands warm resumes them. Savings per pool are on the{' '}
                <PageLink to="/analytics#cache-warming">Analytics page</PageLink>.
              </>
            }
          >
            <Toggle checked={form.tuning.stopWhenNetNegative} onChange={setNetNegative} label="Stop when probes cost more than they save" disabled={!warming} />
          </Field>
          <Field
            label={KEEPALIVE_FIELD_LABELS.netNegativeGraceProbes}
            className="cell--half"
            hint="Probes a session still gets after its net turns negative, granted again on every real request, so one expensive turn does not stop a session about to earn it back."
          >
            <Input
              type="number"
              min={KEEPALIVE_BOUNDS.netNegativeGraceProbes.min}
              max={KEEPALIVE_BOUNDS.netNegativeGraceProbes.max}
              value={form.tuning.netNegativeGraceProbes}
              onChange={(e) => setTuning('netNegativeGraceProbes', Math.max(0, Number(e.target.value)))}
              disabled={!warming || !form.tuning.stopWhenNetNegative}
              aria-label="Grace probes after the net turns negative"
              aria-invalid={invalid(KEEPALIVE_FIELD_LABELS.netNegativeGraceProbes)}
            />
          </Field>
          <div className="cell--half">
            <SwitchedNumber
              label="Daily probe cost cap ($)"
              disabled={!warming}
              checked={form.capEnabled}
              onChange={(v) => set('capEnabled', v)}
              input={{ min: 0, step: 0.5, value: form.capUsd, onChange: (e) => set('capUsd', Number(e.target.value)), 'aria-label': 'Daily probe cost cap in dollars', 'aria-invalid': invalid('Daily probe cost cap in dollars') }}
              hint={
                <>
                  Per UTC day, for probes paid in dollars: on Anthropic-key accounts, on subscription accounts in overage, and on models whose 5-hour share the cost table cannot price. Plan-funded probes do not count here. The <CostReferenceLink /> prices a probe.
                </>
              }
            />
          </div>
        </div>
        {warming && shortAffinity && (
          <div className="callout callout--warning" role="status">
            <IconWarning size={16} />
            <div>
              Session affinity is {fmtAffinity(form.affinity)}, shorter than one hour: a one-hour cache entry outlives the pin, so a session can move away from the cache it warmed. Raise it under <GroupLink id="pinning">Pinning</GroupLink>.
            </div>
          </div>
        )}
      </>
    ),
    probeTuning: (
      <div className="cells">
        {PROBE_TUNING_ROWS.map((keys) =>
          keys.length === 1 ? (
            probeField(keys[0], undefined, PROBE_HINT[keys[0]])
          ) : (
            <Fragment key={keys[0]}>
              {probeField(keys[0], 'cell--half')}
              {probeField(keys[1], 'cell--half')}
              <p className="field__hint">{PROBE_HINT[keys[0]]}</p>
            </Fragment>
          ),
        )}
      </div>
    ),
    responseHeaders: (
      <div className="cells">
        <Field label="Quota headers" hint={QUOTA_HEADERS_HINT}>
          <Select value={form.quotaHeaders} onChange={(e) => set('quotaHeaders', e.target.value as QuotaHeaders)} aria-label="Quota headers" aria-invalid={invalid('Quota headers')}>
            <option value="pass">Pass through</option>
            <option value="hide">Hide on committed answers</option>
          </Select>
        </Field>
      </div>
    ),
    bodyCapture: (
      <div className="cells">
        <Field
          label="Capture request and response bodies"
          beside
          hint={
            <>
              Keeps the request and response bodies of this pool&apos;s keys for the request drawer on the <PageLink to="/requests">Requests page</PageLink>. A key can override it, and only admins read bodies.
            </>
          }
        >
          <Toggle checked={form.capture} onChange={(v) => set('capture', v)} label="Capture request and response bodies" />
        </Field>
        <Field
          label="Retention (hours)"
          hint={
            <>
              At most the bootstrap&apos;s body capture limit, {captureMaxHours} hours here, shown on the <PageLink to="/settings">Settings page</PageLink>.
            </>
          }
        >
          <Input
            type="number"
            min={1}
            max={captureMaxHours}
            value={form.captureHours}
            onChange={(e) => set('captureHours', e.target.value)}
            disabled={!form.capture}
            aria-label="Capture retention in hours"
            aria-invalid={invalid('Capture retention in hours')}
          />
        </Field>
      </div>
    ),
    cacheDiagnostics: (
      <div className="cells">
        <Field
          label="Cache diagnostics"
          hint={
            <>
              On asks the upstream why a cache read missed, on every account of this pool, and the reason lands in the <PageLink to="/requests">request log</PageLink>. Inherit follows the bootstrap setting on the{' '}
              <PageLink to="/settings">Settings page</PageLink>.
            </>
          }
        >
          <Select value={form.cacheDiagnostics === null ? 'inherit' : form.cacheDiagnostics ? 'on' : 'off'} onChange={(e) => set('cacheDiagnostics', e.target.value === 'inherit' ? null : e.target.value === 'on')}>
            <option value="inherit">Inherit the bootstrap setting</option>
            <option value="on">On</option>
            <option value="off">Off</option>
          </Select>
        </Field>
      </div>
    ),
  };
  const groups: DialogGroup[] = POOL_GROUPS.map((id) => ({ ...POOL_RAIL[id], body: bodies[id], id, refused: refusedGroups.has(id) }));

  return (
    <>
      <GroupedDialog
        open={open}
        onClose={() => onClose(attempted)}
        title={pool ? `Edit pool ${pool.name}` : 'Create pool'}
        railLabel="Pool settings"
        groups={groups}
        chosen={chosen}
        onChoose={(id) => setChosen(id as PoolGroup)}
        // The line always explains the current hold: a refusal of the form's own beats a server message left from
        // before the edit.
        status={first ? `Save is held: ${first.text}` : (error ?? 'Every group saves together.')}
        hold={first?.group ?? null}
        held={first !== null || error !== null}
        footer={
          <>
            <Button variant="ghost" onClick={() => onClose(attempted)}>
              Cancel
            </Button>
            <Button variant="primary" disabled={!valid || busy} onClick={submit}>
              {busy ? 'Saving…' : pool ? 'Save pool' : 'Create pool'}
            </Button>
          </>
        }
      />
      <KeyDialog
        open={keyOpen}
        onClose={() => setKeyOpen(false)}
        pools={keyDialogPools}
        initialPoolId={pool ? pool.id : draftPoolId}
        lockPool
        onCreated={(c) => {
          setKeyOpen(false);
          setLocalKeys((prev) => [c.key, ...prev]);
          setCreated(c);
          onKeyCreated(c);
        }}
      />
      <Dialog
        open={moving !== null}
        onClose={() => setMoving(null)}
        title={moving ? `Move key ${moving.name}` : 'Move key'}
        subtitle="A key reaches only its own pool's members, so moving it ends its current session pins."
        width={440}
        footer={
          <>
            <Button variant="ghost" onClick={() => setMoving(null)}>
              Cancel
            </Button>
            <Button variant="primary" disabled={!moveTo || moveBusy} onClick={moveKey}>
              Move key
            </Button>
          </>
        }
      >
        {moveError && (
          <p className="dialog__status is-held" role="alert">
            {moveError}
          </p>
        )}
        <Field label="Pool">
          <Select value={moveTo} onChange={(e) => setMoveTo(e.target.value)} aria-label="Destination pool">
            {otherPools.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </Select>
        </Field>
      </Dialog>
      <ConfirmDialog
        open={revoking !== null}
        onClose={() => setRevoking(null)}
        title={revoking ? `Revoke ${revoking.name}` : 'Revoke key'}
        confirmLabel="Revoke"
        danger
        body={revoking ? `Requests with ${revoking.name} are rejected immediately. The key stays in this pool and its history stays in the ledger.` : ''}
        onConfirm={() => {
          if (revoking) void revokeKey(revoking);
        }}
      />
      <Dialog
        open={created !== null}
        onClose={() => setCreated(null)}
        title="Key created"
        subtitle="Copy the secret now. The router stores only a hash, so it cannot be shown again."
        footer={
          <Button variant="primary" onClick={() => setCreated(null)}>
            Done
          </Button>
        }
      >
        {created && (
          <div className="secret">
            <code>{created.secret}</code>
            <CopyButton text={created.secret} size="md" />
          </div>
        )}
      </Dialog>
    </>
  );
}
