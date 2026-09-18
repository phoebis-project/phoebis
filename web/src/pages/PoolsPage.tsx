import { useEffect, useMemo, useState } from 'react';
import { Link } from 'react-router-dom';
import { Bar, ComposedChart, Line, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts';
import { api, CACHE_TTL_DEFAULTS, KEEPALIVE_TUNING_DEFAULTS, type PoolInput } from '@/api';
import type { Account, ApiKey, CacheTtl, CreatedApiKey, KeyStatus, OveragePolicy, Pool, PoolCacheTtl, PoolKeepalive, PoolStrategy } from '@/api/types';
import { useAppSettings } from '@/app/settings';
import { useChartInk } from '@/components/charts/useChartInk';
import { ChartTooltip, Legend } from '@/components/charts/Tooltip';
import { CostReferenceLink } from '@/components/ui/CostReference';
import { MultiSelect } from '@/components/ui/MultiSelect';
import { IconPencil, IconPlus, IconWarning } from '@/components/ui/icons';
import { Badge, Button, Card, Collapsible, ConfirmDialog, CopyButton, Dialog, Field, Input, Menu, SectionedDialog, Select, Skeleton, Toast, Toggle, type DialogSection } from '@/components/ui/primitives';
import { fmtAbsolute, fmtCompact, fmtHourTick, fmtInt, fmtPct, fmtRelative, fmtUsd } from '@/lib/format';
import { useAsync, useNow } from '@/lib/hooks';
import { KeyDialog } from './KeysPage';

export const STRATEGY_LABEL: Record<PoolStrategy, string> = { earliest_refresh: 'Earliest refresh', least_utilized: 'Least utilized', priority_random: 'Priority, then random' };
export const OVERAGE_LABEL: Record<OveragePolicy, string> = { allow: 'Allow overage', never: 'Never use overage', prefer_plan: 'Prefer plan quota' };
const KEY_STATUS_TONE: Record<KeyStatus, 'critical' | 'good' | 'neutral'> = { active: 'good', expired: 'neutral', revoked: 'critical' };
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
  { value: 'prefer_plan', title: 'Prefer plan quota', body: 'move sessions to a sibling with plan quota when this upstream starts billing extra usage' },
  { value: 'allow', title: 'Allow', body: 'ignore extra usage' },
  { value: 'never', title: 'Never', body: 'treat an upstream in extra usage as unavailable until its window resets' },
];
const PROBE_FIELDS: { help: string; key: NumericTuningKey; label: string; min?: number }[] = [
  { key: 'leadSeconds5m', label: 'Lead time, 5m cache (s)', help: 'How long before a 5-minute entry expires its probe is due. The lead has to cover the round trip and the jitter below, or the entry expires before the probe lands.', min: 1 },
  {
    key: 'leadSeconds1h',
    label: 'Lead time, 1h cache (s)',
    help: 'The same for 1-hour entries. The lead is larger because the entry is worth more: missing the refresh costs a full 1-hour write, so the probe is given room for a retry before the entry goes cold.',
    min: 1,
  },
  { key: 'jitterSeconds5m', label: 'Jitter, 5m cache (s)', help: 'Random seconds taken off the 5-minute due time, so sessions that started together do not all probe in the same instant.', min: 0 },
  { key: 'jitterSeconds1h', label: 'Jitter, 1h cache (s)', help: 'The same spread for 1-hour entries, wider because their due times cluster on the hour.', min: 0 },
  {
    key: 'coldProbeLimit',
    label: 'Cold probe limit',
    help: "A cold probe is one whose response shows no cache read for the prefix it was meant to keep warm, because the entry had already expired or the upstream changed. After this many cold probes in a row the recipe pauses until the next real request. A cold probe is the expensive kind (it rewrites the entry instead of reading it), so a streak also drags the session's net negative; the grace probes in this section are what lets the next turn recover. A probe that migrated the session does not count.",
    min: 0,
  },
  { key: 'retrySeconds', label: 'Retry backoff base (s)', help: 'How long the recipe waits after a probe fails on the transport or with a server error. Each further failure doubles the wait.', min: 1 },
  { key: 'retryMaxSeconds', label: 'Retry backoff max (s)', help: 'Ceiling for that doubling, so a long upstream outage settles into a steady retry instead of growing without bound.', min: 1 },
  {
    key: 'maxIdleSeconds',
    label: 'Max idle seconds',
    help: "Stop probing the main conversation agent once this much time has passed since its own last real request. It applies to that agent only: subagents stop when their lifecycle ends, at their Stop hook or the end of the session, never by idle time. 0 keeps probing while the pin lives.",
    min: 0,
  },
  {
    key: 'maxIdleTicks',
    label: 'Max idle ticks',
    help: 'The same limit counted in probes: stop after this many probes with no real request in between. It applies to the main conversation agent only, for the same reason as the idle seconds above. 0 means no limit.',
    min: 0,
  },
];
const NET_NEGATIVE_HELP =
  'The router keeps a running net per session in the ledger: the gross cache savings credited by real requests that landed on a warm entry, minus the cost of every probe. Once that net is below zero the recipe gets the grace probes below and then pauses. A later real request that lands warm adds to the savings again and can resume probing.';
