import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';
import { api, ApiError } from '@/api';
import { slug } from '@/api/http';
import type { ApiKey, CreatedApiKey, KeyModels, KeyStatus, ModelRule, ModelRulesMode, Pool } from '@/api/types';
import { useSession } from '@/app/session';
import { useChartInk } from '@/components/charts/useChartInk';
import { PatternEditor, useCatalogSuggestions } from '@/components/PatternEditor';
import { Tile } from '@/components/ui/Tile';
import { IconInfo, IconPlus, IconWarning } from '@/components/ui/icons';
import { Badge, Button, Card, ConfirmDialog, CopyButton, Dialog, EmptyState, ErrorState, Expert, Field, GroupedDialog, Input, Menu, PageLink, SearchInput, Segmented, Select, Skeleton, Toggle, type DialogGroup } from '@/components/ui/primitives';
import { KEYS_CODEC, type KeysAddress } from '@/lib/address';
import { anchorOf } from '@/lib/attention';
import { clock } from '@/lib/clock';
import { fmtAbsolute, fmtCompact, fmtDate, fmtInt, fmtRelative, fmtUsd } from '@/lib/format';
import { KEY_GROUPS, keyFormOf, keyInputOf, keyRefusals, sortKeys, type AutoModeOverride, type CaptureMode, type KeyGroup } from '@/lib/forms';
import { useAsync, useCommittedText, useNow, useQueryFilters, type Loaded } from '@/lib/hooks';
import { scrollToElement } from '@/lib/scroll';

const STATUS_TONE: Record<KeyStatus, 'critical' | 'good' | 'neutral' | 'warning'> = { active: 'good', disabled: 'warning', expired: 'neutral', revoked: 'critical' };
const isKeyGroup = (value: string | null): value is KeyGroup => value !== null && (KEY_GROUPS as readonly string[]).includes(value);
/** A key with no override of its own captures for as long as its pool says. */
export const effectiveCaptureHours = (key: Pick<ApiKey, 'bodyCaptureHours'>, pool: Pick<Pool, 'bodyCaptureHours'> | null): number => key.bodyCaptureHours ?? pool?.bodyCaptureHours ?? 0;
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
const retiredNote = (sessions: number) => `${sessions} ${sessions === 1 ? 'session' : 'sessions'} still on the previous secret`;

