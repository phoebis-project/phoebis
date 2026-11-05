import { useEffect, useId, useMemo, useRef, useState, type ReactNode } from 'react';
import { useLocation } from 'react-router-dom';
import { api, ApiError } from '@/api';
import { slug } from '@/api/http';
import type { ApiKey, CreatedApiKey, Guardrail, GuardrailRef, KeyModels, KeyStatus, ModelRulesMode, Pool } from '@/api/types';
import { useSession } from '@/app/session';
import { useAppSettings } from '@/app/settings';
import { useAddressEdit } from '@/app/useAddressEdit';
import { useChartInk } from '@/components/charts/useChartInk';
import { FilterBar } from '@/components/FilterBar';
import { PatternEditor, useCatalogSuggestions } from '@/components/PatternEditor';
import { OptionList } from '@/components/ui/OptionList';
import { Popover, PopoverHost, useHoldHost } from '@/components/ui/Popover';
import { sumTileOf, Tile } from '@/components/ui/Tile';
import { IconChevron, IconInfo, IconPlus, IconWarning, IconX } from '@/components/ui/icons';
import { Badge, Button, Card, ConfirmDialog, CopyButton, Dialog, EmptyState, ErrorState, Expert, Field, GroupedDialog, Input, Menu, PageLink, Refusal, Segmented, Select, Skeleton, Toggle, type DialogGroup } from '@/components/ui/primitives';
import { EDIT_PARAM, EDIT_PARAMS, KEYS_CODEC } from '@/lib/address';
import { dollarsUnder, nativeSidesOf, sidesLine, sumFunded, UNPRICED_TITLE, type FundedSum, type WeekReading } from '@/lib/analytics';
import { anchorOf } from '@/lib/attention';
import { effectiveCaptureHours } from '@/lib/capture';
import { clock } from '@/lib/clock';
import { weekReadingOf } from '@/lib/costs';
import { KEYS_DIMENSIONS, notLoadedReason } from '@/lib/filterBar';
import type { PauseCause } from '@/lib/follow';
import { fmtAbsolute, fmtCompact, fmtDate, fmtInt, fmtRelative, fmtUsd, messageOf, noun, plural } from '@/lib/format';
import { guardrailsChange, KEY_GROUPS, keyFormOf, keyInputOf, keyRefusals, refusalsView, sameValues, saveStatus, type AutoModeOverride, type KeyForm, type KeyGroup } from '@/lib/forms';
import { MODE_TONE } from '@/lib/guardrails';
import { useAsync, useNow, useQueryFilters, type Loaded } from '@/lib/hooks';
import { scrollToElement } from '@/lib/scroll';
import type { UnitPosition } from '@/lib/unit';

export const KEY_STATUS_TONE: Record<KeyStatus, 'critical' | 'good' | 'neutral' | 'warning'> = { active: 'good', disabled: 'warning', expired: 'neutral', revoked: 'critical' };
const isKeyGroup = (value: string | null): value is KeyGroup => value !== null && (KEY_GROUPS as readonly string[]).includes(value);
const captureLabel = (hours: number) => (hours > 0 ? `bodies captured, ${hours}h` : 'bodies not captured');
/** The capture line whole, its inherited mark included, for the title of a line the fill column cuts. */
const captureLine = (key: Pick<ApiKey, 'bodyCaptureHours'>, pool: Pick<Pool, 'bodyCaptureHours'> | null) => `${captureLabel(effectiveCaptureHours(key, pool))}${key.bodyCaptureHours === null ? ' inherited' : ''}`;
const CAPTURE_INHERIT_HINT = 'This key sets no capture of its own, so it follows its pool.';
/** The Models summary of a key without rules of its own. */
const NO_RULES: Record<ModelRulesMode, string> = { extend: 'Pool rules', replace: 'No rules' };
const MODES: { label: string; value: ModelRulesMode }[] = [
  { label: 'Extend pool rules', value: 'extend' },
  { label: 'Replace pool rules', value: 'replace' },
];
const RETIRED_SECRET_HINT = 'Sessions whose newest request presented the previous secret. A session that registered a companion and sent no request since counts as switched.';
/** The note under the Status badge. The fill column cuts it, so its title opens with the whole of it. */
const retiredNote = (sessions: number) => `${plural(sessions, 'session')} still on the previous secret`;
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
/**
 * A key's bar length on the lifetime cost card: under native the Pro-week figure, which the chosen week scales alike for
 * every bar, where a billed key's dollars read in its label alone, and null where a plan row priced nothing, since the
 * priced rows alone would draw a length the key's traffic does not have. A dollar position ranks by its dollars.
 */
