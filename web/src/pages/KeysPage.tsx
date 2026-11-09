import { useEffect, useMemo, useRef, useState } from 'react';
import { useLocation } from 'react-router-dom';
import { api } from '@/api';
import { oneOf } from '@/api/decode';
import type { ApiKey, CreatedApiKey, FundedSum, ModelRulesMode, Pool } from '@/api/types';
import { useRun, useSession } from '@/app/session';
import { useAppSettings } from '@/app/settings';
import { useAddressEdit } from '@/app/useAddressEdit';
import { useHashLanding } from '@/app/useHashLanding';
import { useChartInk } from '@/components/charts/useChartInk';
import { FilterBar } from '@/components/FilterBar';
import { KEY_STATUS_TONE, KeyDialog, SecretDialog } from '@/components/KeyDialog';
import { SortControl } from '@/components/SortControl';
import { PopoverHost, useHoldHost } from '@/components/ui/Popover';
import { sumTileOf, Tile } from '@/components/ui/Tile';
import { IconPlus } from '@/components/ui/icons';
import { Badge, Button, Card, ConfirmDialog, EmptyState, ErrorState, Menu, Skeleton, SkeletonRows } from '@/components/ui/primitives';
import { EDIT_PARAM, EDIT_PARAMS, KEY_SORT_KEYS, KEYS_CODEC, sortDirOf, type KeysAddress } from '@/lib/address';
import { dollarsUnder, fundedRankOf, nativeSidesOf, sidesLine, sumFunded, UNPRICED_TITLE, type WeekReading } from '@/lib/analytics';
import { effectiveCaptureHours } from '@/lib/capture';
import { KEYS_DIMENSIONS, notLoadedReason } from '@/lib/filterBar';
import { fmtAbsolute, fmtCompact, fmtDate, fmtInt, fmtRelative, fmtUsd, noun, plural } from '@/lib/format';
import { KEY_GROUPS, type KeyGroup } from '@/lib/forms';
import { useAsync, useNow, useQueryFilters, useWeekReading } from '@/lib/hooks';
import { orderRows, type SortFigure } from '@/lib/sort';
import type { UnitPosition } from '@/lib/unit';

const captureLabel = (hours: number) => (hours > 0 ? `bodies captured, ${hours}h` : 'bodies not captured');
/** The capture line whole, its inherited mark included, for the title of a line the fill column cuts. */
const captureLine = (key: Pick<ApiKey, 'bodyCaptureHours'>, pool: Pick<Pool, 'bodyCaptureHours'> | null) => `${captureLabel(effectiveCaptureHours(key, pool))}${key.bodyCaptureHours === null ? ' inherited' : ''}`;
const CAPTURE_INHERIT_HINT = 'This key sets no capture of its own, so it follows its pool.';
/** The Models summary of a key without rules of its own. */
const NO_RULES: Record<ModelRulesMode, string> = { extend: 'Pool rules', replace: 'No rules' };
const RETIRED_SECRET_HINT = 'Sessions whose newest request presented the previous secret. A session that registered a companion and sent no request since counts as switched.';
/** The note under the Status badge. The fill column cuts it, so its title opens with the whole of it. */
const retiredNote = (sessions: number) => `${plural(sessions, 'session')} still on the previous secret`;
/** The week's requests under a key's name, which the line's title repeats since the fill column can cut it. */
const weekRequests = (k: Pick<ApiKey, 'usage7d'>) => `${plural(k.usage7d.requests, 'request')}, 7d`;
const STATUS_OPTIONS = {
  status: [
    { label: 'Active', value: 'active' },
    { label: 'Disabled', value: 'disabled' },
    { label: 'Expired', value: 'expired' },
    { label: 'Revoked', value: 'revoked' },
  ],
};

/**
 * A key's figure as a label or a line reads it: native's two sides on one line in the chosen week, a side with no rows
 * left out and a dash titled `empty` where neither has any, and a dollar position's dollars. A key spans its pool's
 * accounts, so no one account's windows bound it and it carries no hint. Null under native while the scale is out.
 */
