import { Fragment, memo, useEffect, useId, useMemo, useRef, useState, type ReactNode } from 'react';
import { Link } from 'react-router-dom';
import { Bar, ComposedChart, Line, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts';
import { api, ApiError, CACHE_TTL_DEFAULTS, KEEPALIVE_DEFAULTS } from '@/api';
import { POOL_AFFINITY_DEFAULTS, POOL_ENGINE_DEFAULTS, type CreatePoolInput, type OverviewRange, type PoolEngineFields } from '@/api/client';
import { oneOf } from '@/api/decode';
import { isStaleEdit, slug } from '@/api/http';
import type {
  Account, AffinityMode, ApiKey, AttentionItem, CacheTtl, CreatedApiKey, KeepaliveOverview, MemberEligibility, ModelRule, OveragePolicy, Pool, PoolCacheTtl, PoolMember, PoolStrategy, PoolWarning, Range, ReturnToPlan,
  SessionPlacement, SubagentPlacement, TtlRules, WarmSessions,
} from '@/api/types';
import { useSession } from '@/app/session';
import { useAppSettings } from '@/app/settings';
import { useAddressEdit } from '@/app/useAddressEdit';
import { useHashLanding } from '@/app/useHashLanding';
import { AttentionRows } from '@/components/AttentionRows';
import { useChartInk } from '@/components/charts/useChartInk';
import { Legend, timeTip } from '@/components/charts/Tooltip';
import { FilterBar } from '@/components/FilterBar';
import { GuardrailList, KEY_STATUS_TONE, KeyDialog, SecretDialog } from '@/components/KeyDialog';
import { PatternEditor, useCatalogSuggestions } from '@/components/PatternEditor';
import { SortControl } from '@/components/SortControl';
import { AttentionLine } from '@/components/ui/AttentionLine';
import { CostReferenceLink } from '@/components/ui/CostReference';
import { SpeedChip } from '@/components/ui/SpeedChip';
import { sumTileOf, Tile } from '@/components/ui/Tile';
import { UtilizationBar } from '@/components/UtilizationBar';
import { IconPencil, IconPlus, IconWarning } from '@/components/ui/icons';
import { PopoverHost, useHoldHost } from '@/components/ui/Popover';
import { Badge, BadgeLink, Button, Card, ConfirmDialog, Dialog, ErrorState, Expert, Field, GroupedDialog, GroupLink, Input, Menu, Meter, PageLink, Refusal, Select, Skeleton, StaleRefusal, SwitchedNumber, Toggle, type DialogGroup } from '@/components/ui/primitives';
import { accountHref, EDIT_PARAMS, editHref, POOL_SORT_KEYS, poolKeysHref, POOLS_CODEC, sortDirOf, type PoolsAddress } from '@/lib/address';
import { FAILOVER_MOVE_REASONS, fundedRankOf, RULE_MOVE_REASONS, moveLabel, nativeSidesOf, sidesLine, sumFunded, type WeekReading } from '@/lib/analytics';
import { groupInto } from '@/lib/counts';
import { fmtCompact, fmtDuration, fmtHours, fmtInt, fmtPct, fmtPeriodEnd, fmtShortTime, fmtUsd, messageOf, noun, plural, shortModel } from '@/lib/format';
import { EMPTY_FIELD, ENGINE_BOUNDS, ENGINE_FIELD_LABELS, KEEPALIVE_BOUNDS, KEEPALIVE_FIELD_LABELS, POOL_GROUPS, guardrailIdsOf, guardrailsChange, keyFormOf, keyInputOf, memberField, poolRefusals, refusalsView, sameValues, saveStatus, typedNumber, typedOf, type PoolGroup, type TypedField } from '@/lib/forms';
import { failedFirst, refreshAttention, useAsync, useAttention, useLatest, useNow, useQueryFilters, useWeekReading, type Loaded } from '@/lib/hooks';
import { POOL_RAIL } from '@/lib/poolRail';
import { FAILOVER_PRESETS, SHEDDING_PRESETS, failoverPresetOf, sheddingPresetOf, shedPreview, shedPreviewLine, slowDownPreview, slowDownPreviewLine, type FailoverPreset, type SheddingPreset } from '@/lib/presets';
import { minuteSpan, windowOf } from '@/lib/quota';
import { aroundSubject, logHref, words } from '@/lib/routingLog';
import { orderRows, type SortFigure } from '@/lib/sort';
import { GROUP_LABEL, INELIGIBLE_LABEL, LAST_RESORT_LABEL, RULE_NOTE, STRATEGY_LABEL, cooldownBadge, kindOrdered, memberSpeed, overageBadge, poolStatusSentence, selectionOrder, spentBadge, type Placement, type StandingBadge, windingDownBadge } from '@/lib/selection';

const POOL_STATUS_TONE = { active: 'good', degraded: 'warning', disabled: 'neutral', exhausted: 'critical' } as const;
// What a first pool needs, said on the create dialog's rail. The words stay while the refusal dots come and go.
const CREATE_MARKS: Partial<Record<PoolGroup, string>> = { guardrails: 'after saving', identity: 'needed', keys: 'after saving', members: 'needed' };
// Every field of these groups belongs to the expert view, so with the mode off a group has nothing to show and leaves
// the rail.
const EXPERT_ONLY: ReadonlySet<PoolGroup> = new Set(['autoMode', 'cacheDiagnostics', 'failover', 'placement', 'slowDown']);
const percentOf = (share: number) => Math.round(share * 10_000) / 100;
/** The route's bounds on the two shares the dialog reads in percent: the window ceiling and the shed cap. */
const CEILING_PERCENT = { max: 100, min: 50 } as const;
const CAP_PERCENT = { max: 100, min: 10 } as const;
/**
 * The strip: the features a pool has on, and the shedding's state, in the rail's order, each opening the dialog on the
 * group that edits it.
 */
const FEATURES: { group: PoolGroup; on: (pool: Pool) => boolean; word: (pool: Pool) => string }[] = [
  // Every attached guardrail, a disabled one too, since its attachment stands either way.
  { group: 'guardrails', on: (p) => p.guardrails.length > 0, word: (p) => plural(p.guardrails.length, 'guardrail') },
  { group: 'autoMode', on: (p) => !p.autoModeServer, word: () => 'server-side auto mode off' },
  // Drawn on every pool, so a pool whose switch is off says so on its card.
  { group: 'shedding', on: () => true, word: (p) => (p.loadShedding ? `sheds above ${percentOf(p.windowCeiling)} %` : 'shedding off') },
  { group: 'slowDown', on: (p) => p.slowDown, word: (p) => `slows above ${minuteSpan(p.slowDownStretchSeconds * 1_000)}` },
  { group: 'cacheTtl', on: (p) => p.cacheTtl.enabled, word: () => 'cache TTL' },
  { group: 'keepalive', on: (p) => p.keepalive.enabled, word: () => 'keepalive' },
];
/** The dialog group that edits each pool field a warning names. */
const WARNING_FIELD_GROUP: Readonly<Record<string, PoolGroup>> = {
  affinity_ttl_seconds: 'pinning',
  burn_average_seconds: 'utilization',
  cache_ttl: 'cacheTtl',
  in_flight_cap: 'failover',
  load_shedding: 'shedding',
  migrate_when_spent: 'failover',
  return_to_plan_dwell_seconds: 'overage',
  shed_cap: 'shedding',
  shed_dwell_seconds: 'shedding',
  slow_down_hold_seconds: 'slowDown',
  slow_down_stretch_seconds: 'slowDown',
  warm_sessions: 'shedding',
  window_ceiling: 'utilization',
};
/** Where a warning is fixed: the group of its first field, or none for a field this build does not know. */
const warningGroup = (warning: PoolWarning): PoolGroup | null => WARNING_FIELD_GROUP[warning.fields[0] ?? ''] ?? null;
type IdleReturnBucket = KeepaliveOverview['pools'][number]['idleReturns'][number];
interface ReturnRateProps {
  buckets: IdleReturnBucket[];
  error: string | null;
  loading: boolean;
  onRange: (range: OverviewRange) => void;
  range: OverviewRange;
}
const BREAK_EVEN_HINT = "The pause length at which the recipe's probes cost one re-write of its prefix, for this pool's funding, cache TTL and models.";
const SHARE_HINT = "The idle window's probes against one re-write of the prefix, so the bet this window makes is visible per pool.";
/** A range's two ends through `format`, one figure when both ends read alike. */
const rangeText = (range: Range, format: (value: number) => string) => {
  const [min, max] = [format(range.min), format(range.max)];
  return min === max ? min : `${min} to ${max}`;
};
/** The same through `fmtDuration`, each figure kept whole on its line. */
function RangeFigures({ range }: { range: Range }) {
  const [min, max] = [fmtDuration(range.min * 1_000), fmtDuration(range.max * 1_000)];
  return min === max ? <span className="nowrap">{min}</span> : <><span className="nowrap">{min}</span> to <span className="nowrap">{max}</span></>;
}
/**
 * A line about the break-even under the hint that explains it: beside a ceiling left off, the figure it resolves to in
 * the pattern of the key dialog's inherited values, beside a parked or horizon ceiling switched on past the break-even,
 * the figures the router caps it at, and under the idle window set past the break-even, the note that the ceiling lies
 * above it, as the router runs it.
 */
function BreakEvenNote({ lead, range }: { lead: string; range: Range }) {
  return (
    <span className="keepalive-resolved" title={BREAK_EVEN_HINT}>
      {lead} <RangeFigures range={range} />
    </span>
  );
}
/** A ceiling past the break-even's low end, as the router caps it: the figures it runs to, or null below the cap. */
const cappedAt = (seconds: number, breakEven: Range): Range | null =>
  seconds > breakEven.min ? { max: Math.min(seconds, breakEven.max), min: Math.min(seconds, breakEven.min) } : null;
/** The card's line: the three ceilings the router resolves for the pool, in whole hours. */
const ceilingsLine = (r: NonNullable<Pool['keepaliveResolved']>) => {
  const hours = (seconds: number) => fmtHours(seconds * 1_000);
  return `idle ${rangeText(r.idleSeconds, hours)} · parked ${rangeText(r.parkedSeconds, hours)} · horizon ${rangeText(r.horizonSeconds, hours)}`;
};
/** A bucket's idle ages: `0 to 5m`, then `5m to 15m` and on to the open-ended `over 32h`. */
const ageRange = (b: IdleReturnBucket) => {
  const from = b.fromSeconds === 0 ? '0' : fmtHours(b.fromSeconds * 1_000);
  return b.toSeconds === null ? `over ${from}` : `${from} to ${fmtHours(b.toSeconds * 1_000)}`;
};
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
/** Members first, lowest priority number first, then alphabetically. Non-members trail alphabetically. */
const orderedAccountIds = (accounts: Account[], members: Record<string, number>): string[] => orderRows(accounts, (a) => members[a.id] ?? null, 'asc', (a) => a.label).map((a) => a.id);
const AFFINITY_HINT = 'How long a session stays on its account after its last request. Off keeps no pins, and a custom value is at least 60 seconds.';
// The order the pills read in: the default, the two directions away from it, then a hand-set mix.
const PRESET_PILLS = ['balanced', 'fast', 'conservative', 'custom'] as const;
const PRESET_LABEL: Record<SheddingPreset, string> = { balanced: 'Balanced', conservative: 'Conservative', custom: 'Custom', fast: 'Fast' };
const SHEDDING_HINT: Record<SheddingPreset, string> = {
  balanced: 'The default.',
  conservative: 'Averages over an hour and waits longer between rounds: fewer cache rewrites, slower relief.',
  custom: 'Set the cap, the burn average under Windows, and the dwell by hand.',
  fast: 'Averages over fifteen minutes and sheds again sooner: faster relief, more cache rewrites.',
};
const FAILOVER_HINT: Record<FailoverPreset, string> = {
  balanced: 'The default.',
  conservative: 'Waits out a longer throttle before moving on.',
  custom: 'Set the cap and the retry bound by hand.',
  fast: 'Moves on sooner and spreads placements wider.',
};
// A scale from the most returns to none.
const RETURN_CHOICES: { body: string; title: string; value: ReturnToPlan }[] = [
  { value: 'always', title: 'Always', body: 'at once when cold, after the dwell when warm' },
  { value: 'when_cold', title: 'When cold', body: 'only when the move is free: a compaction, a restart or a lane change' },
  { value: 'never', title: 'Never', body: 'the session stays until the member refuses it' },
];
// A scale from the fewest warm sessions moved to the most.
const WARM_CHOICES: { body: string; title: string; value: WarmSessions }[] = [
  { value: 'never', title: 'Never', body: 'a round marks cold sessions only, whatever the excess still asks' },
  { value: 'when_needed', title: 'When needed', body: 'cold sessions first, then warm ones by burn over the prompt they would rewrite' },
  { value: 'always', title: 'Always', body: 'every session by burn over the prompt it would rewrite, warm or cold alike' },
];

/** A radio group over a choice table, each choice's title leading its body and the picked one marked active. */
function ChoiceList<T extends string>({ choices, invalid, label, name, onChange, value }: { choices: readonly { body: string; title: string; value: T }[]; invalid?: true; label: string; name: string; onChange: (value: T) => void; value: T }) {
  return (
    <div className="radio-list" role="radiogroup" aria-label={label} aria-invalid={invalid}>
      {choices.map((c) => (
        <label key={c.value} className={`radio-list__item ${value === c.value ? 'is-active' : ''}`}>
          <input type="radio" name={name} checked={value === c.value} onChange={() => onChange(c.value)} />
          <span>
            <b>{c.title}:</b> {c.body}
          </span>
        </label>
      ))}
    </div>
  );
}

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
/** The Strategy hint follows the chosen strategy. */
const STRATEGY_HINT: Record<PoolStrategy, ReactNode> = {
  pace: (
    <>
      Pace scores each member by its spare rate: the allowance left in the window it reads, the model&apos;s own 7-day Fable window where the model has one, else the 7-day window, per hour until that window resets or the subscription ends, less what its sessions burn of that window an hour, within the first <GroupLink id="members">priority tier</GroupLink> that has an eligible account. An API-key member scores the room left in its worst bucket. An unsampled account counts as untouched, and ties are random. Each account&apos;s windows are on the <PageLink to="/accounts">Accounts page</PageLink>.
    </>
  ),
  priority: (
    <>
      Within the first <GroupLink id="members">priority tier</GroupLink> that has an eligible account, any of them at random.
    </>
  ),
  round_robin: (
    <>
      Within the first <GroupLink id="members">priority tier</GroupLink> that has an eligible account, each in turn. A restart starts the round again.
    </>
  ),
};

export function PoolsPage() {
  const { unit, week } = useAppSettings();
  // The cost model whose capacities scale the day's figures to the chosen week. The Pro week reads without it.
  const { reading, settings } = useWeekReading(week);
  // The dialog's one-shot `edit` and `group` wait in the address until the lists load, so a pick written before then
  // carries them through.
  const { address, filters, update } = useQueryFilters(POOLS_CODEC, { keep: EDIT_PARAMS });
  const dir = sortDirOf(POOL_SORT_KEYS, filters);
  const [dialog, setDialog] = useState<{ initialGroup: PoolGroup | null; pool: Pool | null } | null>(null);
  const [deleting, setDeleting] = useState<Pool | null>(null);
  const [intervalMs, setIntervalMs] = useState(5_000);
  // The pool dialog draws these lists, so auto reload waits while it is open rather than change them under an edit, and
  // while the Delete pool confirm is open, as every read waits under a dialog.
  const { cause: pauseCause, host } = useHoldHost();
  const reread = { paused: pauseCause !== null, refreshMs: intervalMs };
  const pools = useAsync(() => api.listPools(), [], reread);
  const accounts = useAsync(() => api.listAccounts(), [], reread);
  const keys = useAsync(() => api.listKeys(), [], reread);
  // Feeds the cache TTL per-model override picker's options alone, so it is read once per visit rather than on every
  // auto reload, and Refresh reads it again.
  const catalog = useAsync(() => api.listCatalog(), []);
  const attention = useAttention();
  const now = useNow();
  const pageRef = useRef<HTMLDivElement>(null);
  const { notify } = useSession();

  // The dialog reads both lists, so the address's edit waits for the accounts too.
  useAddressEdit(accounts.data ? pools.data : null, 'Pool', (pool, group) => setDialog({ initialGroup: oneOf(POOL_GROUPS, group) ?? 'identity', pool }));

  // `/pools#<pool id>` lands on the card once the pools have drawn it.
  useHashLanding(pageRef, '.pool', () => pools.data !== null, [pools.data]);

  const accountById = useMemo(() => new Map((accounts.data ?? []).map((a) => [a.id, a])), [accounts.data]);
  // The cards alone take the order: a card's member rows keep the selection order.
  const ordered = useMemo(() => {
    if (pools.data === null) return null;
    const figures: Record<PoolsAddress['sort'], (p: Pool) => SortFigure> = {
      cost: (p) => fundedRankOf(unit, p.day24h),
      members: (p) => p.members.length,
      name: (p) => p.name,
      pins: (p) => p.activePins,
      requests: (p) => p.requests24h,
      status: (p) => p.status,
    };
    return orderRows(pools.data, figures[filters.sort], dir, (p) => p.name);
  }, [dir, filters.sort, pools.data, unit]);
  const keysByPool = useMemo(() => groupInto(keys.data ?? [], (k) => k.poolId), [keys.data]);

  const removePool = async (p: Pool) => {
    try {
      await api.deletePool(p.id);
      notify('Pool deleted');
    } catch (e) {
      notify(e instanceof ApiError && e.status === 409 ? 'Move or revoke its keys first' : messageOf(e));
    }
    pools.reload();
    keys.reload();
  };
  // `useAsync` keeps the last list through a reload and after a failed one, so the read's state decides rather than the
  // list: during a refresh the old list lacks the key just created, and a failed refresh leaves it standing.
  const keysRead = keys.loading ? 'loading' : keys.error ? 'failed' : 'loaded';
  const totals = useMemo(
    () =>
      pools.data
        ? {
            pools: pools.data.length,
            accounts: new Set(pools.data.flatMap((p) => p.members.map((m) => m.accountId))).size,
            // Null until the key list is read, so the tile waits as the cards' Keys tiles do rather than reading 0.
            keys: keysRead === 'loaded' && keys.data ? keys.data.length : null,
            requests: pools.data.reduce((s, p) => s + p.requests24h, 0),
            day: sumFunded(pools.data.map((p) => p.day24h)),
          }
        : null,
    [keys.data, keysRead, pools.data],
  );
  const attentionOf = useMemo(() => new Map((pools.data ?? []).map((p) => [p.id, (attention.items ?? []).filter((i) => i.pools.includes(p.id))])), [attention.items, pools.data]);
  const dayTile = totals ? sumTileOf(unit, totals.day, reading) : null;

  return (
    <PopoverHost.Provider value={host}>
      <div className="stack" ref={pageRef}>
        <FilterBar
          address={address}
          count={pools.data === null ? '' : plural(pools.data.length, 'pool')}
          filters={filters}
          refresh={{
            intervalMs,
            onIntervalChange: setIntervalMs,
            onRefresh: () => {
              pools.reload();
              accounts.reload();
              keys.reload();
              catalog.reload();
              settings.reload();
            },
            paused: pauseCause,
            refreshTitle: 'Reads the list again now',
          }}
          update={update}
        >
          <SortControl keys={POOL_SORT_KEYS} onChange={update} value={filters} />
          <Button variant="primary" icon={<IconPlus size={14} />} onClick={() => setDialog({ initialGroup: null, pool: null })}>
            Create pool
          </Button>
        </FilterBar>
        <p className="page-head__meta">A pool groups accounts behind a set of keys. Requests pick a member by priority, stick to it per session, and fail over when a member hits a rate limit.</p>
        <div className="stat-grid stat-grid--fit">
          <Tile label="Pools" value={totals ? fmtInt(totals.pools) : null} />
          <Tile label="Accounts in pools" value={totals ? fmtInt(totals.accounts) : null} />
          <Tile label="Keys" value={totals && totals.keys !== null ? fmtInt(totals.keys) : keysRead === 'failed' ? <span className="muted">n/a</span> : null} />
          <Tile label="Requests, 24h" value={totals ? fmtInt(totals.requests) : null} />
          <Tile label="Cost, 24h" value={dayTile?.value ?? null} title={dayTile?.title} failure={dayTile?.failure ?? null} />
        </div>
        {/* A failed read leaves the cards on the last answer's items, or none before the first. This line says so. */}
        {attention.error !== null && <AttentionLine items={attention.items} error={attention.error} loadedAt={attention.loadedAt} now={now} refresh={refreshAttention} />}
        <div className="pool-grid">
          {ordered
            ? ordered.map((p) => (
                <PoolCard
                  key={p.id}
                  address={address}
                  pool={p}
                  keys={keysByPool.get(p.id) ?? []}
                  keysRead={keysRead}
                  accounts={accounts.data}
                  accountById={accountById}
                  attention={attentionOf.get(p.id) ?? []}
                  now={now}
                  reading={reading}
                  onEdit={() => setDialog({ initialGroup: null, pool: p })}
                  onDelete={() => setDeleting(p)}
                />
              ))
            : pools.error
              ? <ErrorState title="Could not load the pools" body={pools.error} retry={pools.reload} />
              : [0, 1].map((i) => <Skeleton key={i} height={360} />)}
        </div>
        <PoolDialog
          open={dialog !== null}
          captureMaxHours={settings.data?.retention.bodyCaptureMaxHours}
          pool={dialog?.pool ?? null}
          initialGroup={dialog?.initialGroup ?? null}
          onClose={(reload) => {
            setDialog(null);
            if (reload) {
              pools.reload();
              keys.reload();
            }
          }}
          accounts={accounts}
          keys={keys.data ?? []}
          pools={pools.data ?? []}
          models={(catalog.data ?? []).map((m) => ({ label: m.id, value: m.id }))}
          onSaved={(created) => {
            setDialog(null);
            notify(created ? 'Pool created' : 'Pool saved');
            pools.reload();
            keys.reload();
          }}
          // The pool route lists its keys, so a key created, moved or revoked from the dialog changes the card's
          // warning too.
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
    </PopoverHost.Provider>
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
        <table className="table ttl-grid table--plain">
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

/** One rule set's per-model overrides. The picker's state is its own, so each set adds overrides independently. */
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
          <table className="table rules-table table--plain">
            <thead>
              <tr>
                <th className="col-fill">Model</th>
                <th className="col-fit">Main conversation</th>
                <th className="col-fit">Subagents</th>
                <th className="col-fit" />
              </tr>
            </thead>
            <tbody>
              {rules.models.map((m) => (
                <tr key={m.model}>
                  <td className="mono col-fill" title={m.model}>{shortModel(m.model)}</td>
                  <td className="col-fit">
                    <Select value={m.main} disabled={disabled} onChange={(e) => setModelOverride(m.model, 'main', e.target.value as CacheTtl)} aria-label={`${label} main conversation TTL for ${m.model}`}>
                      <option value="5m">5 minutes</option>
                      <option value="1h">1 hour</option>
                    </Select>
                  </td>
                  <td className="col-fit">
                    <Select value={m.subagent} disabled={disabled} onChange={(e) => setModelOverride(m.model, 'subagent', e.target.value as CacheTtl)} aria-label={`${label} subagent TTL for ${m.model}`}>
                      <option value="5m">5 minutes</option>
                      <option value="1h">1 hour</option>
                    </Select>
                  </td>
                  <td className="col-fit">
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

/**
 * A pool card's Cost 24h under the unit. Native stacks the day's figure in the chosen week over its billed dollars, a
 * side with no rows left out, and a day a plan row priced nothing on reads the dash under `UNPRICED_TITLE`. A pool sums
 * its members' accounts, so its figure carries no hint. The figure waits on the scale, so the card draws its skeleton
 * while the cost model is out, and the reading's error as a dash titled with it.
 */
function PoolCost({ pool, reading }: { pool: Pool; reading: WeekReading }) {
  const { unit } = useAppSettings();
  const dash = (title?: string) => (
    <dd className="num" title={title}>
      -
    </dd>
  );
  const day = pool.day24h;
  const sides = nativeSidesOf(day);
  if (unit === 'native' && sides.equivalent === undefined && sides.billed === undefined) return dash('No request in the last 24 hours');
  const tile = sumTileOf(unit, day, reading);
  if (tile.value === null)
    return tile.failure ? (
      dash(tile.failure.text)
    ) : (
      <dd className="num">
        <Skeleton width={72} height={12} />
      </dd>
    );
  const { scale, week } = reading;
  return (
    <dd className="num" title={tile.title ?? (unit === 'native' && scale !== null ? sidesLine(sides, week, scale) : undefined)}>
      {tile.value}
    </dd>
  );
}

function PoolCard({
  address,
  pool,
  keys,
  keysRead,
  accounts,
  accountById,
  attention,
  now,
  onEdit,
  onDelete,
  reading,
}: {
  accountById: Map<string, Account>;
  /** Null until the account list is read. The status sentence then says only what the pool's verdicts carry. */
  accounts: Account[] | null;
  /** The view's query string, which the dialog links carry so the dialog's close returns to the same order. */
  address: string;
  attention: AttentionItem[];
  keys: ApiKey[];
  /** Until the key list has loaded, `keys` says nothing about the pool's keys, so the count line and the tile wait. */
  keysRead: 'failed' | 'loaded' | 'loading';
  now: number;
  onDelete: () => void;
  onEdit: () => void;
  pool: Pool;
  reading: WeekReading;
}) {
  const standingId = useId();
  const warnedAt = pool.warnings.length === 0 ? null : warningGroup(pool.warnings[0]);
  const tiers = selectionOrder(pool, accounts ?? [], now);
  const grouped = kindOrdered(pool);
  const sentence = poolStatusSentence(pool, accounts, now);
  const features = FEATURES.filter((f) => f.on(pool));
  const caption = `${STRATEGY_LABEL[pool.strategy]}${grouped ? `${pool.strategy === 'priority' ? ', with' : ','} subscription accounts first` : ''}`;
  const memberOf = (id: string) => pool.members.find((m) => m.accountId === id);
  /** The pins the route counts past the members' sum: pins on an account that left the pool. */
  const formerPins = Math.max(0, pool.activePins - pool.members.reduce((n, m) => n + m.activePins, 0));
  // The selection order skips a member the accounts list does not carry, so the table lists it after the tiers.
  const unlisted = pool.members.filter((m) => !accountById.has(m.accountId));
  // The route counts the moves of the last hour by reason on the pool's member accounts, so every figure is a whole
  // number. The figure sums the rule moves, a keepalive probe's move included, which the log lists as moved only with
  // probe decisions chosen. The title names the failovers apart, which the log draws as a later attempt's placement,
  // and any reason this build does not know apart from both.
  const moveCounts = Object.entries(pool.movesPerHour)
    .filter(([, count]) => count > 0)
    .sort(([, a], [, b]) => b - a);
  const among = (reasons: readonly string[]) => moveCounts.filter(([reason]) => reasons.includes(reason));
  const ruleMoves = among(RULE_MOVE_REASONS);
  const failovers = among(FAILOVER_MOVE_REASONS);
  const known: readonly string[] = [...RULE_MOVE_REASONS, ...FAILOVER_MOVE_REASONS];
  const others = moveCounts.filter(([reason]) => !known.includes(reason));
  const movesTotal = ruleMoves.reduce((s, [, count]) => s + count, 0);
  const moveLine = ([reason, count]: [string, number]) => `${moveLabel(reason)} ${fmtInt(count)}`;
  const movesTitle = [
    ruleMoves.length === 0 ? 'No rule moved a session in the last hour' : 'Rule moves in the last hour',
    ...ruleMoves.map(moveLine),
    ...(failovers.length === 0 ? [] : ['Failovers left out of the count', ...failovers.map(moveLine)]),
    ...(others.length === 0 ? [] : ['Other moves left out of the count', ...others.map(moveLine)]),
    `Opens the Routing log on pool ${pool.id} and the moved kind for the last hour, which lists a keepalive probe's move only with probe decisions chosen`,
  ].join('\n');
  return (
    <Card id={pool.id} tabIndex={-1} padded={false} className="pool">
      <header className="pool__head">
        <div className="pool__title">
          <h2 className="card__title mono">{pool.name}</h2>
          <Badge tone={POOL_STATUS_TONE[pool.status]} dot>
            {pool.status}
          </Badge>
          {keysRead === 'loaded' ? (
            <Link to={poolKeysHref(pool.id)} className={`pool__keys ${keys.length === 0 ? 'pool__keys--none' : ''}`}>
              {keys.length === 0 ? 'No keys yet' : plural(keys.length, 'key')}
            </Link>
          ) : (
            keysRead === 'failed' && <span className="pool__keys pool__keys--unread">Keys unavailable</span>
          )}
          <span className="pool__actions">
            <Button size="sm" variant="ghost" icon={<IconPencil size={13} />} onClick={onEdit} aria-label={`Edit pool ${pool.name}`}>
              Edit
            </Button>
            <Button size="sm" variant="ghost" onClick={onDelete} aria-label={`Delete pool ${pool.name}`}>
              Delete
            </Button>
          </span>
        </div>
        {features.length > 0 && (
          <div className="pool__features chips">
            {features.map((f) => (
              <Fragment key={f.group}>
                <BadgeLink to={editHref('/pools', pool.id, f.group, address)} title={`Opens the pool dialog on ${POOL_RAIL[f.group].label}`}>
                  {f.word(pool)}
                </BadgeLink>
                {f.group === 'keepalive' && pool.keepaliveResolved && <span className="muted pool__ceilings">{ceilingsLine(pool.keepaliveResolved)}</span>}
              </Fragment>
            ))}
          </div>
        )}
        <p className="pool__status">
          <b>{sentence.word}:</b>{' '}
          {sentence.parts.map((part, i) =>
            'text' in part ? (
              <Fragment key={i}>{part.text}</Fragment>
            ) : (
              <Link key={i} to={accountHref(part.accountId)}>
                {part.label}
              </Link>
            ),
          )}
        </p>
        <AttentionRows items={attention} now={now} />
      </header>

      <div className="pool__serving">
        <table className="table serving">
          <caption title={RULE_NOTE}>{caption}</caption>
          <thead>
            <tr>
              <th className="col-fit">Tier</th>
              <th className="col-fill">Account</th>
              <th className="col-fit">Standing</th>
              <th className="right col-fit">Pins</th>
            </tr>
          </thead>
          <tbody>
            {tiers.flatMap((tier) =>
              tier.placements.map((p) => {
                const m = memberOf(p.account.id);
                const isNext = tier.next.includes(p.account.id);
                const why = p.ineligible
                  ? `${p.account.label}: ${INELIGIBLE_LABEL[p.ineligible]}, skipped by selection`
                  : isNext
                    ? `${p.account.label}: where the next fresh session lands`
                    : p.lastResort
                      ? `${p.account.label}: ${LAST_RESORT_LABEL[p.lastResort]}, taken only once no other account can take a fresh session`
                      : p.deferred
                        ? `${p.account.label}: in overage, taken after the plan-funded members of this tier`
                        : `${p.account.label}: rank ${p.rank} in this tier`;
                const whyId = `${standingId}-${p.account.id}`;
                // A pool whose switch is off holds nobody, so an entry its account still carries draws no chip here.
                const speed = memberSpeed({ speed: m?.speed ?? null }, pool.slowDown ? p.account : null, now);
                return (
                  <tr key={p.account.id} className={isNext ? 'is-next' : undefined}>
                    <td className="pool__prio num col-fit">{grouped ? `${GROUP_LABEL[tier.group]} P${tier.priority}` : `P${tier.priority}`}</td>
                    <td className="col-fill" title={p.account.label}>
                      <Link to={accountHref(p.account.id)}>{p.account.label}</Link>
                      {m && <MemberBudget member={m} />}
                    </td>
                    <td className="serving__standing col-fit" aria-describedby={whyId}>
                      <span className="chips">
                        {isNext ? <Badge tone="accent">next</Badge> : m && <Eligibility verdict={m.eligible} account={p.account} now={now} periodEnd={m.budgetUntil} />}
                        {!isNext && p.lastResort && <LastResort placement={p} now={now} />}
                        {!isNext && p.deferred && <Badge tone="neutral">after plan quota</Badge>}
                        {/* The route's budget verdict says this itself. The badge stands for a member the route names
                            an earlier obstacle for. */}
                        {!isNext && m && m.budgetUntil !== null && m.eligible !== 'budget' && <AtBudget periodEnd={m.budgetUntil} />}
                        <SpeedChip factor={speed.factor} hover={speed.hover} />
                      </span>
                      <span id={whyId} className="sr-only">
                        {why}
                      </span>
                    </td>
                    <td className="right num col-fit">{p.pins === 0 ? <span className="muted">0</span> : p.pins}</td>
                  </tr>
                );
              }),
            )}
            {unlisted.map((m) => {
              const whyId = `${standingId}-${m.accountId}`;
              const speed = memberSpeed(m, null, now);
              return (
                <tr key={m.accountId}>
                  <td className="pool__prio num col-fit">P{m.priority}</td>
                  <td className="mono col-fill" title={m.accountId}>{m.accountId}</td>
                  <td className="serving__standing col-fit" aria-describedby={whyId}>
                    <span className="chips">
                      <Eligibility verdict={m.eligible} account={undefined} now={now} periodEnd={m.budgetUntil} />
                      <SpeedChip factor={speed.factor} hover={speed.hover} />
                    </span>
                    <span id={whyId} className="sr-only">
                      {`${m.accountId}: not a known account`}
                    </span>
                  </td>
                  <td className="right num col-fit">{m.activePins === 0 ? <span className="muted">0</span> : m.activePins}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>

      <PoolTraffic requests24h={pool.requests24h} traffic={pool.traffic24h} />

      <dl className="pool__stats">
        <div>
          <dt>Members</dt>
          <dd className="num">{pool.members.length}</dd>
        </div>
        <div>
          <dt>Keys</dt>
          <dd className="num">{keysRead === 'loaded' ? keys.length : keysRead === 'failed' && <span className="muted">n/a</span>}</dd>
        </div>
        <div>
          <dt>Requests 24h</dt>
          <dd className="num">{fmtInt(pool.requests24h)}</dd>
        </div>
        <div>
          <dt>Cost 24h</dt>
          <PoolCost pool={pool} reading={reading} />
        </div>
        <div>
          <dt>Active pins</dt>
          <dd className="num">
            {pool.activePins}
            {formerPins > 0 && <span className="sub">{formerPins} on {noun(formerPins, 'a former member', 'former members')}</span>}
          </dd>
        </div>
        <div>
          <dt>Warnings</dt>
          <dd className="num" title={pool.warnings.length === 0 ? undefined : pool.warnings.map((w) => w.text).join(' ')}>
            {warnedAt === null ? fmtInt(pool.warnings.length) : <Link to={editHref('/pools', pool.id, warnedAt, address)}>{fmtInt(pool.warnings.length)}</Link>}
          </dd>
        </div>
        <div>
          <dt>Moves in the last hour</dt>
          <dd className="num" title={movesTitle}>
            <Link to={logHref({ pools: [pool.id], kinds: ['moved'], ...aroundSubject(null, now) })}>{fmtInt(movesTotal)}</Link>
          </dd>
        </div>
      </dl>
    </Card>
  );
}

/**
 * A pool card's 24-hour traffic. Memoized, since the Pools page's polls and clock re-render every card while the traffic
 * changes with the pools read alone, and recharts would redraw the bars and the line from it each time.
 */
const PoolTraffic = memo(function PoolTraffic({ requests24h, traffic }: { requests24h: number; traffic: Pool['traffic24h'] }) {
  const ink = useChartInk();
  const data = traffic.map((b, i, all) => {
    const window = all.slice(Math.max(0, i - 2), i + 1);
    return { at: Date.parse(b.at), requests: b.requests, tokens: b.tokens, average: window.reduce((s, x) => s + x.requests, 0) / window.length };
  });
  const tokens24h = traffic.reduce((s, b) => s + b.tokens, 0);
  return (
    <div className="pool__chart">
      <div className="pool__chart-head">
        <span className="secondary">Traffic, 24h</span>
        <span className="num">
          <b>{fmtCompact(tokens24h)}</b> tokens, <b>{fmtInt(requests24h)}</b> requests
        </span>
      </div>
      {requests24h === 0 ? (
        <div className="empty" style={{ padding: 30 }}>
          No traffic in the last 24h
        </div>
      ) : (
        <ResponsiveContainer width="100%" height={170}>
          <ComposedChart data={data} margin={{ top: 6, right: 4, bottom: 0, left: 4 }} barCategoryGap={2}>
            <XAxis dataKey="at" tickFormatter={(v: number) => fmtShortTime(v)} tickLine={false} axisLine={{ stroke: ink.axis }} interval={5} minTickGap={24} />
            <YAxis width={32} tickLine={false} axisLine={false} tick={{ fill: ink.ink2, fontSize: 11 }} allowDecimals={false} />
            <Tooltip
              cursor={{ fill: ink.grid, opacity: 0.6 }}
              content={timeTip<(typeof data)[number]>((p) => [
                { color: ink.series[0], label: 'requests', value: fmtInt(p.requests) },
                { color: ink.ink2, label: '3-hour average', value: p.average.toFixed(1) },
                { label: 'tokens', value: fmtCompact(p.tokens) },
              ])}
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
  );
});

/** The pool's idle-age counters: of the idle windows that reached each age, how many the next request continued. */
function ReturnRate({ buckets, error, loading, onRange, range }: ReturnRateProps) {
  return (
    <div className="field return-rate">
      <label className="row">
        <span className="field__label">Return rate by idle age</span>
        <Select value={range} onChange={(e) => onRange(e.target.value as OverviewRange)} aria-label="Return rate range">
          <option value="7d">7 days</option>
          <option value="30d">30 days</option>
        </Select>
      </label>
      <p className="field__hint">Of the idle windows that reached each age in the last {range === '7d' ? 7 : 30} days, the ones whose next request continued the same prefix within it.</p>
      {error !== null && <p className="muted">Could not load the return rate: {error}</p>}
      {loading ? (
        <Skeleton height={28} />
      ) : buckets.every((b) => b.reached === 0) ? (
        // A failed read with no buckets leaves its failure line alone, since it says nothing of the idle windows.
        (buckets.length > 0 || error === null) && <p className="muted">No idle windows recorded yet</p>
      ) : (
        <ul className="plain-list return-rate__rows">
          {buckets.map((b) => {
            const rate = b.reached > 0 ? b.returned / b.reached : 0;
            return (
              <li key={b.fromSeconds} className="return-rate__row">
                <span className="return-rate__age">{ageRange(b)}</span>
                <span className="num">
                  {fmtInt(b.returned)} of {fmtInt(b.reached)}
                </span>
                <Meter thin tone="accent" value={rate} label={`${ageRange(b)}: ${fmtPct(rate)} returned`} />
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}

/**
 * A member's spend against each budget it carries, an API-key member's whole spend and a subscription member's
 * metered spend (overage and fast answers), one line each under its name. A member without a budget draws none.
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
  if (budgets.length === 0) return null;
  return (
    <>
      {budgets.map(({ budget, period }) => {
        const spent = member.spends.find((s) => s.period === period)?.spentUsd;
        // The fill column cuts the line and the cell's title names the account alone, so the line carries its own.
        const text = `${metered}${spent === undefined ? '-' : fmtUsd(spent)} of ${fmtUsd(budget)} ${period === 'day' ? 'today' : 'this month'}`;
        return (
          <span key={period} className="sub num" title={text}>
            {text}
          </span>
        );
      })}
    </>
  );
}

function AtBudget({ periodEnd }: { periodEnd: string | null }) {
  return <Badge tone="warning">{periodEnd ? `at budget until ${fmtPeriodEnd(periodEnd)}` : 'at budget'}</Badge>;
}

/** The route's verdict, as the serving table's Standing cell reads it. The account only adds when and why. */
function Eligibility({ verdict, account, now, periodEnd }: { account: Account | undefined; now: number; periodEnd: string | null; verdict: MemberEligibility }) {
  switch (verdict) {
    case 'eligible':
      return <Badge tone="good">eligible</Badge>;
    case 'budget':
      return <AtBudget periodEnd={periodEnd} />;
    case 'budget_unknown':
      return <Badge tone="warning" title="The ledger is degraded, so the router has no reading of this member's budget and places nothing on it">{INELIGIBLE_LABEL[verdict]}</Badge>;
    case 'overage':
      return <WordedBadge tone="serious" badge={overageBadge(account?.overage?.reset ?? null, now)} />;
    case 'spent':
      return <WordedBadge tone="serious" badge={spentBadge(account?.spend?.resetsAt ?? null, now)} />;
    // The badge keeps its one word, where the table's label reads "not available".
    case 'unavailable':
      return <Badge tone="critical" title={account ? words(account.status) : 'not a known account'}>unavailable</Badge>;
    case 'unknown':
      return <Badge tone="neutral" title="The router has not loaded this member, or does not answer">{INELIGIBLE_LABEL[verdict]}</Badge>;
    case 'subscription_ended':
      return <Badge tone="neutral">{INELIGIBLE_LABEL[verdict]}</Badge>;
    case 'cooldown':
      return <WordedBadge tone="warning" badge={cooldownBadge(account?.cooldowns.find((c) => c.scope === 'account')?.until, now)} />;
    default: {
      // A verdict the type gains fails to compile here until it draws a badge of its own.
      const unhandled: never = verdict;
      return unhandled;
    }
  }
}

/** Why an eligible member waits for the rest: winding down, with the time left to its end. */
function LastResort({ placement, now }: { now: number; placement: Placement }) {
  if (placement.lastResort === 'winding_down' && placement.end !== null) return <WordedBadge tone="warning" badge={windingDownBadge(placement.end, now)} />;
  return placement.lastResort && <Badge tone="warning">{LAST_RESORT_LABEL[placement.lastResort]}</Badge>;
}

/** The saved pool's warnings above the dialog's groups, each with a link to the group of its first field. */
function PoolWarnings({ warnings }: { warnings: PoolWarning[] }) {
  return (
    <div className="pool-warnings">
      {warnings.map((warning) => {
        const group = warningGroup(warning);
        return (
          <div key={warning.code} className="callout callout--warning" role="status">
            <IconWarning size={16} />
            <div>
              {warning.text}
              {group !== null && (
                <>
                  {' '}
                  Go to <GroupLink id={group}>{POOL_RAIL[group].label}</GroupLink>.
                </>
              )}
            </div>
          </div>
        );
      })}
    </div>
  );
}

/**
 * The stale save's callout. The dialog brings its body back to the top when a notice first appears, but the saved
 * pool's warnings may hold the notice already, so the callout does so itself as it arrives.
 */
function StaleNotice({ children }: { children: ReactNode }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    ref.current?.closest('.dialog__body')?.scrollTo?.({ top: 0 });
  }, []);
  return <div ref={ref}>{children}</div>;
}

function WordedBadge({ badge, tone }: { badge: StandingBadge; tone: 'serious' | 'warning' }) {
  return (
    <Badge tone={tone} title={badge.title}>
      {badge.text}
    </Badge>
  );
}

interface PoolForm {
  affinity: number;
  affinityMode: AffinityMode;
  autoModeServer: boolean;
  /** Each member's daily and monthly budget fields, empty for none, and kept for an unticked member too. */
  budgets: Record<string, { daily: TypedField; monthly: TypedField }>;
  cacheDiagnostics: boolean;
  cacheTtl: PoolCacheTtl;
  capture: boolean;
  /** The retention field as typed, so an emptied field stays empty rather than reading as 0. */
  captureHours: string;
  customAffinity: boolean;
  engine: PoolEngineFields;
  /** The Custom pill is held apart from the values, so its inputs stay open while an edit passes through a preset. */
  failoverCustom: boolean;
  /** The pool's guardrail ids in the order they run, which the save sends apart from the input. */
  guardrails: string[];
  /** Off sends the scheduled horizon as null, the recipe's break-even, and keeps the number for the switch coming back. */
  horizonOn: boolean;
  horizonSeconds: number;
  keepalive: boolean;
  maxIdleSeconds: number;
  /** Off sends the parked ceiling as null, the recipe's break-even, and keeps the number for the switch coming back. */
  maxParkedOn: boolean;
  maxParkedSeconds: number;
  members: Record<string, number>;
  modelRules: ModelRule[];
  name: string;
  overage: OveragePolicy;
  preferSubscription: boolean;
  prefixTokens: TypedField;
  sessionPlacement: SessionPlacement;
  sheddingCustom: boolean;
  status: 'active' | 'disabled';
  strategy: PoolStrategy;
  subagentPlacement: SubagentPlacement;
}

const NO_BUDGET: PoolForm['budgets'][string] = { daily: EMPTY_FIELD, monthly: EMPTY_FIELD };

const isPreset = (seconds: number) => AFFINITY_PICKS.some((p) => p.seconds === seconds);

const blankForm = (): PoolForm => ({
  affinity: 3_600,
  affinityMode: POOL_AFFINITY_DEFAULTS.affinityMode,
  autoModeServer: true,
  budgets: {},
  cacheDiagnostics: false,
  cacheTtl: { ...CACHE_TTL_DEFAULTS },
  capture: false,
  captureHours: '24',
  customAffinity: false,
  engine: { ...POOL_ENGINE_DEFAULTS },
  failoverCustom: false,
  guardrails: [],
  horizonOn: KEEPALIVE_DEFAULTS.scheduledHorizonSeconds !== null,
  horizonSeconds: KEEPALIVE_DEFAULTS.scheduledHorizonSeconds ?? 3_600,
  keepalive: false,
  maxIdleSeconds: KEEPALIVE_DEFAULTS.maxIdleSeconds,
  maxParkedOn: KEEPALIVE_DEFAULTS.maxParkedSeconds !== null,
  maxParkedSeconds: KEEPALIVE_DEFAULTS.maxParkedSeconds ?? 3_600,
  members: {},
  modelRules: [],
  name: '',
  overage: 'prefer_plan',
  preferSubscription: true,
  prefixTokens: { bad: false, text: String(POOL_AFFINITY_DEFAULTS.minSharedPrefixTokens) },
  sessionPlacement: POOL_AFFINITY_DEFAULTS.sessionPlacement,
  sheddingCustom: false,
  status: 'active',
  strategy: 'pace',
  subagentPlacement: POOL_AFFINITY_DEFAULTS.subagentPlacement,
});

const formFrom = (pool: Pool): PoolForm => ({
  affinity: pool.affinityTtlSeconds,
  affinityMode: pool.affinityMode,
  autoModeServer: pool.autoModeServer,
  budgets: Object.fromEntries(pool.members.map((m) => [m.accountId, { daily: { bad: false, text: m.budgetDailyUsd === null ? '' : String(m.budgetDailyUsd) }, monthly: { bad: false, text: m.budgetMonthlyUsd === null ? '' : String(m.budgetMonthlyUsd) } }])),
  cacheDiagnostics: pool.cacheDiagnostics,
  cacheTtl: {
    ...pool.cacheTtl,
    metered: { ...pool.cacheTtl.metered, models: pool.cacheTtl.metered.models.map((m) => ({ ...m })) },
    plan: { ...pool.cacheTtl.plan, models: pool.cacheTtl.plan.models.map((m) => ({ ...m })) },
  },
  capture: pool.bodyCaptureHours > 0,
  captureHours: String(pool.bodyCaptureHours || 24),
  customAffinity: !isPreset(pool.affinityTtlSeconds),
  engine: {
    burnAverageSeconds: pool.burnAverageSeconds,
    inFlightCap: pool.inFlightCap,
    loadShedding: pool.loadShedding,
    migrateWhenSpent: pool.migrateWhenSpent,
    overageMoveGraceMinutes: pool.overageMoveGraceMinutes,
    retryInPlaceSeconds: pool.retryInPlaceSeconds,
    returnToPlan: pool.returnToPlan,
    returnToPlanDwellSeconds: pool.returnToPlanDwellSeconds,
    shedCap: pool.shedCap,
    shedDwellSeconds: pool.shedDwellSeconds,
    slowDown: pool.slowDown,
    slowDownHoldSeconds: pool.slowDownHoldSeconds,
    slowDownStretchSeconds: pool.slowDownStretchSeconds,
    warmSessions: pool.warmSessions,
    windDownMinutes: pool.windDownMinutes,
    windowCeiling: pool.windowCeiling,
  },
  failoverCustom: failoverPresetOf(pool) === 'custom',
  guardrails: guardrailIdsOf(pool.guardrails),
  horizonOn: pool.keepalive.scheduledHorizonSeconds !== null,
  horizonSeconds: pool.keepalive.scheduledHorizonSeconds ?? 3_600,
  keepalive: pool.keepalive.enabled,
  maxIdleSeconds: pool.keepalive.maxIdleSeconds,
  maxParkedOn: pool.keepalive.maxParkedSeconds !== null,
  maxParkedSeconds: pool.keepalive.maxParkedSeconds ?? 3_600,
  members: Object.fromEntries(pool.members.map((m) => [m.accountId, m.priority])),
  modelRules: pool.modelRules,
  name: pool.name,
  overage: pool.overagePolicy,
  // The stored answer, never the effective one, so reopening a pool shows what the operator chose.
  preferSubscription: pool.preferSubscription,
  prefixTokens: { bad: false, text: String(pool.minSharedPrefixTokens) },
  sessionPlacement: pool.sessionPlacement,
  sheddingCustom: sheddingPresetOf(pool) === 'custom',
  // Only the two settable states reach the form. A degraded or exhausted pool is still routing.
  status: pool.status === 'disabled' ? 'disabled' : 'active',
  strategy: pool.strategy,
  subagentPlacement: pool.subagentPlacement,
});

export function PoolDialog({
  captureMaxHours,
  open,
  pool,
  initialGroup = null,
  onClose,
  onSaved,
  onKeysChanged,
  accounts: accountsRead,
  keys,
  pools,
  models,
}: {
  accounts: Pick<Loaded<Account[]>, 'data' | 'error' | 'reload'>;
  /** The bootstrap's body capture limit from the host page's settings read, undefined while that read is out. */
  captureMaxHours?: number;
  /** The group the address named. The reset opens on it, Identity otherwise. */
  initialGroup?: PoolGroup | null;
  keys: ApiKey[];
  models: { label: string; value: string }[];
  /** `reload` is true from the stale save's Reload, and on a close after any refused save or a key move refused as stale, whose cause may be lists that are out of date. */
  onClose: (reload: boolean) => void;
  onKeysChanged: () => void;
  onSaved: (created: boolean) => void;
  open: boolean;
  pool: Pool | null;
  pools: Pool[];
}) {
  const accounts = accountsRead.data ?? [];
  // The Members group names a read that never answered in place of the table.
  const accountsFailed = failedFirst(accountsRead) ? accountsRead.error : null;
  // Until the read answers or fails, no stored member is refused or marked unlisted.
  const accountsLoaded = accountsRead.data !== null || accountsRead.error !== null;
  const onRetryAccounts = accountsRead.reload;
  const now = useNow(undefined, open);
  const [form, setForm] = useState<PoolForm>(blankForm);
  // The values the dialog opened with, which the discard question compares the form against.
  const [opened, setOpened] = useState(form);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [attempted, setAttempted] = useState(false);
  const [stale, setStale] = useState<string | null>(null);
  const [chosen, setChosen] = useState<PoolGroup>('identity');
  // Once per opening of a saved pool, since a create takes no attachment and offers nothing to attach.
  const guardrailRegistry = useAsync(() => (open && pool ? api.listGuardrails() : Promise.resolve(null)), [open, pool?.id]);
  // The return rate's range, which each opening starts on 7 days. The close puts it back, in the render that closes,
  // so an opening's first overview read is the 7-day one.
  const [returnRange, setReturnRange] = useState<OverviewRange>('7d');
  if (!open && returnRange !== '7d') setReturnRange('7d');
  // Each opening of a saved pool reads the sessions and the keepalive overview once, the overview again on a range
  // pick, and both again after a key write, never at the interval.
  const sessionsRead = useAsync(() => (open && pool ? api.listSessions() : Promise.resolve(null)), [open, pool?.id]);
  // Each answer carries the range it was read for.
  const overview = useAsync(
    () => (open && pool ? api.getKeepaliveOverview({ range: returnRange }).then((read) => ({ overview: read, range: returnRange })) : Promise.resolve(null)),
    [open, pool?.id, returnRange],
  );
  const answer = overview.data?.range === returnRange ? overview.data.overview : null;
  // The pool's idle-age counters, empty for a new pool or one the overview does not list.
  const idleReturns: ReturnRateProps = {
    // The last answer outlives a pending or failed read, which after a pick is the other range's, so the dialog draws
    // an answer read for the range it names, a settled read's failure above it or alone, and its loading otherwise.
    buckets: (pool && answer?.pools.find((p) => p.poolId === pool.id)?.idleReturns) || [],
    error: overview.loading ? null : overview.error,
    loading: answer === null && overview.loading,
    onRange: setReturnRange,
    range: returnRange,
  };
  // The sessions under the pool's keys: a shedding round weighs the pool's own pins on a member alone. A session
  // reaches a pool through its key, which is the only link the sessions route carries.
  const sessions = useMemo(() => {
    const poolKeyIds = new Set(keys.filter((k) => k.poolId === pool?.id).map((k) => k.id));
    return (sessionsRead.data ?? []).filter((s) => poolKeyIds.has(s.keyId));
  }, [keys, pool?.id, sessionsRead.data]);
  // The flag `GroupedDialog` hands its expert wrappers, whose registry the bodies built here cannot reach: a half cell
  // whose row-mate is expert spans the row while the mate is hidden.
  const { expertView } = useAppSettings();
  const [keyOpen, setKeyOpen] = useState(false);
  const [created, setCreated] = useState<CreatedApiKey | null>(null);
  // Keys created from inside the dialog appear in the list before the parent reloads.
  const [localKeys, setLocalKeys] = useState<ApiKey[]>([]);
  const [moved, setMoved] = useState<Set<string>>(new Set());
  const [moveBusy, setMoveBusy] = useState(false);
  const [moveError, setMoveError] = useState<string | null>(null);
  const [moveTo, setMoveTo] = useState('');
  const [openedMoveTo, setOpenedMoveTo] = useState('');
  const [moving, setMoving] = useState<ApiKey | null>(null);
  const [revoked, setRevoked] = useState<Set<string>>(new Set());
  const [revoking, setRevoking] = useState<ApiKey | null>(null);
  // Held apart from the form so a priority keystroke cannot move the row under the cursor.
  const [memberOrder, setMemberOrder] = useState<string[]>([]);
  // A refused return dwell under When cold or Never stays editable while it holds the focus, so the keystroke that
  // clears the refusal does not disable the field under the cursor.
  const [dwellFocused, setDwellFocused] = useState(false);
  // The reset reads the accounts through a ref: the list is a fresh array on every render while the read is out, and
  // reacting to that would wipe the form mid-edit.
  const accountsNow = useLatest(accounts);
  useEffect(() => {
    if (!open) return;
    const next = pool ? formFrom(pool) : blankForm();
    // eslint-disable-next-line react-hooks/set-state-in-effect -- the reset runs after the dialog's open effect, which turns the mode on for an expert-only group before the reset names it
    setForm(next);
    setOpened(next);
    setMemberOrder(orderedAccountIds(accountsNow.current, next.members));
    setLocalKeys([]);
    setMoveError(null);
    setMoved(new Set());
    setRevoked(new Set());
    setBusy(false);
    setError(null);
    setAttempted(false);
    setStale(null);
    setChosen(initialGroup ?? 'identity');
  }, [open, pool, initialGroup, accountsNow]);
  // The server's refusal describes the form it was sent. Every control writes through `setForm`, so the next edit
  // takes it away, while a failed save changes no field and leaves it standing.
  const [refusedFor, setRefusedFor] = useState(form);
  if (refusedFor !== form) {
    setRefusedFor(form);
    setError(null);
  }
  // The reset seeds the order once per open from the list it found, so a list that lands later, a slow first read or
  // Try again after a failed one, seeds an order still empty. Every member edit re-sorts the order through its own
  // handler.
  if (open && accounts.length > 0 && memberOrder.length === 0) setMemberOrder(orderedAccountIds(accounts, form.members));

  const set = <K extends keyof PoolForm>(key: K, value: PoolForm[K]) => setForm((f) => ({ ...f, [key]: value }));
  const poolKeys = (pool ? [...localKeys, ...keys.filter((k) => k.poolId === pool.id && !localKeys.some((l) => l.id === k.id))] : localKeys).filter((k) => !moved.has(k.id));
  const otherPools = pools.filter((p) => p.id !== pool?.id);
  // The id the create call derives from the name, which keys created before the pool is saved use too.
  const draftPoolId = slug(form.name.trim());
  const draftName = form.name.trim() || 'new pool';
  const draftCaptureHours = form.capture ? Number(form.captureHours) || 0 : 0;
  const keyDialogPools = useMemo(
    () =>
      pool || pools.some((p) => p.id === draftPoolId)
        ? pools
        : [...pools, { autoModeServer: form.autoModeServer, bodyCaptureHours: draftCaptureHours, guardrails: [], id: draftPoolId, members: Object.keys(form.members).map((accountId) => ({ accountId })), modelRules: form.modelRules, name: draftName }],
    [pool, pools, draftCaptureHours, draftPoolId, draftName, form.autoModeServer, form.members, form.modelRules],
  );
  const keyIds = useMemo(() => new Set([...keys, ...localKeys].map((k) => k.id)), [keys, localKeys]);
  const suggestions = useCatalogSuggestions(Object.keys(form.members), open);
  // Below five minutes a pin expires before the probe it scheduled can help.
  const keepaliveBlocked = form.affinity < 300;
  const shortAffinity = form.affinity >= 300 && form.affinity < 3_600;
  const warming = form.keepalive && !keepaliveBlocked;
  // The router's figures for the saved pool. The console holds no price, so an unsaved edit moves none of them.
  const resolved = pool?.keepaliveResolved ?? null;
  // 0 runs the window to the break-even and any other figure runs as set, above the break-even too, which the note
  // names.
  const idleAtBreakEven: Range | null = resolved && form.maxIdleSeconds === 0 ? resolved.breakEvenSeconds : null;
  const idleAbove: Range | null = resolved && form.maxIdleSeconds > resolved.breakEvenSeconds.min ? resolved.breakEvenSeconds : null;
  // The router caps the two ceilings at the one break-even, so a ceiling switched on past it names the one in effect.
  const parkedCapped = resolved && form.maxParkedOn ? cappedAt(form.maxParkedSeconds, resolved.breakEvenSeconds) : null;
  const scheduledCapped = resolved && form.horizonOn ? cappedAt(form.horizonSeconds, resolved.breakEvenSeconds) : null;
  // The share is the router's for the saved window, so a typed window names the window the share belongs to.
  const savedIdle = pool?.keepalive.maxIdleSeconds ?? form.maxIdleSeconds;
  const shareOf = savedIdle === form.maxIdleSeconds ? '' : ` at the saved ${savedIdle === 0 ? 'break-even' : fmtDuration(savedIdle * 1_000)}`;
  // Sessions by warm prefix and subagents by cohort both place by the shared prefix, so either one reads the threshold.
  const prefixEnabled = form.sessionPlacement === 'by_warm_prefix' || form.subagentPlacement === 'by_cohort';
  const budgetOf = (accountId: string) => form.budgets[accountId] ?? NO_BUDGET;
  const input: CreatePoolInput = {
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
      ? {
          enabled: true,
          maxIdleSeconds: form.maxIdleSeconds,
          maxParkedSeconds: form.maxParkedOn ? form.maxParkedSeconds : null,
          scheduledHorizonSeconds: form.horizonOn ? form.horizonSeconds : null,
        }
      : { ...(pool?.keepalive ?? KEEPALIVE_DEFAULTS), enabled: false },
    members: Object.entries(form.members).map(([accountId, priority]) => {
      const budget = budgetOf(accountId);
      return { accountId, budgetDailyUsd: typedNumber(budget.daily), budgetMonthlyUsd: typedNumber(budget.monthly), priority };
    }),
    // With neither placement reading the threshold it goes back as stored, so an edit left in it holds nothing.
    minSharedPrefixTokens: prefixEnabled
      ? (typedNumber(form.prefixTokens) ?? Number.NaN)
      : (pool?.minSharedPrefixTokens ?? POOL_AFFINITY_DEFAULTS.minSharedPrefixTokens),
    modelRules: form.modelRules,
    name: form.name.trim(),
    overagePolicy: form.overage,
    preferSubscription: form.preferSubscription,
    sessionPlacement: form.sessionPlacement,
    status: form.status,
    strategy: form.strategy,
    subagentPlacement: form.subagentPlacement,
  };
  const listed = new Set(accounts.map((a) => a.id));
  // Until the list answers every stored member stands, so nothing is refused for an id the list may yet hold.
  const refusals = poolRefusals(input, accountsFailed !== null ? null : accountsLoaded ? listed : new Set(Object.keys(form.members)), {
    capture: { maxHours: captureMaxHours, on: form.capture, typed: form.captureHours },
    identity: { isNew: pool === null, poolIds: new Set(pools.map((p) => p.id)) },
  });
  const { first, invalid, refusalOf, refusedFields, refusedGroups } = refusalsView(refusals);
  const valid = first === null;

  const setEngine = <K extends keyof PoolEngineFields>(key: K, value: PoolEngineFields[K]) => setForm((f) => ({ ...f, engine: { ...f.engine, [key]: value } }));
  const pickShedding = (name: SheddingPreset) =>
    setForm((f) => (name === 'custom' ? { ...f, sheddingCustom: true } : { ...f, engine: { ...f.engine, ...SHEDDING_PRESETS[name] }, sheddingCustom: false }));
  const pickFailover = (name: FailoverPreset) =>
    setForm((f) => (name === 'custom' ? { ...f, failoverCustom: true } : { ...f, engine: { ...f.engine, ...FAILOVER_PRESETS[name] }, failoverCustom: false }));
  const rulesRefusal = refusalOf('Model rules');
  const prefixRefusal = refusalOf('Minimum shared prefix, tokens');
  const setBudget = (accountId: string, patch: Partial<PoolForm['budgets'][string]>) =>
    setForm((f) => ({ ...f, budgets: { ...f.budgets, [accountId]: { ...(f.budgets[accountId] ?? NO_BUDGET), ...patch } } }));
  const setCacheTtl = <K extends keyof PoolCacheTtl>(key: K, value: PoolCacheTtl[K]) => setForm((f) => ({ ...f, cacheTtl: { ...f.cacheTtl, [key]: value } }));

  const keysChanged = () => {
    onKeysChanged();
    sessionsRead.reload();
    overview.reload();
  };

  const startMove = (k: ApiKey) => {
    const to = otherPools[0]?.id ?? '';
    setOpenedMoveTo(to);
    setMoveTo(to);
    setMoveError(null);
    setMoving(k);
  };

  const moveKey = async () => {
    if (!moving || !moveTo) return;
    setMoveBusy(true);
    setMoveError(null);
    try {
      await api.updateKey(moving.id, keyInputOf({ ...keyFormOf(moving, moving.poolId), poolId: moveTo }, now), moving.revision);
      setMoved((prev) => new Set(prev).add(moving.id));
      setMoving(null);
      keysChanged();
    } catch (e) {
      setMoveError(messageOf(e));
      // The key moved since the page read it, so the lists are out of date and the dialog's close reads them again.
      if (isStaleEdit(e)) setAttempted(true);
    } finally {
      setMoveBusy(false);
    }
  };

  const revokeKey = async (k: ApiKey) => {
    try {
      await api.revokeKey(k.id);
      setRevoked((prev) => new Set(prev).add(k.id));
      keysChanged();
    } catch (e) {
      setError(messageOf(e));
    }
  };

  const submit = async () => {
    setBusy(true);
    setError(null);
    setStale(null);
    try {
      if (pool) await api.updatePool(pool.id, { ...input, ...guardrailsChange(form.guardrails, opened.guardrails) }, pool.revision);
      else await api.createPool(input);
    } catch (e) {
      // Stated inside the dialog, which stays open: it sits in the browser's top layer, where a toast raised
      // behind its backdrop would never be seen, and the edit that was refused is still on screen.
      setBusy(false);
      if (isStaleEdit(e)) {
        // The row moved under the dialog: the edit is about values that are gone, so the operator reloads and
        // re-applies it rather than saving over what someone else wrote. Cancel reads the lists again as well.
        setStale(e.message);
        setAttempted(true);
        return;
      }
      setError(messageOf(e));
      if (e instanceof ApiError && e.status >= 400) setAttempted(true);
      return;
    }
    setBusy(false);
    onSaved(pool === null);
  };

  // Keyed by group, so a group without a body fails to compile. `POOL_GROUPS` sets the rail order.
  const bodiesOf = (): Record<PoolGroup, ReactNode> => {
    const orderIndex = new Map(memberOrder.map((id, i) => [id, i]));
    const orderedAccounts = [...accounts].sort((a, b) => (orderIndex.get(a.id) ?? accounts.length) - (orderIndex.get(b.id) ?? accounts.length) || a.label.localeCompare(b.label));
    // A stored member the list does not hold gets a row of its own, the only place it can be unticked.
    const unlisted = accountsFailed === null ? Object.keys(form.members).filter((id) => !listed.has(id)).sort() : [];
    const sheddingPill: SheddingPreset = form.sheddingCustom ? 'custom' : sheddingPresetOf(form.engine);
    const failoverPill: FailoverPreset = form.failoverCustom ? 'custom' : failoverPresetOf(form.engine);
    const budgetRefusal = refusals.find((r) => r.member?.column === 'budget_daily' || r.member?.column === 'budget_monthly');
    const budgetRefused = budgetRefusal && {
      label: accounts.find((a) => a.id === budgetRefusal.member?.accountId)?.label ?? budgetRefusal.field,
      refusal: budgetRefusal.text,
    };
    const memberAccounts = accounts.filter((a) => a.id in form.members);
    const readings = memberAccounts.map((a) => {
      const shared = (id: '5h' | '7d') => {
        const w = windowOf(a, id);
        return w === null ? null : { utilization: w.utilization };
      };
      return { fiveHour: shared('5h'), id: a.id, label: a.label, sevenDay: shared('7d') };
    });
    // The route's verdict per stored member, so a member in cooldown is no destination for a shed session.
    const verdict = new Map((pool?.members ?? []).map((m) => [m.accountId, m.eligible]));
    const preview = shedPreview({ ...input, members: input.members.map((m) => ({ ...m, eligible: verdict.get(m.accountId) })) }, accounts, sessions, now);
    // The route's factor per stored member, which a member whose account did not load is read by.
    const factorOf = new Map((pool?.members ?? []).map((m) => [m.accountId, m.factor]));
    const slowPreview = slowDownPreview({ ...input, members: input.members.map((m) => ({ ...m, factor: factorOf.get(m.accountId) ?? null })) }, accounts);
    return {
      identity: (
        <div className="cells">
          <Field
            label="Name"
            className="cell--half"
            hint={
              <>
                The pool&apos;s id is made from this name when the pool is created, and stays when it is renamed.
                {pool === null && form.name.trim() !== '' && ` Id: ${draftPoolId}`}
              </>
            }
          >
            <Input value={form.name} onChange={(e) => set('name', e.target.value)} placeholder="shared" className="mono" aria-invalid={invalid('Name')} />
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
            Lower numbers are tried first: the strategy picks inside the first tier that has an eligible account, after the kind order that <GroupLink id="selection">Prioritize subscription accounts</GroupLink> sets. An account belongs to one pool. Each account&apos;s windows and state are on the{' '}
            <PageLink to="/accounts">Accounts page</PageLink>.
          </p>
          <p className="field__hint">
            A member whose spend reaches its daily or its monthly budget is skipped until that UTC day or month ends, priced at API list rates: an API-key member&apos;s whole spend counts, a subscription member&apos;s metered spend alone (overage and fast answers). Set one, both, or neither.
          </p>
          {accountsFailed !== null ? (
            <ErrorState title="The account list did not load" body={accountsFailed} retry={onRetryAccounts} />
          ) : (
            <div className="table-wrap">
              <table className="table members-table table--plain">
                <thead>
                  <tr>
                    <th className="col-fill">Account</th>
                    <th className="right col-fit">Priority</th>
                    <th className="right col-fit">Daily budget, USD</th>
                    <th className="right col-fit">Monthly budget, USD</th>
                  </tr>
                </thead>
                <tbody>
                  {orderedAccounts.map((a) => {
                    const on = a.id in form.members;
                    // A ticked member keeps its checkbox even once a re-read finds another pool holding it, so the
                    // operator can still untick it.
                    const heldBy = on ? undefined : otherPools.find((p) => p.members.some((m) => m.accountId === a.id));
                    if (heldBy) {
                      return (
                        <tr key={a.id}>
                          <td className="col-fill" title={a.label}>
                            <div className="row">
                              <div className="members-table__check" />
                              <span className="members-table__name ellipsis">{a.label}</span>
                              {a.email && <span className="muted mono">{a.email}</span>}
                              <span className="muted" title={`An account belongs to one pool. Remove it from ${heldBy.id} first.`}>
                                In pool {heldBy.id}
                              </span>
                            </div>
                          </td>
                          <td className="right col-fit">
                            <div className="input members-table__blank">&nbsp;</div>
                          </td>
                          <td className="right col-fit" />
                          <td className="right col-fit" />
                        </tr>
                      );
                    }
                    return (
                      <tr key={a.id}>
                        <td className="col-fill" title={a.label}>
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
                            <span className="members-table__name ellipsis">{a.label}</span>
                            {a.email && <span className="muted mono">{a.email}</span>}
                          </label>
                        </td>
                        <td className="right col-fit">
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
                            aria-invalid={invalid(memberField('priority', a.id))}
                          />
                        </td>
                        <td className="right col-fit">
                          <Input
                            type="number"
                            min={0.01}
                            step="any"
                            value={budgetOf(a.id).daily.text}
                            disabled={!on}
                            placeholder="none"
                            onInput={(e) => setBudget(a.id, { daily: typedOf(e.currentTarget) })}
                            aria-label={`Daily budget for ${a.label}, USD`}
                            aria-invalid={invalid(memberField('budget_daily', a.id))}
                          />
                        </td>
                        <td className="right col-fit">
                          <Input
                            type="number"
                            min={0.01}
                            step="any"
                            value={budgetOf(a.id).monthly.text}
                            disabled={!on}
                            placeholder="none"
                            onInput={(e) => setBudget(a.id, { monthly: typedOf(e.currentTarget) })}
                            aria-label={`Monthly budget for ${a.label}, USD`}
                            aria-invalid={invalid(memberField('budget_monthly', a.id))}
                          />
                        </td>
                      </tr>
                    );
                  })}
                  {unlisted.map((id) => (
                    <tr key={id}>
                      <td className="col-fill" title={id}>
                        <label>
                          <input
                            type="checkbox"
                            checked
                            disabled={!accountsLoaded}
                            title={accountsLoaded ? undefined : 'Waiting for the account list'}
                            onChange={() => {
                              const next = { ...form.members };
                              delete next[id];
                              set('members', next);
                            }}
                            aria-invalid={invalid(memberField('member', id))}
                          />
                          <span className="members-table__name ellipsis">{id}</span>
                          {accountsLoaded && <Badge tone="neutral">not listed</Badge>}
                        </label>
                      </td>
                      <td className="right col-fit">
                        <Input type="number" value={form.members[id]} disabled aria-label={`Priority for ${id}`} />
                      </td>
                      <td className="right col-fit">
                        <Input type="number" value={budgetOf(id).daily.text} disabled placeholder="none" aria-label={`Daily budget for ${id}, USD`} />
                      </td>
                      <td className="right col-fit">
                        <Input type="number" value={budgetOf(id).monthly.text} disabled placeholder="none" aria-label={`Monthly budget for ${id}, USD`} />
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
          {budgetRefused && (
            <Refusal>
              {budgetRefused.label}: {budgetRefused.refusal}
            </Refusal>
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
                <table className="table keys-table table--plain">
                  <thead>
                    <tr>
                      <th className="col-fill">Key</th>
                      <th className="col-fit">Prefix</th>
                      <th className="col-fit">Status</th>
                      <th className="col-fit" />
                    </tr>
                  </thead>
                  <tbody>
                    {poolKeys.map((k) => {
                      const status = revoked.has(k.id) ? 'revoked' : k.status;
                      return (
                        <tr key={k.id}>
                          <td className="col-fill" title={k.name}>{k.name}</td>
                          <td className="mono muted col-fit">{k.prefix}…</td>
                          <td className="col-fit">
                            <Badge tone={KEY_STATUS_TONE[status]} dot>
                              {status}
                            </Badge>
                          </td>
                          <td className="col-fit">
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
              A key reaches only this pool&apos;s members. Create key, Move to and Revoke apply at once, without Save pool. Rotation and expiry are on the <PageLink to="/keys">Keys page</PageLink>.
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
            hint="Top to bottom, first match wins. A model no rule names is allowed, and a closing Deny * allows only what the list names. Write an undated id with a trailing *. A key extends or replaces these rules, and the saved key's Effective list shows the list it ends up with."
          >
            <PatternEditor label="Model rules" value={form.modelRules} onChange={(rules) => set('modelRules', rules)} suggestions={suggestions} />
          </Field>
          {rulesRefusal && <Refusal>{rulesRefusal}</Refusal>}
        </div>
      ),
      guardrails: (
        <div className="cells">
          {/* Field's frame without its label element: a label forwards a click on its text to the first button
              inside. */}
          <Field
            label="Guardrails"
            plain
            hint={
              <>
                Top to bottom. A pool&apos;s guardrails run before a key&apos;s own. <PageLink to="/guardrails">Manage guardrails</PageLink>
              </>
            }
          >
            <GuardrailList onChange={(ids) => set('guardrails', ids)} pending={pool ? undefined : 'Guardrails can be attached once the pool is saved.'} refs={pool?.guardrails ?? []} registry={guardrailRegistry} value={form.guardrails} />
          </Field>
        </div>
      ),
      autoMode: (
        <div className="cells">
          <Expert>
            <Field
              label="Support server-side auto mode"
              beside
              hint={
                <>
                  Claude Code asks the upstream to review each tool use on the model&apos;s own turn. Off, the router drops that request from every message of this pool&apos;s keys, and the client reviews with classifier requests of its own, billed to the account&apos;s windows. A key can override this. The Client page of the documentation explains what the client does in each state.
                </>
              }
            >
              <Toggle checked={form.autoModeServer} onChange={(v) => set('autoModeServer', v)} label="Support server-side auto mode" />
            </Field>
          </Expert>
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
                Plan quota first, then accounts in <GroupLink id="overage">overage</GroupLink>, then the metered accounts, and <GroupLink id="members">priority</GroupLink> orders each of those groups.
              </>
            }
          >
            <Toggle checked={form.preferSubscription} onChange={(v) => set('preferSubscription', v)} label="Prioritize subscription accounts" />
          </Field>
        </div>
      ),
      placement: (
        <div className="cells">
          <Expert refused={refusedFields.has('Subagent placement')}>
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
          </Expert>
          <Expert refused={refusedFields.has('Session placement')}>
            <Field
              label="Session placement"
              hint={
                <>
                  By warm prefix lands a fresh session where its static prefix is already cached, when the prefix reaches the minimum below. By strategy leaves it to the <GroupLink id="selection">strategy</GroupLink>.
                </>
              }
            >
              <Select value={form.sessionPlacement} onChange={(e) => set('sessionPlacement', e.target.value as SessionPlacement)} aria-label="Session placement" aria-invalid={invalid('Session placement')}>
                <option value="by_strategy">By strategy</option>
                <option value="by_warm_prefix">By warm prefix</option>
              </Select>
            </Field>
          </Expert>
          <Expert refused={refusedFields.has('Minimum shared prefix, tokens')}>
            <Field
              label="Minimum shared prefix, tokens"
              hint={
                <>
                  The smallest shared prefix that places a fresh session, or a subagent by cohort, by warmth. Below it the <GroupLink id="selection">strategy</GroupLink> decides. Estimated from the request&apos;s bytes, not counted by the upstream.
                </>
              }
            >
              <Input
                type="number"
                min={0}
                step={1_000}
                value={form.prefixTokens.text}
                onInput={(e) => set('prefixTokens', typedOf(e.currentTarget))}
                disabled={!prefixEnabled}
                aria-label="Minimum shared prefix, tokens"
                aria-invalid={invalid('Minimum shared prefix, tokens')}
              />
            </Field>
          </Expert>
          {prefixRefusal && <Refusal>{prefixRefusal}</Refusal>}
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
          <Expert refused={refusedFields.has('Pin sessions')}>
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
          </Expert>
        </div>
      ),
      utilization: (
        <>
          <p className="dialog-group__intro">
            The windows the router works from: the share of each it keeps every account under, and the span each session&apos;s burn is averaged over.
          </p>
          <div className="cells">
            <Field
              label="Window ceiling (%)"
              className={expertView || refusedFields.has(ENGINE_FIELD_LABELS.burnAverageSeconds) ? 'cell--half' : undefined}
              hint="A move or a new session lands on a member only where its windows stay under this share until they reset, or for the next five hours on a weekly window. A round runs on a member whose window would pass it."
            >
              <Input
                type="number"
                min={CEILING_PERCENT.min}
                max={CEILING_PERCENT.max}
                value={percentOf(form.engine.windowCeiling)}
                onChange={(e) => setEngine('windowCeiling', Number(e.target.value) / 100)}
                aria-label="Window ceiling (%)"
                aria-invalid={invalid('Window ceiling (%)')}
              />
            </Field>
            <Expert refused={refusedFields.has(ENGINE_FIELD_LABELS.burnAverageSeconds)}>
              <Field label={ENGINE_FIELD_LABELS.burnAverageSeconds} className="cell--half" hint="The time constant of each session's burn average. The projection, the slow-down and the round read it. A 5-hour window younger than twice it fires no round. A slowed member's holds reach their full length over it. Editable under the Custom preset in Load shedding.">
                <Input
                  type="number"
                  min={ENGINE_BOUNDS.burnAverageSeconds.min}
                  step={60}
                  value={form.engine.burnAverageSeconds}
                  onChange={(e) => setEngine('burnAverageSeconds', Number(e.target.value))}
                  disabled={sheddingPill !== 'custom'}
                  aria-invalid={invalid(ENGINE_FIELD_LABELS.burnAverageSeconds)}
                />
              </Field>
            </Expert>
            <Field
              label="Member utilization"
              plain
              hint={
                <>
                  Each tick is a member&apos;s latest 5-hour or 7-day reading, as on the <PageLink to="/quota">Quota page</PageLink>. Drag the ceiling&apos;s handle, or focus it and use the arrow keys.
                </>
              }
            >
              <UtilizationBar
                ceiling={{ onChange: (v) => setEngine('windowCeiling', v / 100), value: percentOf(form.engine.windowCeiling) }}
                min={CEILING_PERCENT.min}
                max={CEILING_PERCENT.max}
                readings={readings}
              />
            </Field>
          </div>
        </>
      ),
      shedding: (
        <>
          <p className="dialog-group__intro">
            A member whose window would pass the <GroupLink id="utilization">ceiling</GroupLink> before its reset, or within five hours on a weekly window, runs a round that marks the sessions that cover the excess, and each moves to a sibling with runway at its next request. With the slow-down on, a round fires when the time the slow-down adds passes the stretch threshold, and removes only what the slow-down cannot cover.
          </p>
          <div className="cells">
            <Field label="Load shedding" beside hint="Off, no round runs and nothing is marked. The ceiling still holds every move and new session, and Migrate when spent keeps its own switch.">
              <Toggle checked={form.engine.loadShedding} onChange={(v) => setEngine('loadShedding', v)} label="Load shedding" />
            </Field>
            <Expert>
              <Field label="Preset" plain hint={SHEDDING_HINT[sheddingPill]}>
                <PresetPills label="Load shedding preset" value={sheddingPill} onPick={pickShedding} hints={SHEDDING_HINT} />
              </Field>
            </Expert>
            {/* An expert half halves its row only while its mate draws, in the expert view or under the mate's refusal,
                so a refused half drawn alone in the plain view spans its row. */}
            <Expert refused={refusedFields.has('Shed cap (%)')}>
              <Field label="Shed cap (%)" className={expertView || refusedFields.has(ENGINE_FIELD_LABELS.shedDwellSeconds) ? 'cell--half' : undefined} hint="The most of the member's burn one round removes. The round removes what brings the window under the ceiling, up to this.">
                <Input
                  type="number"
                  min={CAP_PERCENT.min}
                  max={CAP_PERCENT.max}
                  value={percentOf(form.engine.shedCap)}
                  onChange={(e) => setEngine('shedCap', Number(e.target.value) / 100)}
                  disabled={sheddingPill !== 'custom'}
                  aria-invalid={invalid('Shed cap (%)')}
                />
              </Field>
            </Expert>
            <Expert refused={refusedFields.has(ENGINE_FIELD_LABELS.shedDwellSeconds)}>
              <Field label={ENGINE_FIELD_LABELS.shedDwellSeconds} className={expertView || refusedFields.has('Shed cap (%)') ? 'cell--half' : undefined} hint="The least time between two rounds on one member, and how long a moved session stays off it.">
                <Input
                  type="number"
                  min={ENGINE_BOUNDS.shedDwellSeconds.min}
                  step={60}
                  value={form.engine.shedDwellSeconds}
                  onChange={(e) => setEngine('shedDwellSeconds', Number(e.target.value))}
                  disabled={sheddingPill !== 'custom'}
                  aria-invalid={invalid(ENGINE_FIELD_LABELS.shedDwellSeconds)}
                />
              </Field>
            </Expert>
            <Expert refused={refusedFields.has('Warm sessions')}>
              <Field
                label="Warm sessions"
                plain
                hint={
                  <>
                    A warm session rewrites its cache where it lands. The <CostReferenceLink tab="plan-rates" /> shows what that write costs.
                  </>
                }
              >
                <ChoiceList choices={WARM_CHOICES} invalid={invalid('Warm sessions')} label="Warm sessions" name={`warm-${pool?.id ?? 'new'}`} onChange={(v) => setEngine('warmSessions', v)} value={form.engine.warmSessions} />
              </Field>
            </Expert>
            <Expert>
              <Field label="Preview" plain>
                <p className="shed-preview num" role="status">
                  {shedPreviewLine(preview, now)}
                </p>
              </Field>
            </Expert>
          </div>
        </>
      ),
      slowDown: (
        <>
          <p className="dialog-group__intro">
            The slow-down holds each request of a session before it is forwarded, so an account whose sessions would pass the ceiling before the reset reaches it under the ceiling instead. A session waits its own cycle times the slow-down, so a fast session waits more than a slow one and each keeps its share.
          </p>
          <div className="cells">
            <Expert>
              <Field label="Slow-down" beside hint="Holds requests on a member whose five-hour window would pass the ceiling. Auxiliary requests, compactions, the fast lane, probes and token counts are never held.">
                <Toggle checked={form.engine.slowDown} onChange={(v) => setEngine('slowDown', v)} label="Slow-down" />
              </Field>
            </Expert>
            <Expert refused={refusedFields.has(ENGINE_FIELD_LABELS.slowDownStretchSeconds)}>
              <Field
                label={ENGINE_FIELD_LABELS.slowDownStretchSeconds}
                className={expertView || refusedFields.has(ENGINE_FIELD_LABELS.slowDownHoldSeconds) ? 'cell--half' : undefined}
                hint="The time the slow-down may add to a session before a round moves sessions off the member, with load shedding on. Under it the slow-down holds alone."
              >
                <Input
                  type="number"
                  min={ENGINE_BOUNDS.slowDownStretchSeconds.min}
                  max={ENGINE_BOUNDS.slowDownStretchSeconds.max}
                  step={60}
                  value={form.engine.slowDownStretchSeconds}
                  onChange={(e) => setEngine('slowDownStretchSeconds', Number(e.target.value))}
                  aria-invalid={invalid(ENGINE_FIELD_LABELS.slowDownStretchSeconds)}
                />
              </Field>
            </Expert>
            <Expert refused={refusedFields.has(ENGINE_FIELD_LABELS.slowDownHoldSeconds)}>
              <Field label={ENGINE_FIELD_LABELS.slowDownHoldSeconds} className={expertView || refusedFields.has(ENGINE_FIELD_LABELS.slowDownStretchSeconds) ? 'cell--half' : undefined} hint="The longest one request waits. 120 s leaves 60 s of the client's 180 s first-byte window for the upstream's head.">
                <Input
                  type="number"
                  min={ENGINE_BOUNDS.slowDownHoldSeconds.min}
                  max={ENGINE_BOUNDS.slowDownHoldSeconds.max}
                  value={form.engine.slowDownHoldSeconds}
                  onChange={(e) => setEngine('slowDownHoldSeconds', Number(e.target.value))}
                  aria-invalid={invalid(ENGINE_FIELD_LABELS.slowDownHoldSeconds)}
                />
              </Field>
            </Expert>
            <Expert>
              <Field label="Preview" plain>
                {/* Drawn as the shedding preview is, under a class of its own so a check reads the one it means. */}
                <p className="shed-preview slow-down-preview num" role="status">
                  {slowDownPreviewLine(slowPreview)}
                </p>
              </Field>
            </Expert>
          </div>
        </>
      ),
      failover: (
        <>
          <p className="dialog-group__intro">
            How long a request waits out a throttle on its member, how many requests one member serves at once, and whether a spent window moves the session at once.
          </p>
          <div className="cells">
            <Expert>
              <Field label="Preset" plain hint={FAILOVER_HINT[failoverPill]}>
                <PresetPills label="Failover preset" value={failoverPill} onPick={pickFailover} hints={FAILOVER_HINT} />
              </Field>
            </Expert>
            <Expert refused={refusedFields.has(ENGINE_FIELD_LABELS.inFlightCap)}>
              <Field
                label={ENGINE_FIELD_LABELS.inFlightCap}
                className={expertView || refusedFields.has(ENGINE_FIELD_LABELS.retryInPlaceSeconds) ? 'cell--half' : undefined}
                hint="Requests one member serves at once. A member at the cap takes no fresh session while a sibling has room. With every member full, a request waits up to Retry in place for a slot."
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
            </Expert>
            <Expert refused={refusedFields.has(ENGINE_FIELD_LABELS.retryInPlaceSeconds)}>
              <Field label={ENGINE_FIELD_LABELS.retryInPlaceSeconds} className={expertView || refusedFields.has(ENGINE_FIELD_LABELS.inFlightCap) ? 'cell--half' : undefined} hint="The longest throttle a request waits out on its member before failing over, and the longest wait for a free slot. At 0 a request fails over at once. Claude Code gives up on a request that has waited about 180 seconds without an answer and retries it on its own, so a longer wait is spent for nothing.">
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
            </Expert>
            <Expert>
              <Field label="Migrate when spent" beside hint="Moves a session off a member whose window for its model is spent, to a sibling with allowance left, without waiting for a round or the upstream's 429.">
                <Toggle checked={form.engine.migrateWhenSpent} onChange={(v) => setEngine('migrateWhenSpent', v)} label="Migrate when spent" />
              </Field>
            </Expert>
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
                For subscription accounts billing extra usage. A daily or monthly budget in <GroupLink id="members">Members</GroupLink> caps a member&apos;s spend in dollars: an API-key member&apos;s whole spend, a subscription member&apos;s overage and fast answers. The <PageLink to="/accounts">Accounts page</PageLink> shows which account is in overage.
              </>
            }
          >
            <ChoiceList choices={OVERAGE_CHOICES} label="Overage policy" name={`overage-${pool?.id ?? 'new'}`} onChange={(v) => set('overage', v)} value={form.overage} />
          </Field>
          <Expert refused={refusedFields.has(ENGINE_FIELD_LABELS.overageMoveGraceMinutes)}>
            <Field label={ENGINE_FIELD_LABELS.overageMoveGraceMinutes} hint="Under Prefer plan quota, a session stays on an account in overage while its window resets within this many minutes. At 0 it moves at once.">
              <Input
                type="number"
                min={ENGINE_BOUNDS.overageMoveGraceMinutes.min}
                max={ENGINE_BOUNDS.overageMoveGraceMinutes.max}
                value={form.engine.overageMoveGraceMinutes}
                onChange={(e) => setEngine('overageMoveGraceMinutes', Number(e.target.value))}
                aria-invalid={invalid(ENGINE_FIELD_LABELS.overageMoveGraceMinutes)}
              />
            </Field>
          </Expert>
          <Expert refused={refusedFields.has('Return to plan')}>
            <Field label="Return to plan" plain hint="When a session on an API-key member returns to a subscription member. Under Never it leaves the metered member on expiry, a cooldown or a mark alone.">
              <ChoiceList choices={RETURN_CHOICES} invalid={invalid('Return to plan')} label="Return to plan" name={`return-${pool?.id ?? 'new'}`} onChange={(v) => setEngine('returnToPlan', v)} value={form.engine.returnToPlan} />
            </Field>
          </Expert>
          <Expert refused={refusedFields.has(ENGINE_FIELD_LABELS.returnToPlanDwellSeconds)}>
            <Field label={ENGINE_FIELD_LABELS.returnToPlanDwellSeconds} hint="Under Always, how long a warm session stays on the API-key member before it returns. 0 returns it at once.">
              <Input
                type="number"
                min={ENGINE_BOUNDS.returnToPlanDwellSeconds.min}
                max={ENGINE_BOUNDS.returnToPlanDwellSeconds.max}
                value={form.engine.returnToPlanDwellSeconds}
                onChange={(e) => setEngine('returnToPlanDwellSeconds', Number(e.target.value))}
                onFocus={() => setDwellFocused(true)}
                onBlur={() => setDwellFocused(false)}
                // A refused dwell stays editable whatever the mode, or its hold on the save could not be cleared.
                disabled={form.engine.returnToPlan !== 'always' && !refusedFields.has(ENGINE_FIELD_LABELS.returnToPlanDwellSeconds) && !dwellFocused}
                aria-invalid={invalid(ENGINE_FIELD_LABELS.returnToPlanDwellSeconds)}
              />
            </Field>
          </Expert>
          <Expert refused={refusedFields.has(ENGINE_FIELD_LABELS.windDownMinutes)}>
            <Field
              label={ENGINE_FIELD_LABELS.windDownMinutes}
              hint={
                <>
                  This long before a canceled subscription ends, its account takes new sessions only as the last resort and cold sessions move off. Inside it the account is no destination for a move. The end is set on the <PageLink to="/accounts">Accounts page</PageLink>.
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
          </Expert>
        </div>
      ),
      cacheTtl: (
        <div className="cells">
          <Field label="Set the cache TTL for this pool" beside hint="Rewrites every cache marker a request carries to the TTL below, on every account of this pool, so the client's own TTL setting has no effect through it.">
            <Toggle checked={form.cacheTtl.enabled} onChange={(v) => setCacheTtl('enabled', v)} label="Set the cache TTL for this pool" />
          </Field>
          <Expert>
            <Field
              label="Add a cache marker to a request without one"
              beside
              hint={
                <>
                  Adds a marker to a request that carries none. Off leaves such a request as sent. <GroupLink id="pinning">Pin sessions</GroupLink> counts an added marker.
                </>
              }
            >
              <Toggle checked={form.cacheTtl.injectWhenAbsent} onChange={(v) => setCacheTtl('injectWhenAbsent', v)} label="Add a cache marker to a request without one" disabled={!form.cacheTtl.enabled} />
            </Field>
          </Expert>
          <Field
            label="Write cost"
            plain
            hint="Plan writes apply while an account spends plan quota, where a 1-hour write costs the same as a 5-minute one. Metered writes apply while a write costs money, on an API-key account or a subscription account in overage: 2x input for an hour against 1.25x for five minutes. Reads cost the same either way."
          >
            <div className="row">
              <CostReferenceLink tab="api-prices" />
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
          <Expert>
            <CacheTtlOverrides
              key={`plan-${open}-${pool?.id ?? 'new'}`}
              name="plan"
              label="Plan writes"
              disabled={!form.cacheTtl.enabled}
              rules={form.cacheTtl.plan}
              onChange={(rules) => setCacheTtl('plan', rules)}
              models={models}
            />
          </Expert>
          <Expert>
            <CacheTtlOverrides
              key={`metered-${open}-${pool?.id ?? 'new'}`}
              name="metered"
              label="Metered writes"
              disabled={!form.cacheTtl.enabled}
              rules={form.cacheTtl.metered}
              onChange={(rules) => setCacheTtl('metered', rules)}
              models={models}
            />
          </Expert>
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
                  Probes an idle session shortly before its cache expires, so its next turn reads instead of writes. A probe follows the session&apos;s pin, so <GroupLink id="pinning">Session affinity</GroupLink> must be at least 5m.
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
            <Field
              label={KEEPALIVE_FIELD_LABELS.maxIdleSeconds}
              hint={
                <>
                  The idle window of the main conversation. 0 is the recipe&apos;s break-even, and a value above the break-even runs as set.
                  {resolved && (
                    <span className="keepalive-resolved">
                      <span title={SHARE_HINT}>
                        Costs {rangeText(resolved.idleShare, fmtPct)} of one re-write{shareOf}
                      </span>
                      {idleAtBreakEven && (
                        <>
                          {' · '}capped at the break-even, <RangeFigures range={idleAtBreakEven} />
                        </>
                      )}
                    </span>
                  )}
                  {idleAbove && <BreakEvenNote lead="Above the break-even," range={idleAbove} />}
                </>
              }
            >
              <Input
                type="number"
                min={KEEPALIVE_BOUNDS.maxIdleSeconds.min}
                max={KEEPALIVE_BOUNDS.maxIdleSeconds.max}
                value={form.maxIdleSeconds}
                onChange={(e) => set('maxIdleSeconds', Number(e.target.value))}
                disabled={!warming}
                aria-invalid={invalid(KEEPALIVE_FIELD_LABELS.maxIdleSeconds)}
              />
            </Field>
            <div className="cell--half">
              <SwitchedNumber
                label={KEEPALIVE_FIELD_LABELS.maxParkedSeconds}
                disabled={!warming}
                checked={form.maxParkedOn}
                onChange={(v) => set('maxParkedOn', v)}
                input={{ min: KEEPALIVE_BOUNDS.maxParkedSeconds.min, max: KEEPALIVE_BOUNDS.maxParkedSeconds.max, value: form.maxParkedSeconds, onChange: (e) => set('maxParkedSeconds', Number(e.target.value)), 'aria-label': KEEPALIVE_FIELD_LABELS.maxParkedSeconds, 'aria-invalid': invalid(KEEPALIVE_FIELD_LABELS.maxParkedSeconds) }}
                hint={
                  <>
                    How long a park on a background command or a subagent is probed. Off is the recipe&apos;s break-even.
                    {resolved && !form.maxParkedOn && <BreakEvenNote lead="Break-even:" range={resolved.breakEvenSeconds} />}
                    {parkedCapped && <BreakEvenNote lead="Capped at the break-even," range={parkedCapped} />}
                  </>
                }
              />
            </div>
            <div className="cell--half">
              <SwitchedNumber
                label={KEEPALIVE_FIELD_LABELS.scheduledHorizonSeconds}
                disabled={!warming}
                checked={form.horizonOn}
                onChange={(v) => set('horizonOn', v)}
                input={{ min: KEEPALIVE_BOUNDS.scheduledHorizonSeconds.min, max: KEEPALIVE_BOUNDS.scheduledHorizonSeconds.max, value: form.horizonSeconds, onChange: (e) => set('horizonSeconds', Number(e.target.value)), 'aria-label': KEEPALIVE_FIELD_LABELS.scheduledHorizonSeconds, 'aria-invalid': invalid(KEEPALIVE_FIELD_LABELS.scheduledHorizonSeconds) }}
                hint={
                  <>
                    A park on a scheduled wakeup is probed while the wake is at most this far away. Off is the recipe&apos;s break-even.
                    {resolved && !form.horizonOn && <BreakEvenNote lead="Break-even:" range={resolved.breakEvenSeconds} />}
                    {scheduledCapped && <BreakEvenNote lead="Capped at the break-even," range={scheduledCapped} />}
                  </>
                }
              />
            </div>
          </div>
          <ReturnRate {...idleReturns} />
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
      bodyCapture: (
        <div className="cells">
          <Field
            label="Capture request and response bodies"
            beside
            hint={
              <>
                Keeps the request and response bodies of this pool&apos;s keys for the request drawer on the <PageLink to="/requests">Requests page</PageLink>. A key can override it, and only the admin secret can read them.
              </>
            }
          >
            <Toggle checked={form.capture} onChange={(v) => set('capture', v)} label="Capture request and response bodies" />
          </Field>
          <Field
            label="Retention (hours)"
            hint={
              <>
                At most the bootstrap&apos;s body capture limit, {captureMaxHours !== undefined && `${plural(captureMaxHours, 'hour')} here, `}shown on the <PageLink to="/settings">Settings page&apos;s About card</PageLink>.
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
          <Expert>
            <Field
              label="Cache diagnostics"
              beside
              hint={
                <>
                  On asks the upstream why a cache read missed, on every account of this pool, and the reason lands in the <PageLink to="/requests">request log</PageLink>.
                </>
              }
            >
              <Toggle checked={form.cacheDiagnostics} onChange={(v) => set('cacheDiagnostics', v)} label="Cache diagnostics" />
            </Field>
          </Expert>
        </div>
      ),
    };
  };
  const bodies = open ? bodiesOf() : null;
  const warnings = pool?.warnings ?? [];
  const groups: DialogGroup[] = POOL_GROUPS.map((id) => ({ ...POOL_RAIL[id], body: bodies?.[id] ?? null, expertOnly: EXPERT_ONLY.has(id), id, mark: pool ? undefined : CREATE_MARKS[id], refused: refusedGroups.has(id) }));
  const save = saveStatus({ error, first, stale, subject: pool ? 'the pool' : null });

  return (
    <>
      <GroupedDialog
        open={open}
        onClose={() => onClose(attempted)}
        title={pool ? `Edit pool ${pool.name}` : 'Create pool'}
        // Key order is dropped on purpose: `members` is rebuilt in tick order, so a member unticked and ticked again at
        // its old priority reads as no change (`budgets` keeps an unticked member's entry, so it is not rebuilt).
        unsaved={!sameValues(form, opened)}
        railLabel="Pool settings"
        groups={groups}
        chosen={chosen}
        initialGroup={initialGroup}
        onChoose={(id) => setChosen(id as PoolGroup)}
        status={save.status}
        hold={save.hold}
        held={save.held}
        // The stale save's callout first, since only Reload clears it, then the saved pool's warnings, which a new pool
        // has none of until the route computes them.
        notice={
          stale || warnings.length > 0 ? (
            <div className="pool-notice">
              {stale && (
                <StaleNotice>
                  <StaleRefusal message={stale} onReload={() => onClose(true)} />
                </StaleNotice>
              )}
              {warnings.length > 0 && <PoolWarnings warnings={warnings} />}
            </div>
          ) : null
        }
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
        captureMaxHours={captureMaxHours}
        onClose={() => setKeyOpen(false)}
        pools={keyDialogPools}
        existingIds={keyIds}
        initialPoolId={pool ? pool.id : draftPoolId}
        lockPool
        onCreated={(c) => {
          setKeyOpen(false);
          setLocalKeys((prev) => [c.key, ...prev]);
          setCreated(c);
          keysChanged();
        }}
      />
      <Dialog
        open={moving !== null}
        // Held while the move is out: its answer writes into whichever key's dialog is open when it lands.
        onClose={() => {
          if (!moveBusy) setMoving(null);
        }}
        title={moving ? `Move key ${moving.name}` : 'Move key'}
        // False while the move is out, so an outside click or Escape reaches the hold above rather than a question.
        unsaved={!moveBusy && moving !== null && moveTo !== openedMoveTo}
        subtitle="A key reaches only its own pool's members, so moving it ends its current session pins."
        width={440}
        footer={
          <>
            <Button variant="ghost" disabled={moveBusy} onClick={() => setMoving(null)}>
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
      <SecretDialog secret={created?.secret ?? null} title="Key created" onClose={() => setCreated(null)} />
    </>
  );
}