export const keyRankOf = (unit: UnitPosition, lifetime: FundedSum): number | null => {
  return unit === 'native' ? lifetime.equivalent : dollarsUnder(unit, lifetime);
};

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
  const settings = useAsync(() => api.getSettings(), []);
  const reading = weekReadingOf(settings.data?.subscriptionCosts ?? null, settings.error, week);
  const now = useNow();
  const ink = useChartInk();
  const location = useLocation();
  const pageRef = useRef<HTMLDivElement>(null);
  const landed = useRef<string | null>(null);
  // The dialog's one-shot `edit` and `group` wait in the address until the key list loads, so a filter written before
  // then carries them through.
  const { address, filters, replaceLater, update } = useQueryFilters(KEYS_CODEC, { keep: EDIT_PARAMS });
  const status = filters.status;
  const [dialog, setDialog] = useState<{ initialGroup: KeyGroup | null; key: ApiKey | null } | null>(null);
  // Held only while the dialog that shows it is open, because the router keeps no copy of a secret.
  const [created, setCreated] = useState<{ key: CreatedApiKey; title: string } | null>(null);
  const [confirm, setConfirm] = useState<{ body: string; danger?: boolean; label: string; onConfirm: () => void; title: string } | null>(null);
  const { notify } = useSession();
  const [intervalMs, setIntervalMs] = useState(5_000);
  // The key dialog resets its fields whenever the pool list changes, so a refresh landing under it would wipe them. A
  // row's open menu holds the page through its host, since an answer landing under it could add or drop rows beneath
  // it.
  const { cause: holdCause, host } = useHoldHost();
  const pauseCause: PauseCause | null = dialog !== null || created !== null || confirm !== null ? 'dialog' : holdCause;
  const paused = pauseCause !== null;
  const keys = useAsync(() => api.listKeys(), [], { paused, refreshMs: intervalMs });
  const pools = useAsync(() => api.listPools(), [], { paused, refreshMs: intervalMs });

  useAddressEdit(keys.data, 'Key', (key, group) => setDialog({ initialGroup: isKeyGroup(group) ? group : 'key', key }));

  const poolList = useMemo(() => pools.data ?? [], [pools.data]);
  const poolById = useMemo(() => new Map(poolList.map((p) => [p.id, p])), [poolList]);
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

  // `/keys#<key id>` lands on the row once the keys have drawn it, whenever the list arrives after the mount, and once
  // per arrival, since the page's own address rewrites keep the hash. A filter that would hide the row is cleared
  // first, and the next run finds the row drawn. The row is looked up inside this page, so an id such as `root` cannot
  // name the app's container.
  useEffect(() => {
    if (location.hash === landed.current) return;
    const id = anchorOf(location.hash);
    if (id === null) {
      landed.current = location.hash;
      return;
    }
    if (!keys.data?.some((k) => k.id === id)) return;
    if (!rows.some((k) => k.id === id)) {
      replaceLater({ ...filters, pools: [], search: '', status: 'all' });
      return;
    }
    const row = [...(pageRef.current?.querySelectorAll<HTMLElement>('tbody tr') ?? [])].find((el) => el.id === id);
    if (!row) return;
    landed.current = location.hash;
    scrollToElement(row);
    row.focus({ preventScroll: true });
  }, [filters, keys.data, location.hash, replaceLater, rows]);

  // The page re-renders on its two polls and the clock, so the sums and the lifetime card's order follow the keys and
  // the unit alone.
  const { byRank, maxRank, totals } = useMemo(() => {
    const data = keys.data;
    const rank = (k: ApiKey) => keyRankOf(unit, k.lifetime);
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

  /** Runs one key mutation and reloads the table behind it; a rejection only toasts, leaving the list alone. */
  const run = async (action: () => Promise<void>) => {
    try {
      await action();
    } catch (e) {
      notify(messageOf(e));
      return;
    }
    keys.reload();
  };

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
                  const rank = keyRankOf(unit, k.lifetime);
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
                      : [0, 1, 2].map((i) => (
                          <tr key={i} className="table__detail">
                            <td colSpan={8}>
                              <Skeleton />
                            </td>
                          </tr>
                        ))
                    : rows.map((k) => (
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
                                      onConfirm: () =>
                                        void run(async () => {
                                          await api.revokeKey(k.id);
                                          notify('Key revoked');
                                        }),
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
                                      onConfirm: () =>
                                        void run(async () => {
                                          await api.deleteKey(k.id);
                                          notify('Key deleted');
                                        }),
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
          existingIds={new Set((keys.data ?? []).map((k) => k.id))}
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

/** A new or rotated key's secret, shown this once, since the router stores only its hash. */
export function SecretDialog({ children, onClose, secret, title }: { children?: ReactNode; onClose: () => void; secret: string | null; title: string }) {
  return (
    <Dialog
      open={secret !== null}
      onClose={onClose}
      title={title}
      subtitle="Copy the secret now. The router stores only a hash, so it cannot be shown again."
      footer={
        <Button variant="primary" onClick={onClose}>
          Done
        </Button>
      }
    >
      {secret !== null && (
        <>
          <div className="secret">
            <code>{secret}</code>
            <CopyButton text={secret} size="md" />
          </div>
          {children}
        </>
      )}
    </Dialog>
  );
}

/** What the key dialog reads of a pool: its auto mode and capture defaults, the rules and guardrails a key inherits, and whose catalogs to suggest from. */
export type KeyDialogPool = Pick<Pool, 'autoModeServer' | 'bodyCaptureHours' | 'guardrails' | 'id' | 'modelRules' | 'name'> & { members: Pick<Pool['members'][number], 'accountId'>[] };

const KEY_GROUP_LABEL: Record<KeyGroup, string> = { access: 'Access', capture: 'Capture', guardrails: 'Guardrails', key: 'Key', models: 'Models', sessions: 'Sessions' };
const KEY_GUARDRAILS_HINT = "The pool's guardrails run first and cannot be dropped here. This key's own run after them.";

/** A row of a guardrail list: the route's ref, or the registry's row for one attached since the dialog opened. */
type ListedGuardrail = Pick<GuardrailRef, 'enabled' | 'id' | 'name'> & { mode: GuardrailRef['mode'] | null };

/**
 * A row's name, the view's word for a switched-off guardrail, and its mode badge in the Guardrails view's tones,
 * drawn last so it ends at the trailing slot in every row.
 */
function GuardrailWords({ guardrail: g }: { guardrail: ListedGuardrail }) {
  return (
    <>
      <span className="pattern-editor__pattern">{g.name}</span>
      {!g.enabled && <span className="muted">off</span>}
      {g.mode !== null && <Badge tone={MODE_TONE[g.mode]}>{g.mode}</Badge>}
    </>
  );
}

/**
 * A pool's or a key's guardrails in the order they run, each with move up, move down and remove, and "Attach guardrail"
 * offering the registry's guardrails that `inherited` does not hold, since one on both runs once at its pool position,
 * the list's own ones among them and checked. `inherited` draws a key's pool's rows first, read-only. `refs` are the
 * record's own rows as the route reported them, which name a row before the registry answers. `pending` is the sentence
 * a subject not saved yet reads in place of its own list and the control, since the create routes take no attachment.
 */
export function GuardrailList({ inherited = [], onChange, pending, refs, registry, value }: { inherited?: GuardrailRef[]; onChange: (ids: string[]) => void; pending?: string; refs: GuardrailRef[]; registry: Loaded<Guardrail[] | null>; value: string[] }): JSX.Element {
  const boxId = useId();
  const [anchor, setAnchor] = useState<HTMLButtonElement | null>(null);
  const [open, setOpen] = useState(false);
  const fromPool = [...inherited].sort((a, b) => a.position - b.position);
  const own = pending === undefined ? value : [];
  const listed = (id: string): ListedGuardrail => refs.find((r) => r.id === id) ?? registry.data?.find((g) => g.id === id) ?? { enabled: true, id, mode: null, name: id };
  const options = (registry.data ?? []).filter((g) => !fromPool.some((r) => r.id === g.id)).map((g) => ({ label: g.name, value: g.id }));
  const held = registry.data === null ? (registry.error ? `The guardrail list did not load: ${registry.error}` : 'Reading the guardrail list') : options.length === 0 ? 'No other guardrail is left to attach' : undefined;
  const move = (i: number, j: number) => {
    const next = [...value];
    [next[i], next[j]] = [next[j], next[i]];
    onChange(next);
  };
  // A pick attaches at once and lands at the end of the order, where the guardrail dialog's attach lands too, and an
  // uncheck removes its guardrail, so the box stays open while the operator picks.
  const pick = (next: string[]) => {
    const kept = value.filter((id) => next.includes(id));
    const added = options.map((o) => o.value).filter((id) => next.includes(id) && !value.includes(id));
    onChange([...kept, ...added]);
  };
  // The Popover hands the focus back to the anchor on Escape itself, and a focus move here would also pull the focus
  // back when it leaves the box for another control.
  const close = () => setOpen(false);
  return (
    <div className="pattern-editor guardrail-list" role="group" aria-label="Guardrails">
      {fromPool.length + own.length > 0 && (
        <ol className="plain-list pattern-editor__rows">
          {fromPool.map((r) => (
            <li key={`inherited-${r.id}`} className="pattern-editor__row pattern-editor__row--inherited">
              <GuardrailWords guardrail={r} />
              <span className="pattern-editor__source guardrail-row__trailing">inherited</span>
            </li>
          ))}
          {own.map((id, i) => {
            const g = listed(id);
            return (
              <li key={id} className="pattern-editor__row">
                <div className="pattern-editor__line">
                  <GuardrailWords guardrail={g} />
                  <span className="pattern-editor__actions guardrail-row__trailing">
                    <Button size="sm" variant="ghost" className="pattern-editor__up" icon={<IconChevron size={14} />} aria-label={`Move ${g.name} up`} disabled={i === 0} onClick={() => move(i, i - 1)} />
                    <Button size="sm" variant="ghost" icon={<IconChevron size={14} />} aria-label={`Move ${g.name} down`} disabled={i === own.length - 1} onClick={() => move(i, i + 1)} />
                    <Button size="sm" variant="ghost" icon={<IconX size={14} />} aria-label={`Remove ${g.name}`} onClick={() => onChange(value.filter((x) => x !== id))} />
                  </span>
                </div>
              </li>
            );
          })}
        </ol>
      )}
      {pending !== undefined ? (
        <p className="muted">{pending}</p>
      ) : (
        <>
          {fromPool.length + own.length === 0 && <p className="muted">No guardrails attached.</p>}
          <div>
            <Button ref={setAnchor} size="sm" icon={<IconPlus size={13} />} aria-haspopup="dialog" aria-expanded={open} disabled={held !== undefined} title={held} onClick={() => setOpen((o) => !o)}>
              Attach guardrail
            </Button>
          </div>
          {/* Each pick adds an own row above the anchor and each uncheck removes one, so the box is placed again. */}
          <Popover anchor={anchor} id={boxId} label="Attach guardrail" onClose={close} open={open} placeKey={value.length}>
            <OptionList autoFocus label="Guardrails" onChange={pick} options={options} searchable value={value} />
          </Popover>
        </>
      )}
    </div>
  );
}

/**
 * Create or edit a key. `existing` switches the dialog to edit mode;
 * `initialPoolId` with `lockPool` pins the pool when the dialog is opened from
 * inside a pool's own dialog. `existingIds` are the listed keys' ids, which a new
 * key's derived id is checked against before the create.
 */
export function KeyDialog({ open, onClose, onCreated, onReload, onUpdated, pools, captureMaxHours, existing = null, existingIds, initialGroup = null, initialPoolId, lockPool = false }: { /** The bootstrap's body capture limit from the host page's settings read, undefined while that read is out. */ captureMaxHours?: number; existing?: ApiKey | null; existingIds: ReadonlySet<string>; initialGroup?: KeyGroup | null; initialPoolId?: string; lockPool?: boolean; onClose: () => void; onCreated: (c: CreatedApiKey) => void; /** The stale save's Reload: closes the dialog and reads the keys again. */ onReload?: () => void; onUpdated?: (k: ApiKey) => void; open: boolean; pools: KeyDialogPool[] }) {
  const [chosen, setChosen] = useState<KeyGroup>('key');
  const [form, setForm] = useState<KeyForm>(() => keyFormOf(null, ''));
  const set = <K extends keyof KeyForm>(key: K, value: KeyForm[K]) => setForm((f) => ({ ...f, [key]: value }));
  const { autoModeServer, captureHours, captureMode, enabled, expiry, guardrails, longContextTwins, modelRules, modelRulesMode, name, notes, optOut, poolId } = form;
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [stale, setStale] = useState<string | null>(null);
  const pool = pools.find((p) => p.id === poolId) ?? null;
  const poolName = pool?.name ?? poolId;
  const poolHours = pool?.bodyCaptureHours ?? 0;
  const effectiveHours = captureMode === 'inherit' ? poolHours : captureMode === 'off' ? 0 : Number(captureHours);
  const suggestions = useCatalogSuggestions(pool?.members.map((m) => m.accountId) ?? [], open);
  const refusals = keyRefusals({ captureHours, captureMode, modelRules, name, poolId }, { identity: { existingIds, isNew: existing === null }, maxHours: captureMaxHours });
  const { first, invalid, refusalOf, refusedGroups } = refusalsView(refusals);
  const rulesRefusal = refusalOf('Model rules');
  const processed = useAsync(() => (open && existing ? api.getKeyModels(existing.id) : Promise.resolve(null)), [open, existing?.id]);
  // Once per opening of a saved key, since a create takes no attachment and offers nothing to attach.
  const registry = useAsync(() => (open && existing ? api.listGuardrails() : Promise.resolve(null)), [open, existing?.id]);
  // The values the dialog opened with, which the discard question compares the fields against.
  const [opened, setOpened] = useState(() => keyFormOf(null, ''));

  // Once per opening, in the render that brings it: the address can open the dialog before the pool list lands, and a
  // reset on its arrival would discard every edit made meanwhile and put back the group the dialog opened on, the
  // focus left in a hidden one.
  const [resetFor, setResetFor] = useState<{ existing: ApiKey | null; initialGroup: KeyGroup | null; initialPoolId: string | undefined } | null>(null);
  const resetDue = open && (resetFor === null || resetFor.existing !== existing || resetFor.initialGroup !== initialGroup || resetFor.initialPoolId !== initialPoolId);
  if (!open && resetFor !== null) setResetFor(null);
  if (resetDue) {
    // A key cannot exist without a pool, so the first pool stands in until one is picked.
    const next = keyFormOf(existing, initialPoolId ?? pools[0]?.id ?? '');
    setResetFor({ existing, initialGroup, initialPoolId });
    setOpened(next);
    setForm(next);
    setChosen(initialGroup ?? 'key');
    setBusy(false);
    setError(null);
    setStale(null);
  }
  // A new key opened before the pools landed has no pool yet, so the first pool fills it once they do, after the
  // render that resets. The fill is no edit of the operator's, so the opening values take it too where they hold no
  // pool yet.
  if (open && !resetDue && existing === null && poolId === '' && pools.length > 0) {
    set('poolId', pools[0].id);
    if (opened.poolId === '') setOpened({ ...opened, poolId: pools[0].id });
  }

  const moving = existing !== null && poolId !== existing.poolId;
  // The server's words leave with the next edit to any field. A failed save changes none, so they stand until then. The
  // compare runs field by field, so a field written again unchanged, as the override's focus writes its mode, keeps them.
  const [edited, setEdited] = useState(form);
  if ((Object.keys(form) as (keyof KeyForm)[]).some((field) => form[field] !== edited[field])) {
    setEdited(form);
    setError(null);
  }

  const submit = async () => {
    setBusy(true);
    setError(null);
    setStale(null);
    const input = keyInputOf(form, clock.now());
    try {
      if (existing) {
        const updated = await api.updateKey(existing.id, { ...input, ...guardrailsChange(guardrails, opened.guardrails) }, existing.revision);
        setBusy(false);
        onUpdated?.(updated);
        return;
      }
      const created = await api.createKey({ ...input, expiresInDays: expiry === 'never' ? null : Number(expiry) });
      setBusy(false);
      onCreated(created);
    } catch (e) {
      // Stated in the dialog rather than a toast, so the form the operator filled in is still on screen beside it.
      setBusy(false);
      if (e instanceof ApiError && e.status === 409 && e.code === 'stale_edit') {
        // The row moved under the dialog: the edit is about values that are gone, so the operator reloads and
        // re-applies it rather than saving over what someone else wrote.
        setStale(e.message);
        return;
      }
      setError(messageOf(e));
    }
  };

  // Keyed by group, so a group without a body fails to compile; `KEY_GROUPS` sets the rail order.
  const bodies: Record<KeyGroup, ReactNode> = {
    key: (
      <>
        {moving && (
          <div className="callout callout--warning" role="status">
            <IconInfo size={16} />
            <div>Moving this key ends its current session pins.</div>
          </div>
        )}
        <div className="cells">
          <Field label="Name" className="cell--half" hint={!existing && name.trim() ? `Id: ${slug(name.trim())}` : undefined}>
            <Input value={name} onChange={(e) => set('name', e.target.value)} placeholder="workstation" autoFocus aria-invalid={invalid('Name')} />
          </Field>
          <Field
            label="Pool"
            className="cell--half"
            hint={
              lockPool ? (
                `Created in pool ${poolName}`
              ) : (
                <>
                  A key reaches only its pool&apos;s members, and moving it ends its session pins. Members and rules are on the <PageLink to="/pools">Pools page</PageLink>.
                </>
              )
            }
          >
            <Select value={poolId} onChange={(e) => set('poolId', e.target.value)} disabled={lockPool} aria-invalid={invalid('Pool')}>
              {pools.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </Select>
          </Field>
          <Field label="Notes">
            <textarea className="input" value={notes} onChange={(e) => set('notes', e.target.value)} placeholder="Where this key lives and who holds it" />
          </Field>
        </div>
      </>
    ),
    access: (
      <div className="cells">
        {existing && (
          <Field label="Enabled" beside hint="Off refuses the key and keeps it, so it can come back on without a new secret. A revoked key cannot.">
            {/* The route refuses to re-enable a revoked key, so its switch says so rather than offering it. */}
            <span title={existing.status === 'revoked' ? 'a revoked key cannot be re-enabled' : undefined}>
              <Toggle checked={enabled} onChange={(v) => set('enabled', v)} label="Enabled" disabled={existing.status === 'revoked'} />
            </span>
          </Field>
        )}
        <Field label="Expiry" hint="An expired key is refused, but its history stays in the ledger.">
          <Select value={expiry} onChange={(e) => set('expiry', e.target.value)} aria-label="Expiry">
            {existing && <option value="keep">Keep the current expiry</option>}
            {!existing && <option value="never">No expiration</option>}
            <option value="7">7 days</option>
            <option value="30">30 days</option>
            <option value="60">60 days</option>
            <option value="90">90 days</option>
            <option value="365">1 year</option>
            {existing && <option value="never">No expiration</option>}
          </Select>
        </Field>
        <Expert>
          <Field label="Server-side auto mode" hint="Inherit follows the pool. Off drops the client's server-side tool-use checks on this key, so the client reviews with classifier requests of its own. On keeps them whatever the pool says.">
            <Select value={autoModeServer} onChange={(e) => set('autoModeServer', e.target.value as AutoModeOverride)} aria-label="Server-side auto mode">
              <option value="inherit">Inherit ({pool?.autoModeServer === false ? 'off' : 'on'}, the pool&apos;s setting)</option>
              <option value="on">On</option>
              <option value="off">Off</option>
            </Select>
          </Field>
        </Expert>
      </div>
    ),
    models: (
      <div className="cells">
        {/* Field's frame without its label element: a label forwards a click on its text to the first control
            inside, which here would press Add rule or flip the first row. */}
        <Field
          label="Model rules"
          plain
          hint="Top to bottom, first match wins. The saved key's Effective list shows the list it ends up with."
        >
          <PatternEditor label="Model rules" value={modelRules} onChange={(v) => set('modelRules', v)} inherited={modelRulesMode === 'extend' ? (pool?.modelRules ?? []) : []} suggestions={suggestions} />
        </Field>
        {rulesRefusal && <Refusal>{rulesRefusal}</Refusal>}
        <Expert>
          {/* Field's frame without its label element: a label forwards a click on its text to the first control
              inside, which here would flip the mode. */}
          <Field label="Pool rules" plain hint="Extend applies the pool's rules, in gray, after this key's own. Replace applies this key's rules alone.">
            {/* Its own block, so the segmented control keeps its content width. */}
            <div>
              <Segmented<ModelRulesMode> ariaLabel="How this key's rules meet the pool's" value={modelRulesMode} options={MODES} onChange={(v) => set('modelRulesMode', v)} />
            </div>
          </Field>
        </Expert>
        <Expert>
          <Field label="List [1m] twins in the models listing" beside hint="Claude Code sizes its context window by the id, so the twin lets it use the long-context window the upstream reports.">
            <Toggle checked={longContextTwins} onChange={(v) => set('longContextTwins', v)} label="List [1m] twins in the models listing" />
          </Field>
        </Expert>
        {existing && <EffectiveList processed={processed} />}
      </div>
    ),
    guardrails: (
      <div className="cells">
        {/* Field's frame without its label element: a label forwards a click on its text to the first button inside. */}
        <Field
          label="Guardrails"
          plain
          hint={
            <>
              {KEY_GUARDRAILS_HINT} <PageLink to="/guardrails">Manage guardrails</PageLink>
            </>
          }
        >
          {/* The chosen pool's list, so a key moved in this dialog shows the pool it lands in, and the key's own read of
              its pool's list until the pool list arrives. */}
          <GuardrailList inherited={pool?.guardrails ?? (existing?.poolId === poolId ? existing.poolGuardrails : [])} onChange={(v) => set('guardrails', v)} pending={existing ? undefined : 'Guardrails can be attached once the key is saved.'} refs={existing?.guardrails ?? []} registry={registry} value={guardrails} />
        </Field>
      </div>
    ),
    sessions: (
      <div className="cells">
        <Field
          label="Cache warming opt-out"
          beside
          hint={
            <>
              Sessions on this key never get cache-warming probes, even when the pool keeps caches warm. The pool&apos;s Keepalive is on the <PageLink to="/pools">Pools page</PageLink>.
            </>
          }
        >
          <Toggle checked={optOut} onChange={(v) => set('optOut', v)} label="Cache warming opt-out" />
        </Field>
      </div>
    ),
    capture: (
      <>
        {effectiveHours > 0 && (
          <div className="callout callout--warning">
            <IconWarning size={16} />
            <div>Bodies contain the prompts, file contents, and tool results of every session using this key, and a session in auto mode sends the machine&apos;s permission rules with their literal text, its working and home directories and its git remotes with every request. Only the admin secret can read them.</div>
          </div>
        )}
        <div className="cells">
          {/* Field's frame without its label element, since each choice is a label of its own. */}
          <Field
            label="Capture request and response bodies"
            plain
            hint={
              <>
                Stores the full JSON of each exchange for the request drawer on the <PageLink to="/requests">Requests page</PageLink>. Inherit follows the pool, and any value is capped by the bootstrap&apos;s limit.
              </>
            }
          >
            <div className="radio-list" role="radiogroup" aria-label="Capture request and response bodies">
              <label className={`radio-list__item ${captureMode === 'inherit' ? 'is-active' : ''}`}>
                <input type="radio" name={`capture-${existing?.id ?? 'new'}`} checked={captureMode === 'inherit'} onChange={() => set('captureMode', 'inherit')} />
                <span>Inherit from pool ({poolHours > 0 ? `${poolHours} h` : 'off'})</span>
              </label>
              <label className={`radio-list__item ${captureMode === 'override' ? 'is-active' : ''}`}>
                <input type="radio" name={`capture-${existing?.id ?? 'new'}`} checked={captureMode === 'override'} onChange={() => set('captureMode', 'override')} />
                <span className="row">
                  Override:
                  <Input
                    type="number"
                    min={1}
                    max={captureMaxHours}
                    value={captureHours}
                    onChange={(e) => set('captureHours', e.target.value)}
                    onFocus={() => set('captureMode', 'override')}
                    style={{ flex: 1 }}
                    aria-label="Capture hours for this key"
                    aria-invalid={invalid('Capture hours for this key')}
                  />
                  hours
                </span>
              </label>
              <label className={`radio-list__item ${captureMode === 'off' ? 'is-active' : ''}`}>
                <input type="radio" name={`capture-${existing?.id ?? 'new'}`} checked={captureMode === 'off'} onChange={() => set('captureMode', 'off')} />
                <span>Off for this key</span>
              </label>
            </div>
          </Field>
        </div>
      </>
    ),
  };
  const groups: DialogGroup[] = KEY_GROUPS.map((id) => ({ body: bodies[id], id, label: KEY_GROUP_LABEL[id], refused: refusedGroups.has(id) }));
  const save = saveStatus({ error, first, stale, subject: existing ? `the key ${existing.name}` : null });

  return (
    <GroupedDialog
      open={open}
      onClose={onClose}
      title={existing ? `Edit ${existing.name}` : 'Create key'}
      unsaved={!sameValues(form, opened)}
      railLabel="Key settings"
      groups={groups}
      chosen={chosen}
      initialGroup={initialGroup}
      onChoose={(id) => setChosen(id as KeyGroup)}
      status={save.status}
      hold={save.hold}
      held={save.held}
      notice={
        stale && (
          <Refusal>
            {stale}{' '}
            <Button variant="ghost" onClick={onReload}>
              Reload
            </Button>
          </Refusal>
        )
      }
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" disabled={busy || refusals.length > 0} onClick={submit}>
            {busy ? (existing ? 'Saving…' : 'Creating…') : existing ? 'Save' : 'Create'}
          </Button>
        </>
      }
    />
  );
}

/** The list the saved key is judged by, from the route rather than the form, so an unsaved edit does not show here. */
function EffectiveList({ processed }: { processed: Loaded<KeyModels | null> }) {
  const rules = processed.current?.rules;
  return (
    <div className="field">
      <span className="field__label">Effective list</span>
      <p className="field__hint">What the saved key is judged by, top to bottom, with the level each entry comes from.</p>
      {processed.error ? (
        <p className="muted">Could not load the effective list: {processed.error}</p>
      ) : !rules ? (
        <Skeleton height={28} />
      ) : rules.length === 0 ? (
        <p className="muted">No rules: every model the pool's accounts serve is allowed.</p>
      ) : (
        <ol className="plain-list effective-rules">
          {rules.map((r, i) => {
            const cooling = new Set(r.cooldowns.map((c) => c.accountId)).size;
            return (
              <li key={i}>
                <Badge tone={r.level === 'key' ? 'accent' : 'neutral'}>{r.level}</Badge>
                <span className="effective-rules__effect">{r.not ? 'Deny' : 'Allow'}</span>
                <span className="mono">{r.pattern}</span>
                {cooling > 0 && (
                  <Badge tone="warning" title={r.cooldowns.map((c) => `${c.accountId}: ${c.scope.replace(/^model:/, '')}`).join(', ')}>
                    {plural(cooling, 'account')} cooling
                  </Badge>
                )}
              </li>
            );
          })}
        </ol>
      )}
    </div>
  );
}