const NET_NEGATIVE_GRACE_HELP =
  'Probes a recipe still gets after its net has gone negative, so one expensive turn does not pause a session that is about to earn the cost back. The budget is granted again on every real request while the net stays negative, because a cold streak drags the net down and the net only rises on a request that lands warm, which needs a probe first.';
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
const KEEPALIVE_SUMMARY_HINT =
  'Cold limit: consecutive probes that read no cache before the recipe pauses. Idle: time without a real request on the main conversation agent before probing stops. Net-negative stop: probes left once a session has spent more on probes than it saved.';
const CAPTURE_HELP = 'Bodies of every key in this pool without an override are stored for this long; admins only; capped by the retention setting.';
const AFFINITY_HINT = "Idle time before a session's pin expires. Off means every request picks an upstream afresh.";
const KEEPALIVE_GATE = "Requires session affinity of at least 5 minutes. A probe follows the session's pin; a shorter pin cannot keep the session on the upstream it warmed.";
const KEEPALIVE_WARNING = 'Session affinity is shorter than one hour. One-hour cache entries outlive the pin, so a session can migrate away from a warmed cache.';

export function PoolsPage() {
  const pools = useAsync(() => api.listPools(), []);
  const accounts = useAsync(() => api.listAccounts(), []);
  const keys = useAsync(() => api.listKeys(), []);
  const catalog = useAsync(() => api.listCatalog(), []);
  const now = useNow();
  const { canMutate } = useAppSettings();
  const [dialog, setDialog] = useState<{ pool: Pool | null } | null>(null);
  const [toast, setToast] = useState<string | null>(null);

  const accountById = useMemo(() => new Map((accounts.data ?? []).map((a) => [a.id, a])), [accounts.data]);
  const keysByPool = useMemo(() => {
    const m = new Map<string, ApiKey[]>();
    for (const k of keys.data ?? []) m.set(k.poolId, [...(m.get(k.poolId) ?? []), k]);
    return m;
  }, [keys.data]);
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
        <p className="page-head__meta">A pool groups upstreams behind a set of API keys. Requests pick a member by priority, stick to it per session, and fail over when a member hits a rate limit.</p>
        {canMutate && (
          <Button variant="primary" icon={<IconPlus size={14} />} onClick={() => setDialog({ pool: null })}>
            Create pool
          </Button>
        )}
      </div>
      <div className="stat-grid stat-grid--5">
        <Tile label="Pools" value={totals ? fmtInt(totals.pools) : null} />
        <Tile label="Upstreams in pools" value={totals ? fmtInt(totals.accounts) : null} />
        <Tile label="API keys" value={totals ? fmtInt(totals.keys) : null} />
        <Tile label="Requests, 24h" value={totals ? fmtInt(totals.requests) : null} />
        <Tile label="Cost, 24h" value={totals ? fmtUsd(totals.cost) : null} />
      </div>
      <div className="pool-grid">
        {pools.data ? pools.data.map((p) => <PoolCard key={p.id} pool={p} keys={keysByPool.get(p.id) ?? []} accountById={accountById} now={now} canMutate={canMutate} onEdit={() => setDialog({ pool: p })} />) : [0, 1].map((i) => <Skeleton key={i} height={360} />)}
      </div>
      <PoolDialog
        open={dialog !== null}
        pool={dialog?.pool ?? null}
        onClose={() => setDialog(null)}
        accounts={accounts.data ?? []}
        keys={keys.data ?? []}
        pools={pools.data ?? []}
        models={(catalog.data ?? []).map((m) => ({ label: m.id, value: m.id }))}
        onSaved={(created) => {
          setDialog(null);
          setToast(created ? 'Pool created' : 'Pool saved');
          pools.reload();
          keys.reload();
        }}
        onKeyCreated={() => keys.reload()}
        onKeysChanged={() => keys.reload()}
      />
      <Toast message={toast} onDone={() => setToast(null)} />
    </div>
  );
}