function keyFigureOf(unit: UnitPosition, s: FundedSum, empty: string, reading: WeekReading): { text: string; title?: string } | null {
  if (unit !== 'native') return { text: fmtUsd(dollarsUnder(unit, s)) };
  const sides = nativeSidesOf(s);
  if (sides.equivalent === undefined && sides.billed === undefined) return { text: '-', title: empty };
  if (reading.scale === null) return null;
  return { text: sidesLine(sides, reading.week, reading.scale), title: sides.equivalent === null ? UNPRICED_TITLE : undefined };
}
/** A key's figure while the scale is out: the skeleton, or the reading's failure as a dash titled with it. */
function PendingFigure({ failure }: { failure: WeekReading['failure'] }) {
  return failure === null ? <Skeleton width={72} height={12} /> : <span title={failure.text}>-</span>;
}

/** A key row's 7-day figure under its tokens. */
function WeekLine({ reading, unit, usage }: { reading: WeekReading; unit: UnitPosition; usage: FundedSum }) {
  const figure = keyFigureOf(unit, usage, 'No request in the last 7 days', reading);
  return (
    <span className="sub" title={figure?.title}>
      {figure === null ? <PendingFigure failure={reading.failure} /> : figure.text}
    </span>
  );
}

export function KeysPage() {
  const { unit, week } = useAppSettings();
  // The cost model whose capacities scale every figure to the chosen week. The Pro week reads without it.
  const { reading, settings } = useWeekReading(week);
  const now = useNow();
  const ink = useChartInk();
  const location = useLocation();
  const pageRef = useRef<HTMLDivElement>(null);
  // The dialog's one-shot `edit` and `group` wait in the address until the key list loads, so a filter written before
  // then carries them through.
  const { address, filters, replaceLater, update } = useQueryFilters(KEYS_CODEC, { keep: EDIT_PARAMS });
  const status = filters.status;
  const dir = sortDirOf(KEY_SORT_KEYS, filters);
  const [dialog, setDialog] = useState<{ initialGroup: KeyGroup | null; key: ApiKey | null } | null>(null);
  // Held only while the dialog that shows it is open, because the router keeps no copy of a secret.
  const [created, setCreated] = useState<{ key: CreatedApiKey; title: string } | null>(null);
  const [confirm, setConfirm] = useState<{ body: string; danger?: boolean; label: string; onConfirm: () => void; title: string } | null>(null);
  const { notify } = useSession();
  const [intervalMs, setIntervalMs] = useState(5_000);
  // The key dialog resets its fields whenever the pool list changes, so a refresh landing under it would wipe them. A
  // row's open menu holds the page through its host, since an answer landing under it could add or drop rows beneath
  // it.
  const { cause: pauseCause, host } = useHoldHost();
  const paused = pauseCause !== null;
  const keys = useAsync(() => api.listKeys(), [], { paused, refreshMs: intervalMs });
  const pools = useAsync(() => api.listPools(), [], { paused, refreshMs: intervalMs });

  useAddressEdit(keys.data, 'Key', (key, group) => setDialog({ initialGroup: oneOf(KEY_GROUPS, group) ?? 'key', key }));

  const poolList = useMemo(() => pools.data ?? [], [pools.data]);
  const poolById = useMemo(() => new Map(poolList.map((p) => [p.id, p])), [poolList]);
  const keyIds = useMemo(() => new Set((keys.data ?? []).map((k) => k.id)), [keys.data]);
  // The pool filter is the address: the Pool chips read `?pools=<id>,<id>`, one per id, and write it back, so a reload
  // keeps the filter and the pool card's key count link lands on it. Until the pools are read the address filters as
  // written, so the rows match it during the read and after a failed one, and the Pool list holds an option for each
  // id, which its chip prints. Once the pools have loaded, an id no pool carries leaves the address, so no chip stands
  // for an id the rows no longer filter by. While a dialog query is still in the address, its own effect goes first.
  const poolIds = useMemo(() => filters.pools.filter((id) => pools.data === null || poolById.has(id)), [filters.pools, poolById, pools.data]);
  const poolOptions = useMemo(() => [...poolList.map((p) => ({ label: p.name, value: p.id })), ...filters.pools.filter((id) => !poolById.has(id)).map((id) => ({ label: id, value: id }))], [filters.pools, poolById, poolList]);
  useEffect(() => {
    if (!pools.data || poolIds.length === filters.pools.length) return;
    if (new URLSearchParams(location.search).has(EDIT_PARAM)) return;
    replaceLater({ ...filters, pools: poolIds });
  }, [filters, location.search, poolIds, pools.data, replaceLater]);
  const rows = useMemo(
    () =>
      (keys.data ?? []).filter((k) => {
        if (poolIds.length > 0 && !poolIds.includes(k.poolId)) return false;
        if (status !== 'all' && k.status !== status) return false;
        const q = filters.search.trim().toLowerCase();
        return !q || k.name.toLowerCase().includes(q) || k.prefix.includes(q) || k.poolId.includes(q);
      }),
    [keys.data, poolIds, filters.search, status],
  );
  // The table alone takes the order: the lifetime card ranks its bars by cost whatever the key.
  const ordered = useMemo(() => {
    const figures: Record<KeysAddress['sort'], (k: ApiKey) => SortFigure> = {
      cost: (k) => fundedRankOf(unit, k.usage7d),
      created: (k) => Date.parse(k.createdAt),
      expiry: (k) => (k.expiresAt === null ? null : Date.parse(k.expiresAt)),
      lastUsed: (k) => (k.lastUsedAt === null ? null : Date.parse(k.lastUsedAt)),
      name: (k) => k.name,
      requests: (k) => k.usage7d.requests,
      status: (k) => k.status,
      tokens: (k) => k.usage7d.tokens,
    };
    return orderRows(rows, figures[filters.sort], dir, (k) => k.name);
  }, [dir, filters.sort, rows, unit]);

  // `/keys#<key id>` lands on the row once the keys have drawn it. A filter that would hide the row is cleared first,
  // and the next run finds the row drawn.
  useHashLanding(
    pageRef,
    'tbody tr',
    (id) => {
      if (!keys.data?.some((k) => k.id === id)) return false;
      if (rows.some((k) => k.id === id)) return true;
      replaceLater({ ...filters, pools: [], search: '', status: 'all' });
      return false;
    },
    [filters, keys.data, replaceLater, rows],
  );

  // The page re-renders on its two polls and the clock, so the sums and the lifetime card's order follow the keys and
  // the unit alone.
  const { byRank, maxRank, totals } = useMemo(() => {
    const data = keys.data;
    // A key a plan row priced nothing on ranks null, since its priced rows alone would draw a length its traffic lacks.
    const rank = (k: ApiKey) => fundedRankOf(unit, k.lifetime);
    return {
      // An unknown rank sorts below a zero, so a flagged key comes last.
      byRank: data === null ? null : [...data].sort((a, b) => (rank(b) ?? -1) - (rank(a) ?? -1)),
      // The top rank sets the full width: a fixed floor such as a dollar would dwarf the equivalent's bars, each a small
      // fraction of a window.
      maxRank: Math.max(0, ...(data ?? []).map(rank).filter((r) => r !== null)),
      totals:
        data === null
          ? null
          : {
              keys: data.length,
              active: data.filter((k) => k.status === 'active').length,
              requests: data.reduce((s, k) => s + k.lifetime.requests, 0),
              lifetime: sumFunded(data.map((k) => k.lifetime)),
            },
    };
  }, [keys.data, unit]);
  const lifetimeTile = totals ? sumTileOf(unit, totals.lifetime, reading) : null;

  const run = useRun(keys.reload);

  return (
    <PopoverHost.Provider value={host}>
      <div className="stack" ref={pageRef}>
        <FilterBar
          address={address}
          count={keys.data === null ? '' : plural(rows.length, 'key')}
          dimensions={KEYS_DIMENSIONS}
          disabled={notLoadedReason('pools', 'pool', pools)}
          filters={filters}
          options={{ ...STATUS_OPTIONS, pools: poolOptions }}
          placeholder="Search keys"
          refresh={{
            intervalMs,
            onIntervalChange: setIntervalMs,
            onRefresh: () => {
              keys.reload();
              pools.reload();
              settings.reload();
            },
            paused: pauseCause,
            refreshTitle: 'Reads the list again now',
          }}
          update={update}
        >
          <SortControl keys={KEY_SORT_KEYS} onChange={update} value={filters} />
          <Button variant="primary" icon={<IconPlus size={14} />} onClick={() => setDialog({ initialGroup: null, key: null })}>
            Create key
          </Button>
        </FilterBar>
        <div className="stat-grid">
          <Tile label="Keys" value={totals ? fmtInt(totals.keys) : null} />
          <Tile label="Active" value={totals ? fmtInt(totals.active) : null} />
          <Tile label="Lifetime requests" value={totals ? fmtCompact(totals.requests) : null} />
          <Tile label="Lifetime cost" value={lifetimeTile?.value ?? null} title={lifetimeTile?.title} failure={lifetimeTile?.failure ?? null} />
        </div>

        <div className="keys-layout">
          <Card title="Lifetime cost by key" subtitle="Relative spend across all keys">
            {byRank ? (
              <ul className="plain-list hbars">
                {byRank.map((k) => {
                  const figure = keyFigureOf(unit, k.lifetime, 'No request yet', reading);
                  const rank = fundedRankOf(unit, k.lifetime);
                  return (
                    <li key={k.id}>
                      <div className="hbars__head">
                        <span>
                          {k.name} <span className="muted mono">{k.prefix}…</span>
                        </span>
                        <span className="num">
                          {figure === null ? <PendingFigure failure={reading.failure} /> : <span title={figure.title}>{figure.text}</span>} <span className="muted">{fmtCompact(k.lifetime.requests)} {noun(k.lifetime.requests, 'request')}</span>
                        </span>
                      </div>
                      <div className="hbars__track">
                        <span className="hbars__fill" style={{ width: `${maxRank > 0 && rank !== null ? (rank / maxRank) * 100 : 0}%`, background: ink.series[0] }} />
                      </div>
                    </li>
                  );
                })}
              </ul>
            ) : (
              <Skeleton height={120} />
            )}
          </Card>

          <Card padded={false} className="card--table">
            <div className="table-wrap">
              <table className="table">
                <thead>
                  <tr>
                    <th className="col-fill">Name</th>
                    <th className="col-fit">Prefix</th>
                    <th className="col-fit">Pool</th>
                    <th className="col-fill">Models</th>
                    <th className="right col-fit">Usage, 7d</th>
                    <th className="col-fit">Expiry</th>
                    <th className="col-fill">Status</th>
                    <th className="col-fit" />
                  </tr>
                </thead>
                <tbody>
                  {!keys.data
                    ? keys.error
                      ? (
                          <tr>
                            <td colSpan={8}>
                              <ErrorState title="Could not load the keys" body={keys.error} retry={keys.reload} />
                            </td>
                          </tr>
                        )
                      : <SkeletonRows count={3} colSpan={8} />
                    : ordered.map((k) => (
                        <tr key={k.id} id={k.id} tabIndex={-1} className="key-row">
                          <td className="col-fill" title={k.name}>
                            <b>{k.name}</b>
                            {/* A fill column cuts a line with the ellipsis, so each line carries its whole text on its title. */}
                            <span className="sub" title={captureLine(k, poolById.get(k.poolId) ?? null)}>
                              {captureLabel(effectiveCaptureHours(k, poolById.get(k.poolId) ?? null))}
                              {k.bodyCaptureHours === null && (
                                <span className="muted" title={CAPTURE_INHERIT_HINT}>
                                  {' '}
                                  inherited
                                </span>
                              )}
                            </span>
                            {k.keepaliveOptOut && <span className="sub" title="keepalive off">keepalive off</span>}
                            {/* No column draws the creation date or the week's requests, so the order's figure sits
                                under the name while picked. */}
                            {filters.sort === 'created' && (
                              <span className="sub" title={fmtAbsolute(k.createdAt)}>
                                created {fmtDate(k.createdAt)}
                              </span>
                            )}
                            {filters.sort === 'requests' && (
                              <span className="sub" title={weekRequests(k)}>
                                {weekRequests(k)}
                              </span>
                            )}
                          </td>
                          <td className="mono col-fit">{k.prefix}…</td>
                          <td className="mono col-fit">{k.poolId}</td>
                          <td className="col-fill">{k.modelRules.length ? <span className="mono" title={k.modelRules.map((r) => (r.not ? `NOT ${r.pattern}` : r.pattern)).join(', ')}>{plural(k.modelRules.length, 'rule')}{k.modelRulesMode === 'replace' && ' (replace)'}</span> : <span className="muted" title={NO_RULES[k.modelRulesMode]}>{NO_RULES[k.modelRulesMode]}</span>}</td>
                          <td className="right num col-fit">
                            {fmtCompact(k.usage7d.tokens)} tok
                            <WeekLine reading={reading} unit={unit} usage={k.usage7d} />
                          </td>
                          <td className="col-fit" title={k.expiresAt ? fmtAbsolute(k.expiresAt) : undefined}>{k.expiresAt ? fmtDate(k.expiresAt) : <span className="muted">Never</span>}</td>
                          <td className="col-fill">
                            <Badge tone={KEY_STATUS_TONE[k.status]} dot title={k.status} describe={false}>
                              <span className="key-row__badge-label ellipsis">{k.status}</span>
                            </Badge>
                            {k.lastUsedAt && <span className="sub" title={`used ${fmtRelative(k.lastUsedAt, now)}`}>used {fmtRelative(k.lastUsedAt, now)}</span>}
                            {/* The route answers 0 once the rotation grace has passed, so the page needs no clock of its own. */}
                            {k.sessionsOnRetiredSecret > 0 && (
                              <span className="sub" title={`${retiredNote(k.sessionsOnRetiredSecret)}. ${k.retiredSecretLastUsedAt ? `${RETIRED_SECRET_HINT} Previous secret last used ${fmtRelative(k.retiredSecretLastUsedAt, now)}.` : RETIRED_SECRET_HINT}`}>
                                {retiredNote(k.sessionsOnRetiredSecret)}
                              </span>
                            )}
                          </td>
                          <td className="right col-fit">
                            <Menu
                              items={[
                                { label: 'Edit', onClick: () => setDialog({ initialGroup: null, key: k }) },
                                {
                                  label: 'Rotate',
                                  onClick: () =>
                                    setConfirm({
                                      title: `Rotate ${k.name}`,
                                      body: 'A new secret is issued and shown once. The old secret keeps working for 15 minutes so running sessions can switch.',
                                      label: 'Rotate',
                                      onConfirm: () =>
                                        void run(async () => {
                                          setCreated({ key: await api.rotateKey(k.id), title: 'Key rotated' });
                                        }),
                                    }),
                                },
                                {
                                  label: 'Revoke',
                                  disabled: k.status !== 'active',
                                  onClick: () =>
                                    setConfirm({
                                      title: `Revoke ${k.name}`,
                                      body: 'Requests with this key are rejected immediately. Its history stays in the ledger.',
                                      label: 'Revoke',
                                      danger: true,
                                      onConfirm: () => void run(() => api.revokeKey(k.id), 'Key revoked'),
                                    }),
                                },
                                {
                                  label: 'Delete',
                                  danger: true,
                                  onClick: () =>
                                    setConfirm({
                                      title: `Delete ${k.name}`,
                                      body: 'Removes the key and detaches its request history from the key name. This cannot be undone.',
                                      label: 'Delete',
                                      danger: true,
                                      onConfirm: () => void run(() => api.deleteKey(k.id), 'Key deleted'),
                                    }),
                                },
                              ]}
                            />
                          </td>
                        </tr>
                      ))}
                  {keys.data?.length === 0 && (
                    // A filter that matches no key keeps the table bare. Only a registry with no key at all says so.
                    <tr>
                      <td colSpan={8}>
                        <EmptyState
                          title="No keys yet"
                          action={
                            <Button size="sm" onClick={() => setDialog({ initialGroup: null, key: null })}>
                              Create key
                            </Button>
                          }
                        />
                      </td>
                    </tr>
                  )}
                </tbody>
              </table>
            </div>
          </Card>
        </div>

        <KeyDialog
          open={dialog !== null}
          captureMaxHours={settings.data?.retention.bodyCaptureMaxHours}
          existing={dialog?.key ?? null}
          initialGroup={dialog?.initialGroup ?? null}
          onClose={() => setDialog(null)}
          onReload={() => {
            setDialog(null);
            keys.reload();
          }}
          pools={poolList}
          existingIds={keyIds}
          onCreated={(c) => {
            setDialog(null);
            setCreated({ key: c, title: 'Key created' });
            keys.reload();
          }}
          onUpdated={() => {
            setDialog(null);
            notify('Key saved');
            keys.reload();
          }}
        />
        <SecretDialog secret={created?.key.secret ?? null} title={created?.title ?? 'Key created'} onClose={() => setCreated(null)}>
          <p className="secondary">
            Set it as <code>CLAUDE_CODE_OAUTH_TOKEN</code> on the client. The Settings page has the full snippet.
          </p>
        </SecretDialog>
        <ConfirmDialog open={confirm !== null} onClose={() => setConfirm(null)} title={confirm?.title ?? ''} body={confirm?.body ?? ''} confirmLabel={confirm?.label} danger={confirm?.danger} onConfirm={() => confirm?.onConfirm()} />
      </div>
    </PopoverHost.Provider>
  );
}