export function KeysPage() {
  const now = useNow();
  const ink = useChartInk();
  const location = useLocation();
  const navigate = useNavigate();
  const pageRef = useRef<HTMLDivElement>(null);
  const landed = useRef<string | null>(null);
  const { filters } = useQueryFilters(KEYS_CODEC);
  const status = filters.status as 'all' | KeyStatus;
  // The dialog's one-shot `edit` and `group` wait in the address until the key list loads, so a filter written before
  // then carries them through. Written at once, as the hook writes.
  const update = (patch: Partial<KeysAddress>) => {
    const params = KEYS_CODEC.toParams({ ...filters, ...patch });
    const current = new URLSearchParams(location.search);
    for (const name of ['edit', 'group']) {
      const value = current.get(name);
      if (value !== null) params.set(name, value);
    }
    const next = params.toString();
    navigate({ hash: location.hash, pathname: location.pathname, search: next ? `?${next}` : '' }, { flushSync: true, replace: true });
  };
  const search = useCommittedText(filters.search, (next) => update({ search: next }));
  // An effect's write: `update` flushes at once, which React cannot do inside an effect, so these replace the address
  // the router's usual way.
  const replaceFromEffect = (patch: Partial<KeysAddress>) => {
    const next = KEYS_CODEC.toParams({ ...filters, ...patch }).toString();
    navigate({ hash: location.hash, pathname: location.pathname, search: next ? `?${next}` : '' }, { replace: true });
  };
  const [dialog, setDialog] = useState<{ initialGroup: KeyGroup | null; key: ApiKey | null } | null>(null);
  // Held only while the dialog that shows it is open, because the router keeps no copy of a secret.
  const [created, setCreated] = useState<{ key: CreatedApiKey; title: string } | null>(null);
  const [confirm, setConfirm] = useState<{ body: string; danger?: boolean; label: string; onConfirm: () => void; title: string } | null>(null);
  const { notify } = useSession();
  // The rows whose menu is open, by key id: an answer landing under an open menu could add or drop rows beneath it.
  const [openMenus, setOpenMenus] = useState<ReadonlySet<string>>(() => new Set());
  // The key dialog resets its fields whenever the pool list changes, so a refresh landing under it would wipe them.
  const paused = dialog !== null || created !== null || confirm !== null || openMenus.size > 0;
  const keys = useAsync(() => api.listKeys(), [], { paused, refreshMs: 30_000 });
  const pools = useAsync(() => api.listPools(), [], { paused, refreshMs: 30_000 });

  // The address opens the dialog on a named group once the key list has loaded; the entry is then replaced by the
  // bare route, so closing the dialog, a reload or Back does not open it again.
  useEffect(() => {
    const params = new URLSearchParams(location.search);
    const edit = params.get('edit');
    if (edit === null || !keys.data) return;
    const group = params.get('group');
    params.delete('edit');
    params.delete('group');
    const search = params.toString();
    navigate({ hash: location.hash, pathname: location.pathname, search: search ? `?${search}` : '' }, { replace: true });
    const key = keys.data.find((k) => k.id === edit);
    if (!key) {
      notify(`Key ${edit} not found`);
      return;
    }
    setDialog({ initialGroup: isKeyGroup(group) ? group : 'key', key });
  }, [keys.data, location, navigate, notify]);

  const poolList = useMemo(() => pools.data ?? [], [pools.data]);
  const poolById = useMemo(() => new Map(poolList.map((p) => [p.id, p])), [poolList]);
  // The pool filter is the address: the select reads `?pools=<id>` and writes it back, so a reload keeps the filter and
  // the pool card's key count link lands on it. Until the pools are read the address filters as written, so the rows
  // match it during the read and after a failed one, and the select holds an option for the id: without one it would
  // read All pools beside the filtered rows, and choosing All pools would change nothing. An id no pool carries reads
  // as All pools and leaves the address once the pools have loaded; while a dialog query is still in the address, its
  // own effect goes first. All pools is the empty string, which no pool id can be, since a pool may be named `all`.
  const poolIds = useMemo(() => filters.pools.filter((id) => pools.data === null || poolById.has(id)), [filters.pools, poolById, pools.data]);
  const poolFilter = poolIds[0] ?? '';
  useEffect(() => {
    if (filters.pools.length === 0 || !pools.data || poolIds.length > 0) return;
    if (new URLSearchParams(location.search).has('edit')) return;
    replaceFromEffect({ pools: [] });
    // The writer changes on every render, and the address it writes from reruns the effect.
  }, [filters.pools, location.search, poolIds, pools.data]);
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
      replaceFromEffect({ pools: [], search: '', status: 'all' });
      return;
    }
    const row = [...(pageRef.current?.querySelectorAll<HTMLElement>('tbody tr') ?? [])].find((el) => el.id === id);
    if (!row) return;
    landed.current = location.hash;
    scrollToElement(row);
    row.focus({ preventScroll: true });
    // The writer changes on every render, and the rows follow every filter it clears, so they rerun the effect.
  }, [keys.data, location.hash, rows]);

  const totals = keys.data
    ? {
        keys: keys.data.length,
        active: keys.data.filter((k) => k.status === 'active').length,
        requests: keys.data.reduce((s, k) => s + k.lifetime.requests, 0),
        cost: keys.data.reduce((s, k) => s + k.lifetime.cost, 0),
      }
    : null;
  const maxCost = Math.max(1, ...(keys.data ?? []).map((k) => k.lifetime.cost));

  /** Runs one key mutation and reloads the table behind it; a rejection only toasts, leaving the list alone. */
  const run = async (action: () => Promise<void>) => {
    try {
      await action();
    } catch (e) {
      notify(e instanceof Error ? e.message : String(e));
      return;
    }
    keys.reload();
  };

  return (
    <div className="stack" ref={pageRef}>
      <div className="stat-grid">
        <Tile label="Keys" value={totals ? fmtInt(totals.keys) : null} />
        <Tile label="Active" value={totals ? fmtInt(totals.active) : null} />
        <Tile label="Lifetime requests" value={totals ? fmtCompact(totals.requests) : null} />
        <Tile label="Lifetime cost" value={totals ? fmtUsd(totals.cost) : null} />
      </div>

      <div className="keys-layout">
        <Card title="Lifetime cost by key" subtitle="Relative spend across all keys">
          {keys.data ? (
            <ul className="hbars">
              {[...keys.data]
                .sort((a, b) => b.lifetime.cost - a.lifetime.cost)
                .map((k) => (
                  <li key={k.id}>
                    <div className="hbars__head">
                      <span>
                        {k.name} <span className="muted mono">{k.prefix}…</span>
                      </span>
                      <span className="num">
                        {fmtUsd(k.lifetime.cost)} <span className="muted">{fmtCompact(k.lifetime.requests)} {k.lifetime.requests === 1 ? 'request' : 'requests'}</span>
                      </span>
                    </div>
                    <div className="hbars__track">
                      <span className="hbars__fill" style={{ width: `${(k.lifetime.cost / maxCost) * 100}%`, background: ink.series[0] }} />
                    </div>
                  </li>
                ))}
            </ul>
          ) : (
            <Skeleton height={120} />
          )}
        </Card>

        <Card padded={false} className="card--table">
          <div className="table-toolbar">
            <SearchInput placeholder="Search keys" value={search.draft} onChange={(e) => search.onChange(e.target.value)} onBlur={search.onBlur} onKeyDown={search.onKeyDown} />
            <Select value={status} onChange={(e) => update({ status: e.target.value })} aria-label="Status">
              <option value="all">All statuses</option>
              <option value="active">Active</option>
              <option value="disabled">Disabled</option>
              <option value="expired">Expired</option>
              <option value="revoked">Revoked</option>
            </Select>
            <Select value={poolFilter} onChange={(e) => update({ pools: e.target.value ? [e.target.value] : [] })} aria-label="Pool">
              <option value="">All pools</option>
              {pools.data === null && poolFilter !== '' && <option value={poolFilter}>{poolFilter}</option>}
              {poolList.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </Select>
            <Button variant="primary" icon={<IconPlus size={14} />} onClick={() => setDialog({ initialGroup: null, key: null })} style={{ marginLeft: 'auto' }}>
              Create key
            </Button>
          </div>
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
                            <ErrorState title="Could not load the keys" body={keys.error} action={<Button size="sm" onClick={keys.reload}>Try again</Button>} />
                          </td>
                        </tr>
                      )
                    : [0, 1, 2].map((i) => (
                        <tr key={i}>
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
                        <td className="col-fill">{k.modelRules.length ? <span className="mono" title={k.modelRules.map((r) => (r.not ? `NOT ${r.pattern}` : r.pattern)).join(', ')}>{k.modelRules.length === 1 ? '1 rule' : `${k.modelRules.length} rules`}{k.modelRulesMode === 'replace' && ' (replace)'}</span> : <span className="muted" title={NO_RULES[k.modelRulesMode]}>{NO_RULES[k.modelRulesMode]}</span>}</td>
                        <td className="right num col-fit">
                          {fmtCompact(k.usage7d.tokens)} tok
                          <span className="sub">{fmtUsd(k.usage7d.cost)}</span>
                        </td>
                        <td className="col-fit" title={k.expiresAt ? fmtAbsolute(k.expiresAt) : undefined}>{k.expiresAt ? fmtDate(k.expiresAt) : <span className="muted">Never</span>}</td>
                        <td className="col-fill">
                          <Badge tone={STATUS_TONE[k.status]} dot title={k.status} describe={false}>
                            <span className="key-row__badge-label">{k.status}</span>
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
                            onOpenChange={(open) =>
                              setOpenMenus((s) => {
                                const next = new Set(s);
                                if (open) next.add(k.id);
                                else next.delete(k.id);
                                return next;
                              })
                            }
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
                  // A filter that matches no key keeps the table bare; only a registry with no key at all says so.
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
      <Dialog
        open={created !== null}
        onClose={() => setCreated(null)}
        title={created?.title ?? 'Key created'}
        subtitle="Copy the secret now. The router stores only a hash, so it cannot be shown again."
        footer={
          <Button variant="primary" onClick={() => setCreated(null)}>
            Done
          </Button>
        }
      >
        {created && (
          <>
            <div className="secret">
              <code>{created.key.secret}</code>
              <CopyButton text={created.key.secret} size="md" />
            </div>
            <p className="secondary">
              Set it as <code>CLAUDE_CODE_OAUTH_TOKEN</code> on the client. The Settings page has the full snippet.
            </p>
          </>
        )}
      </Dialog>
      <ConfirmDialog open={confirm !== null} onClose={() => setConfirm(null)} title={confirm?.title ?? ''} body={confirm?.body ?? ''} confirmLabel={confirm?.label} danger={confirm?.danger} onConfirm={() => confirm?.onConfirm()} />
    </div>
  );
}

/** What the key dialog reads of a pool: its auto mode and capture defaults, the rules a key inherits, and whose catalogs to suggest from. */
export type KeyDialogPool = Pick<Pool, 'autoModeServer' | 'bodyCaptureHours' | 'id' | 'modelRules' | 'name'> & { members: Pick<Pool['members'][number], 'accountId'>[] };

const KEY_GROUP_LABEL: Record<KeyGroup, string> = { access: 'Access', capture: 'Capture', key: 'Key', models: 'Models', sessions: 'Sessions' };

/**
 * Create or edit a key. `existing` switches the dialog to edit mode;
 * `initialPoolId` with `lockPool` pins the pool when the dialog is opened from
 * inside a pool's own dialog. `existingIds` are the listed keys' ids, which a new
 * key's derived id is checked against before the create.
 */
export function KeyDialog({ open, onClose, onCreated, onReload, onUpdated, pools, existing = null, existingIds, initialGroup = null, initialPoolId, lockPool = false }: { existing?: ApiKey | null; existingIds: ReadonlySet<string>; initialGroup?: KeyGroup | null; initialPoolId?: string; lockPool?: boolean; onClose: () => void; onCreated: (c: CreatedApiKey) => void; /** The stale save's Reload: closes the dialog and reads the keys again. */ onReload?: () => void; onUpdated?: (k: ApiKey) => void; open: boolean; pools: KeyDialogPool[] }) {
  const [chosen, setChosen] = useState<KeyGroup>('key');
  const [name, setName] = useState('');
  const [poolId, setPoolId] = useState('');
  const [longContextTwins, setLongContextTwins] = useState(true);
  const [modelRules, setModelRules] = useState<ModelRule[]>([]);
  const [modelRulesMode, setModelRulesMode] = useState<ModelRulesMode>('extend');
  const [expiry, setExpiry] = useState('never');
  const [autoModeServer, setAutoModeServer] = useState<AutoModeOverride>('inherit');
  const [notes, setNotes] = useState('');
  const [captureMode, setCaptureMode] = useState<CaptureMode>('inherit');
  const [captureHours, setCaptureHours] = useState('6');
  const [optOut, setOptOut] = useState(false);
  const [enabled, setEnabled] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [stale, setStale] = useState<string | null>(null);
  const settings = useAsync(() => api.getSettings(), []);
  const pool = pools.find((p) => p.id === poolId) ?? null;
  const poolName = pool?.name ?? poolId;
  const poolHours = pool?.bodyCaptureHours ?? 0;
  const maxHours = settings.data?.retention.bodyCaptureMaxHours ?? 72;
  const effectiveHours = captureMode === 'inherit' ? poolHours : captureMode === 'off' ? 0 : Number(captureHours);
  const suggestions = useCatalogSuggestions(pool?.members.map((m) => m.accountId) ?? [], open);
  const refusals = keyRefusals({ captureHours, captureMode, modelRules, name, poolId }, { identity: { existingIds, isNew: existing === null }, maxHours });
  const refusedFields = new Set(refusals.map((r) => r.field));
  const refusedGroups = new Set<KeyGroup>(refusals.map((r) => r.group));
  const invalid = (field: string) => (refusedFields.has(field) ? true : undefined);
  const refusalOf = (field: string) => refusals.find((r) => r.field === field)?.text ?? null;
  const first = refusals[0] ?? null;
  const rulesRefusal = refusalOf('Model rules');
  const processed = useAsync(() => (open && existing ? api.getKeyModels(existing.id) : Promise.resolve(null)), [open, existing?.id]);
  // The values the dialog opened with, which the discard question compares the fields against.
  const opened = useRef(keyFormOf(null, ''));

  // Once per opening: the address can open the dialog before the pool list lands, and a reset on its arrival would
  // discard every edit made meanwhile and put back the group the dialog opened on, the focus left in a hidden one.
  useEffect(() => {
    if (!open) return;
    // A key cannot exist without a pool, so the first pool stands in until one is picked.
    const form = keyFormOf(existing, initialPoolId ?? pools[0]?.id ?? '');
    opened.current = form;
    setChosen(initialGroup ?? 'key');
    setName(form.name);
    setPoolId(form.poolId);
    setLongContextTwins(form.longContextTwins);
    setModelRules(form.modelRules);
    setModelRulesMode(form.modelRulesMode);
    setExpiry(form.expiry);
    setAutoModeServer(form.autoModeServer);
    setNotes(form.notes);
    setCaptureMode(form.captureMode);
    setCaptureHours(form.captureHours);
    setOptOut(form.optOut);
    setEnabled(form.enabled);
    setBusy(false);
    setError(null);
    setStale(null);
  }, [open, existing, initialPoolId, initialGroup]);
  // A new key opened before the pools landed has no pool yet, so the first pool fills it once they do. An updater,
  // so it never replaces the pool the reset above queued in the same render. The fill is no edit of the operator's,
  // so the opening values take it too where they hold no pool yet.
  useEffect(() => {
    if (open && existing === null && poolId === '' && pools.length > 0) {
      setPoolId((current) => (current === '' ? pools[0].id : current));
      if (opened.current.poolId === '') opened.current = { ...opened.current, poolId: pools[0].id };
    }
  }, [existing, open, poolId, pools]);

  // The server's words leave with the next edit to any field; a failed save changes none, so they stand until then.
  useEffect(() => setError(null), [autoModeServer, captureHours, captureMode, enabled, expiry, longContextTwins, modelRules, modelRulesMode, name, notes, optOut, poolId]);

  const moving = existing !== null && poolId !== existing.poolId;
  const current = { autoModeServer, captureHours, captureMode, enabled, expiry, longContextTwins, modelRules, modelRulesMode, name, notes, optOut, poolId };

  const submit = async () => {
    setBusy(true);
    setError(null);
    setStale(null);
    const input = keyInputOf(current, clock.now());
    try {
      if (existing) {
        const updated = await api.updateKey(existing.id, input, existing.revision);
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
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const refusalCallout = (text: ReactNode) => (
    <div className="callout callout--critical" role="alert">
      <IconWarning size={16} />
      <div>{text}</div>
    </div>
  );

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
            <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="workstation" autoFocus aria-invalid={invalid('Name')} />
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
            <Select value={poolId} onChange={(e) => setPoolId(e.target.value)} disabled={lockPool} aria-invalid={invalid('Pool')}>
              {pools.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </Select>
          </Field>
          <Field label="Notes">
            <textarea className="input" value={notes} onChange={(e) => setNotes(e.target.value)} placeholder="Where this key lives and who holds it" />
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
              <Toggle checked={enabled} onChange={setEnabled} label="Enabled" disabled={existing.status === 'revoked'} />
            </span>
          </Field>
        )}
        <Field label="Expiry" hint="An expired key is refused, but its history stays in the ledger.">
          <Select value={expiry} onChange={(e) => setExpiry(e.target.value)} aria-label="Expiry">
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
            <Select value={autoModeServer} onChange={(e) => setAutoModeServer(e.target.value as AutoModeOverride)} aria-label="Server-side auto mode">
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
          hint={
            <>
              Top to bottom, first match wins. The <PageLink to="/models">Models page</PageLink> shows the list per key.
            </>
          }
        >
          <PatternEditor label="Model rules" value={modelRules} onChange={setModelRules} inherited={modelRulesMode === 'extend' ? (pool?.modelRules ?? []) : []} suggestions={suggestions} />
        </Field>
        {rulesRefusal && refusalCallout(rulesRefusal)}
        <Expert>
          {/* Field's frame without its label element: a label forwards a click on its text to the first control
              inside, which here would flip the mode. */}
          <Field label="Pool rules" plain hint="Extend applies the pool's rules, in gray, after this key's own. Replace applies this key's rules alone.">
            {/* Its own block, so the segmented control keeps its content width. */}
            <div>
              <Segmented<ModelRulesMode> ariaLabel="How this key's rules meet the pool's" value={modelRulesMode} options={MODES} onChange={setModelRulesMode} />
            </div>
          </Field>
        </Expert>
        <Expert>
          <Field label="List [1m] twins in the models listing" beside hint="Claude Code sizes its context window by the id, so the twin lets it use the long-context window the upstream reports.">
            <Toggle checked={longContextTwins} onChange={setLongContextTwins} label="List [1m] twins in the models listing" />
          </Field>
        </Expert>
        {existing && <EffectiveList processed={processed} />}
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
          <Toggle checked={optOut} onChange={setOptOut} label="Cache warming opt-out" />
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
                <input type="radio" name={`capture-${existing?.id ?? 'new'}`} checked={captureMode === 'inherit'} onChange={() => setCaptureMode('inherit')} />
                <span>Inherit from pool ({poolHours > 0 ? `${poolHours} h` : 'off'})</span>
              </label>
              <label className={`radio-list__item ${captureMode === 'override' ? 'is-active' : ''}`}>
                <input type="radio" name={`capture-${existing?.id ?? 'new'}`} checked={captureMode === 'override'} onChange={() => setCaptureMode('override')} />
                <span className="row">
                  Override:
                  <Input
                    type="number"
                    min={1}
                    max={maxHours}
                    value={captureHours}
                    onChange={(e) => setCaptureHours(e.target.value)}
                    onFocus={() => setCaptureMode('override')}
                    style={{ flex: 1 }}
                    aria-label="Capture hours for this key"
                    aria-invalid={invalid('Capture hours for this key')}
                  />
                  hours
                </span>
              </label>
              <label className={`radio-list__item ${captureMode === 'off' ? 'is-active' : ''}`}>
                <input type="radio" name={`capture-${existing?.id ?? 'new'}`} checked={captureMode === 'off'} onChange={() => setCaptureMode('off')} />
                <span>Off for this key</span>
              </label>
            </div>
          </Field>
        </div>
      </>
    ),
  };
  const groups: DialogGroup[] = KEY_GROUPS.map((id) => ({ body: bodies[id], id, label: KEY_GROUP_LABEL[id], refused: refusedGroups.has(id) }));

  return (
    <GroupedDialog
      open={open}
      onClose={onClose}
      title={existing ? `Edit ${existing.name}` : 'Create key'}
      unsaved={JSON.stringify(sortKeys(current)) !== JSON.stringify(sortKeys(opened.current))}
      railLabel="Key settings"
      groups={groups}
      chosen={chosen}
      initialGroup={initialGroup}
      onChoose={(id) => setChosen(id as KeyGroup)}
      // The line always explains the current hold: a row that moved under the dialog first, since only Reload clears
      // it, then a refusal of the form's own, which beats a server message left from before the edit.
      status={stale && existing ? `Save is held: the key ${existing.name} changed since you opened it. Reload to edit the current version.` : first ? `Save is held: ${first.text}` : (error ?? 'Every group saves together.')}
      hold={stale ? null : (first?.group ?? null)}
      held={first !== null || error !== null || stale !== null}
      notice={
        stale &&
        refusalCallout(
          <>
            {stale}{' '}
            <Button variant="ghost" onClick={onReload}>
              Reload
            </Button>
          </>,
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
  const rules = processed.stale ? null : processed.data?.rules;
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
        <ol className="effective-rules">
          {rules.map((r, i) => {
            const cooling = new Set(r.cooldowns.map((c) => c.accountId)).size;
            return (
              <li key={i}>
                <Badge tone={r.level === 'key' ? 'accent' : 'neutral'}>{r.level}</Badge>
                <span className="effective-rules__effect">{r.not ? 'Deny' : 'Allow'}</span>
                <span className="mono">{r.pattern}</span>
                {cooling > 0 && (
                  <Badge tone="warning" title={r.cooldowns.map((c) => `${c.accountId}: ${c.scope.replace(/^model:/, '')}`).join(', ')}>
                    {cooling} {cooling === 1 ? 'account' : 'accounts'} cooling
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

export type { ApiKey };