function Tile({ label, value }: { label: string; value: string | null }) {
  return (
    <div className="stat stat--compact">
      <span className="stat__label">{label}</span>
      <span className="stat__value">{value ?? <Skeleton width={60} height={22} />}</span>
    </div>
  );
}

function PoolCard({ pool, keys, accountById, now, canMutate, onEdit }: { accountById: Map<string, Account>; canMutate: boolean; keys: ApiKey[]; now: number; onEdit: () => void; pool: Pool }) {
  const ink = useChartInk();
  const data = pool.traffic24h.map((b, i, all) => {
    const window = all.slice(Math.max(0, i - 2), i + 1);
    return { at: Date.parse(b.at), requests: b.requests, tokens: b.tokens, average: window.reduce((s, x) => s + x.requests, 0) / window.length };
  });
  const tokens24h = pool.traffic24h.reduce((s, b) => s + b.tokens, 0);
  return (
    <Card padded={false} className="pool">
      <header className="pool__head">
        <div className="pool__title">
          <h2 className="card__title mono">{pool.name}</h2>
          <Badge tone={POOL_STATUS_TONE[pool.status]} dot>
            {pool.status}
          </Badge>
          {canMutate && (
            <Button size="sm" variant="ghost" icon={<IconPencil size={13} />} onClick={onEdit} className="pool__edit" aria-label={`Edit pool ${pool.name}`}>
              Edit
            </Button>
          )}
        </div>
        <dl className="pool__policy">
          <div>
            <dt>Strategy</dt>
            <dd>{STRATEGY_LABEL[pool.strategy]}</dd>
          </div>
          <div>
            <dt>Sessions</dt>
            <dd className="num" title={AFFINITY_HINT}>
              Affinity {fmtAffinity(pool.affinityTtlSeconds)}
            </dd>
          </div>
          <div>
            <dt>Overage</dt>
            <dd>{OVERAGE_LABEL[pool.overagePolicy]}</dd>
          </div>
          <div>
            <dt>Migration</dt>
            <dd className="num">{pool.migrateAboveUtilization === null ? <span className="muted">Migrate off</span> : `Migrate above ${fmtPct(pool.migrateAboveUtilization)} utilization`}</dd>
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
              {pool.cacheTtl?.enabled ? (
                <span className="chips">
                  <Badge tone="neutral">
                    Main <span className="mono">{pool.cacheTtl.main}</span>
                  </Badge>
                  <Badge tone="neutral">
                    Subagents <span className="mono">{pool.cacheTtl.subagent}</span>
                  </Badge>
                  {pool.cacheTtl.inOverage && <Badge tone="warning">{pool.cacheTtl.inOverage} in overage</Badge>}
                  {pool.cacheTtl.models.length > 0 && <Badge tone="neutral">{pool.cacheTtl.models.length} model override{pool.cacheTtl.models.length === 1 ? '' : 's'}</Badge>}
                </span>
              ) : (
                <span className="muted">off</span>
              )}
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
              <YAxis width={32} tickLine={false} axisLine={false} tick={{ fill: ink.ink3, fontSize: 11 }} allowDecimals={false} />
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
                  <Link to={`/upstreams/${m.accountId}`}>{a?.label ?? m.accountId}</Link>
                  <span className="pool__eligibility">{a ? <Eligibility account={a} now={now} /> : null}</span>
                </li>
              );
            })}
        </ul>
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

function Eligibility({ account, now }: { account: Account; now: number }) {
  if (account.status === 'reauth_required') return <Badge tone="critical">reauth required</Badge>;
  if (account.status === 'cooldown' && account.cooldowns[0]) {
    return (
      <Badge tone="warning" title={fmtAbsolute(account.cooldowns[0].until)}>
        cooldown ends {fmtRelative(account.cooldowns[0].until, now)}
      </Badge>
    );
  }
  if (account.extraUsage?.inUse) return <Badge tone="serious">overage</Badge>;
  if (!account.routingEnabled) return <Badge tone="neutral">disabled</Badge>;
  return <Badge tone="good">eligible</Badge>;
}

type ProbeTuning = Omit<PoolKeepalive, 'costCapUsdPerDay' | 'enabled' | 'warmSubagents'>;
type NumericTuningKey = { [K in keyof ProbeTuning]-?: ProbeTuning[K] extends number ? K : never }[keyof ProbeTuning];

interface PoolForm {
  affinity: number;
  allowed: string[];
  cacheTtl: PoolCacheTtl;
  capEnabled: boolean;
  capture: boolean;
  captureHours: number;
  capUsd: number;
  customAffinity: boolean;
  keepalive: boolean;
  members: Record<string, number>;
  migrate: number;
  migrateOn: boolean;
  name: string;
  overage: OveragePolicy;
  status: 'active' | 'disabled';
  strategy: PoolStrategy;
  tuning: ProbeTuning;
  warmSubagents: boolean;
}

const isPreset = (seconds: number) => AFFINITY_PICKS.some((p) => p.seconds === seconds);

const blankForm = (): PoolForm => ({
  affinity: 3_600,
  allowed: [],
  cacheTtl: { ...CACHE_TTL_DEFAULTS },
  capEnabled: false,
  capture: false,
  captureHours: 24,
  capUsd: 5,
  customAffinity: false,
  keepalive: true,
  members: {},
  migrate: 90,
  migrateOn: false,
  name: '',
  overage: 'prefer_plan',
  status: 'active',
  strategy: 'priority_random',
  tuning: { ...KEEPALIVE_TUNING_DEFAULTS },
  warmSubagents: true,
});

const formFrom = (pool: Pool): PoolForm => ({
  affinity: pool.affinityTtlSeconds,
  allowed: pool.defaultAllowedModels ?? [],
  cacheTtl: pool.cacheTtl ? { ...pool.cacheTtl, models: pool.cacheTtl.models.map((m) => ({ ...m })) } : { ...CACHE_TTL_DEFAULTS },
  capEnabled: pool.keepalive.costCapUsdPerDay !== null,
  capture: pool.bodyCaptureHours > 0,
  captureHours: pool.bodyCaptureHours || 24,
  capUsd: pool.keepalive.costCapUsdPerDay ?? 5,
  customAffinity: !isPreset(pool.affinityTtlSeconds),
  keepalive: pool.keepalive.enabled,
  members: Object.fromEntries(pool.members.map((m) => [m.accountId, m.priority])),
  migrate: pool.migrateAboveUtilization === null ? 90 : Math.round(pool.migrateAboveUtilization * 100),
  migrateOn: pool.migrateAboveUtilization !== null,
  name: pool.name,
  overage: pool.overagePolicy,
  // Only the two settable states reach the form; a degraded or exhausted pool is still routing.
  status: pool.status === 'disabled' ? 'disabled' : 'active',
  strategy: pool.strategy,
  tuning: {
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
  warmSubagents: pool.keepalive.warmSubagents ?? true,
});

export function PoolDialog({ open, pool, onClose, onSaved, onKeyCreated, onKeysChanged, accounts, keys, pools, models }: { accounts: Account[]; keys: ApiKey[]; models: { label: string; value: string }[]; onClose: () => void; onKeyCreated: (c: CreatedApiKey) => void; onKeysChanged: () => void; onSaved: (created: boolean) => void; open: boolean; pool: Pool | null; pools: Pool[] }) {
  const [form, setForm] = useState<PoolForm>(blankForm);
  const [busy, setBusy] = useState(false);
  const settings = useAsync(() => api.getSettings(), []);
  const captureMaxHours = settings.data?.retention.bodyCaptureMaxHours ?? 72;
  const [keyOpen, setKeyOpen] = useState(false);
  const [created, setCreated] = useState<CreatedApiKey | null>(null);
  // Keys created from inside the dialog appear in the list before the parent reloads.
  const [localKeys, setLocalKeys] = useState<ApiKey[]>([]);
  const [moved, setMoved] = useState<Set<string>>(new Set());
  const [moveTo, setMoveTo] = useState('');
  const [moving, setMoving] = useState<ApiKey | null>(null);
  const [revoked, setRevoked] = useState<Set<string>>(new Set());
  const [revoking, setRevoking] = useState<ApiKey | null>(null);
  // Held apart from the form so a priority keystroke cannot move the row under the cursor.
  const [memberOrder, setMemberOrder] = useState<string[]>([]);
  // The model picked in the "add a per-model override" row, before it joins form.cacheTtl.models.
  const [addModelId, setAddModelId] = useState('');
  useEffect(() => {
    if (!open) return;
    const next = pool ? formFrom(pool) : blankForm();
    setForm(next);
    setMemberOrder(orderedAccountIds(accounts, next.members));
    setLocalKeys([]);
    setMoved(new Set());
    setRevoked(new Set());
    setAddModelId('');
    setBusy(false);
    // `accounts` is read but not tracked: the parent hands down a fresh array on
    // every render, and reacting to that would wipe the form mid-edit.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, pool]);

  const set = <K extends keyof PoolForm>(key: K, value: PoolForm[K]) => setForm((f) => ({ ...f, [key]: value }));
  const poolKeys = (pool ? [...localKeys, ...keys.filter((k) => k.poolId === pool.id && !localKeys.some((l) => l.id === k.id))] : localKeys).filter((k) => !moved.has(k.id));
  const otherPools = pools.filter((p) => p.id !== pool?.id);
  // Keys created before the pool is saved use the id the pool will get from its name.
  const draftPoolId = form.name.trim().toLowerCase().replace(/[^a-z0-9]+/g, '-') || 'new';
  const draftName = form.name.trim() || 'new pool';
  // Memoized so the key dialog's reset effect does not fire on every keystroke in this form.
  const draftCaptureHours = form.capture ? form.captureHours : 0;
  const keyDialogPools = useMemo(
    () => (pool || pools.some((p) => p.id === draftPoolId) ? pools : [...pools, { bodyCaptureHours: draftCaptureHours, id: draftPoolId, name: draftName }]),
    [pool, pools, draftCaptureHours, draftPoolId, draftName],
  );
  // Below five minutes a pin expires before the probe it scheduled can help.
  const keepaliveBlocked = form.affinity < 300;
  const shortAffinity = form.affinity >= 300 && form.affinity < 3_600;
  const valid = form.name.trim().length > 0 && Object.keys(form.members).length > 0 && form.affinity >= 0 && (!form.migrateOn || (form.migrate >= 50 && form.migrate <= 100));

  const orderIndex = new Map(memberOrder.map((id, i) => [id, i]));
  const orderedAccounts = [...accounts].sort((a, b) => (orderIndex.get(a.id) ?? accounts.length) - (orderIndex.get(b.id) ?? accounts.length) || a.label.localeCompare(b.label));

  const setTuning = <K extends NumericTuningKey>(key: K, value: number) => setForm((f) => ({ ...f, tuning: { ...f.tuning, [key]: value } }));
  const setNetNegative = (value: boolean) => setForm((f) => ({ ...f, tuning: { ...f.tuning, stopWhenNetNegative: value } }));
  const setCacheTtl = <K extends keyof PoolCacheTtl>(key: K, value: PoolCacheTtl[K]) => setForm((f) => ({ ...f, cacheTtl: { ...f.cacheTtl, [key]: value } }));
  const setModelOverride = (model: string, key: 'main' | 'subagent', value: CacheTtl) =>
    setForm((f) => ({ ...f, cacheTtl: { ...f.cacheTtl, models: f.cacheTtl.models.map((m) => (m.model === model ? { ...m, [key]: value } : m)) } }));
  const removeModelOverride = (model: string) => setForm((f) => ({ ...f, cacheTtl: { ...f.cacheTtl, models: f.cacheTtl.models.filter((m) => m.model !== model) } }));
  const addModelOverride = () => {
    if (!addModelId) return;
    setForm((f) => ({ ...f, cacheTtl: { ...f.cacheTtl, models: [...f.cacheTtl.models, { model: addModelId, main: f.cacheTtl.main, subagent: f.cacheTtl.subagent }] } }));
    setAddModelId('');
  };

  const startMove = (k: ApiKey) => {
    setMoveTo(otherPools[0]?.id ?? '');
    setMoving(k);
  };

  const moveKey = async () => {
    if (!moving || !moveTo) return;
    await api.updateKey(moving.id, { allowedModels: moving.allowedModels, bodyCaptureHours: moving.bodyCaptureHours, keepaliveOptOut: moving.keepaliveOptOut, name: moving.name, notes: moving.notes, poolId: moveTo });
    setMoved((prev) => new Set(prev).add(moving.id));
    setMoving(null);
    onKeysChanged();
  };

  const submit = async () => {
    setBusy(true);
    const input: PoolInput = {
      affinityTtlSeconds: form.affinity,
      bodyCaptureHours: form.capture ? form.captureHours : 0,
      cacheTtl: form.cacheTtl,
      defaultAllowedModels: form.allowed.length ? form.allowed : null,
      keepalive: { ...form.tuning, costCapUsdPerDay: form.keepalive && form.capEnabled ? form.capUsd : null, enabled: form.keepalive && !keepaliveBlocked, warmSubagents: form.warmSubagents },
      members: Object.entries(form.members).map(([accountId, priority]) => ({ accountId, priority })),
      migrateAboveUtilization: form.migrateOn ? form.migrate / 100 : null,
      name: form.name.trim(),
      overagePolicy: form.overage,
      status: form.status,
      strategy: form.strategy,
    };
    if (pool) await api.updatePool(pool.id, input);
    else await api.createPool(input);
    setBusy(false);
    onSaved(pool === null);
  };

  const sections: DialogSection[] = [
    {
      id: 'general',
      label: 'General',
      body: (
        <>
          <Field label="Name">
            <Input value={form.name} onChange={(e) => set('name', e.target.value)} placeholder="research" className="mono" />
          </Field>
          <Field label="Status" hint="A disabled pool keeps its keys and members but stops accepting requests.">
            <Select value={form.status} onChange={(e) => set('status', e.target.value as 'active' | 'disabled')}>
              <option value="active">Active</option>
              <option value="disabled">Disabled</option>
            </Select>
          </Field>
        </>
      ),
    },
    {
      id: 'routing',
      label: 'Routing',
      body: (
        <>
          <Field label="Strategy">
            <Select value={form.strategy} onChange={(e) => set('strategy', e.target.value as PoolStrategy)}>
              {(Object.keys(STRATEGY_LABEL) as PoolStrategy[]).map((v) => (
                <option key={v} value={v}>
                  {STRATEGY_LABEL[v]}
                </option>
              ))}
            </Select>
          </Field>
          <Field label="Session affinity" hint={AFFINITY_HINT}>
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
            {form.customAffinity && <Input type="number" min={0} step={30} value={form.affinity} onChange={(e) => set('affinity', Math.max(0, Number(e.target.value)))} style={{ width: 120 }} aria-label="Session affinity in seconds" />}
          </Field>
          <div className="capture">
            <label className="row">
              <input type="checkbox" checked={form.migrateOn} onChange={(e) => set('migrateOn', e.target.checked)} />
              <span className="field__label">Migrate above utilization</span>
              {!form.migrateOn && <span className="muted">Off</span>}
            </label>
            {form.migrateOn && (
              <Field label="Utilization (%)" hint="A pinned session moves to another member once its upstream crosses this line on any window.">
                <Input type="number" min={50} max={100} value={form.migrate} onChange={(e) => set('migrate', Number(e.target.value))} style={{ width: 120 }} />
              </Field>
            )}
          </div>
          <Field label="Default allowed models" hint="Empty means every model the members serve.">
            <MultiSelect label="Models" options={models} value={form.allowed} onChange={(v) => set('allowed', v)} searchable mono />
          </Field>
        </>
      ),
    },
    {
      id: 'overage',
      label: 'Overage',
      body: (
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
      ),
    },
    {
      id: 'keepalive',
      label: 'Caching',
      body: (
        <>
          <h3 className="section-title">Cache TTL</h3>
          <div className="capture">
            <div className="field--inline field">
              <span className="field__text">
                <span className="field__label">Set the cache TTL for this pool</span>
                <p className="field__hint">
                  Every breakpoint the client sends is rewritten to the TTL chosen here. The client's own TTL setting then has no effect through this pool. Applies to every upstream in this pool.
                  A request that carries no breakpoints is left alone.
                </p>
              </span>
              <Toggle checked={form.cacheTtl.enabled} onChange={(v) => setCacheTtl('enabled', v)} label="Set the cache TTL for this pool" />
            </div>
            {form.cacheTtl.enabled && (
              <>
                <div className="form-row">
                  <Field label="Main conversation">
                    <Select value={form.cacheTtl.main} onChange={(e) => setCacheTtl('main', e.target.value as CacheTtl)}>
                      <option value="5m">5 minutes</option>
                      <option value="1h">1 hour</option>
                    </Select>
                  </Field>
                  <Field label="Subagents">
                    <Select value={form.cacheTtl.subagent} onChange={(e) => setCacheTtl('subagent', e.target.value as CacheTtl)}>
                      <option value="5m">5 minutes</option>
                      <option value="1h">1 hour</option>
                    </Select>
                  </Field>
                </div>
                <div className="row" style={{ justifyContent: 'space-between' }}>
                  <span className="field__label">Write cost</span>
                  <CostReferenceLink />
                </div>
                <p className="field__hint">A 1-hour entry costs 2 times the input price on every write, a 5-minute entry 1.25 times; reads cost the same either way.</p>
                <Field label="In overage" hint="Applies while the upstream serving the request is in overage, where writes cost money rather than plan quota.">
                  <Select value={form.cacheTtl.inOverage ?? ''} onChange={(e) => setCacheTtl('inOverage', (e.target.value || null) as CacheTtl | null)}>
                    <option value="">Keep the rules above</option>
                    <option value="5m">5 minutes</option>
                    <option value="1h">1 hour</option>
                  </Select>
                </Field>
                <div className="field">
                  <span className="field__label">Per-model overrides</span>
                  <p className="field__hint">A model absent here takes the pool rule above.</p>
                  {form.cacheTtl.models.length > 0 && (
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
                          {form.cacheTtl.models.map((m) => (
                            <tr key={m.model}>
                              <td className="mono">{m.model}</td>
                              <td>
                                <Select value={m.main} onChange={(e) => setModelOverride(m.model, 'main', e.target.value as CacheTtl)} aria-label={`Main conversation TTL for ${m.model}`}>
                                  <option value="5m">5 minutes</option>
                                  <option value="1h">1 hour</option>
                                </Select>
                              </td>
                              <td>
                                <Select value={m.subagent} onChange={(e) => setModelOverride(m.model, 'subagent', e.target.value as CacheTtl)} aria-label={`Subagent TTL for ${m.model}`}>
                                  <option value="5m">5 minutes</option>
                                  <option value="1h">1 hour</option>
                                </Select>
                              </td>
                              <td>
                                <Button size="sm" variant="ghost" onClick={() => removeModelOverride(m.model)}>
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
                    <Select value={addModelId} onChange={(e) => setAddModelId(e.target.value)} aria-label="Model to add a cache TTL override for" style={{ maxWidth: 260 }}>
                      <option value="">Choose a model…</option>
                      {models
                        .filter((m) => !form.cacheTtl.models.some((x) => x.model === m.value))
                        .map((m) => (
                          <option key={m.value} value={m.value}>
                            {m.label}
                          </option>
                        ))}
                    </Select>
                    <Button size="sm" icon={<IconPlus size={13} />} disabled={!addModelId} onClick={addModelOverride}>
                      Add
                    </Button>
                  </div>
                </div>
              </>
            )}
          </div>
          <h3 className="section-title" style={{ marginTop: 12 }}>
            Keep prompt caches warm
          </h3>
          <div className="capture">
            <div className="field--inline field">
              <span className="field__text">
                <span className="field__label">Keep prompt caches warm</span>
                <p className="field__hint">Idle sessions get a small probe before their cache expires, so the next turn reads instead of writes.</p>
              </span>
              <Toggle checked={form.keepalive && !keepaliveBlocked} onChange={(v) => set('keepalive', v)} label="Keep prompt caches warm" disabled={keepaliveBlocked} />
            </div>
            {keepaliveBlocked && <p className="field__hint">{KEEPALIVE_GATE}</p>}
            {!keepaliveBlocked && form.keepalive && shortAffinity && (
              <div className="callout callout--warning" role="status">
                <IconWarning size={16} />
                <div>{KEEPALIVE_WARNING}</div>
              </div>
            )}
          </div>
          {!keepaliveBlocked && form.keepalive && (
            <>
              <div className="capture">
                <div className="field--inline field">
                  <span className="field__text">
                    <span className="field__label">Also warm subagent caches</span>
                    <p className="field__hint">Off probes the main conversation only; subagent recipes end with the subagent either way.</p>
                  </span>
                  <Toggle checked={form.warmSubagents} onChange={(v) => set('warmSubagents', v)} label="Also warm subagent caches" />
                </div>
              </div>
              <div className="capture">
                <div className="row" style={{ justifyContent: 'space-between' }}>
                  <label className="row">
                    <input type="checkbox" checked={form.capEnabled} onChange={(e) => set('capEnabled', e.target.checked)} />
                    <span className="field__label">Daily probe cost cap ($)</span>
                  </label>
                  <CostReferenceLink />
                </div>
                {form.capEnabled && <Input type="number" min={0} step={0.5} value={form.capUsd} onChange={(e) => set('capUsd', Number(e.target.value))} style={{ width: 120 }} aria-label="Daily probe cost cap in dollars" />}
                <p className="field__hint">Estimated at API list prices for every upstream. For subscription upstreams this approximates quota consumption rather than money.</p>
              </div>
              <div className="capture">
                <div className="field--inline field">
                  <span className="field__text">
                    <span className="field__label">Stop when probes cost more than they save</span>
                    <p className="field__hint">{NET_NEGATIVE_HELP}</p>
                  </span>
                  <Toggle checked={form.tuning.stopWhenNetNegative} onChange={setNetNegative} label="Stop when probes cost more than they save" />
                </div>
                {form.tuning.stopWhenNetNegative && (
                  <Field label="Grace probes after the net turns negative" hint={NET_NEGATIVE_GRACE_HELP}>
                    <Input
                      type="number"
                      min={0}
                      value={form.tuning.netNegativeGraceProbes}
                      onChange={(e) => setTuning('netNegativeGraceProbes', Math.max(0, Number(e.target.value)))}
                      style={{ width: 120 }}
                      aria-label="Grace probes after the net turns negative"
                    />
                  </Field>
                )}
              </div>
              <Collapsible title="Probe tuning" hint={`${PROBE_FIELDS.length} settings`}>
                <div className="form-row">
                  {PROBE_FIELDS.map((f) => (
                    <Field key={f.key} label={f.label} hint={f.help}>
                      <Input type="number" min={f.min} value={form.tuning[f.key]} onChange={(e) => setTuning(f.key, Number(e.target.value))} />
                    </Field>
                  ))}
                </div>
              </Collapsible>
            </>
          )}
        </>
      ),
    },
    {
      id: 'capture',
      label: 'Capture',
      body: (
        <div className="capture">
          <div className="field--inline field">
            <span className="field__text">
              <span className="field__label">Capture request and response bodies</span>
              <p className="field__hint">{CAPTURE_HELP}</p>
            </span>
            <Toggle checked={form.capture} onChange={(v) => set('capture', v)} label="Capture request and response bodies" />
          </div>
          {form.capture && (
            <Field label="Retention (hours)">
              <Input type="number" min={1} max={captureMaxHours} value={form.captureHours} onChange={(e) => set('captureHours', Number(e.target.value))} style={{ width: 120 }} aria-label="Capture retention in hours" />
            </Field>
          )}
        </div>
      ),
    },
    {
      id: 'members',
      label: 'Members',
      body: (
        <div className="field">
          <span className="field__label">Members</span>
          <p className="field__hint">Lower priority numbers are tried first, so the list reads in the order the router walks it.</p>
          <ul className="member-picker">
            {orderedAccounts.map((a) => {
              const on = a.id in form.members;
              return (
                <li key={a.id}>
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
                  </label>
                  {on && (
                    <span className="member-picker__prio">
                      <span className="muted">priority</span>
                      <Input
                        type="number"
                        min={0}
                        max={9}
                        value={form.members[a.id]}
                        onChange={(e) => set('members', { ...form.members, [a.id]: Number(e.target.value) })}
                        onBlur={() => setMemberOrder(orderedAccountIds(accounts, form.members))}
                        onKeyDown={(e) => {
                          if (e.key === 'Enter') setMemberOrder(orderedAccountIds(accounts, form.members));
                        }}
                        style={{ width: 56 }}
                        aria-label={`Priority for ${a.label}`}
                      />
                    </span>
                  )}
                </li>
              );
            })}
          </ul>
        </div>
      ),
    },
    {
      id: 'keys',
      label: 'Keys',
      body: (
        <div className="field">
          <div className="row" style={{ justifyContent: 'space-between' }}>
            <span className="field__label">Keys</span>
            <Button size="sm" icon={<IconPlus size={13} />} onClick={() => setKeyOpen(true)} disabled={!pool && !form.name.trim()}>
              Create key
            </Button>
          </div>
          {poolKeys.length === 0 ? (
            <p className="muted">{pool ? 'No keys belong to this pool yet.' : 'Keys can be created once the pool has a name.'}</p>
          ) : (
            <ul className="key-list">
              {poolKeys.map((k) => {
                const status = revoked.has(k.id) ? 'revoked' : k.status;
                return (
                  <li key={k.id}>
                    <span>{k.name}</span>
                    <span className="mono muted">{k.prefix}…</span>
                    <Badge tone={KEY_STATUS_TONE[status]} dot>
                      {status}
                    </Badge>
                    <Menu
                      label={`Actions for key ${k.name}`}
                      items={[
                        { label: 'Move to…', disabled: otherPools.length === 0, onClick: () => startMove(k) },
                        { label: 'Revoke', danger: true, disabled: status !== 'active', onClick: () => setRevoking(k) },
                      ]}
                    />
                  </li>
                );
              })}
            </ul>
          )}
          <p className="field__hint">A key belongs to one pool. Move it to another pool to take it off this one.</p>
        </div>
      ),
    },
  ];

  return (
    <>
      <SectionedDialog
        open={open}
        onClose={onClose}
        title={pool ? `Edit pool ${pool.name}` : 'Create pool'}
        subtitle="Members join at a priority; lower numbers are tried first. Keys bound to the pool can only reach its members."
        sections={sections}
        footer={
          <>
            <Button variant="ghost" onClick={onClose}>
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
        models={models.map((m) => m.value)}
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
            <Button variant="primary" disabled={!moveTo} onClick={moveKey}>
              Move key
            </Button>
          </>
        }
      >
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
          if (revoking) setRevoked((prev) => new Set(prev).add(revoking.id));
        }}
      />
      <Dialog
        open={created !== null}
        onClose={() => setCreated(null)}
        title="API key created"
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
