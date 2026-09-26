import { useEffect, useMemo, useRef, useState } from 'react';
import { api } from '@/api';
import type { ApiKey, CreatedApiKey, KeyModels, KeyStatus, ModelRule, ModelRulesMode, Pool } from '@/api/types';
import { useAppSettings } from '@/app/settings';
import { useChartInk } from '@/components/charts/useChartInk';
import { PatternEditor, useCatalogSuggestions } from '@/components/PatternEditor';
import { IconInfo, IconPlus, IconWarning } from '@/components/ui/icons';
import { Badge, Button, Card, ConfirmDialog, CopyButton, Dialog, ErrorState, Field, Input, Menu, SearchInput, SectionedDialog, Segmented, Select, Skeleton, Toast, Toggle, type DialogSection } from '@/components/ui/primitives';
import { fmtAbsolute, fmtCompact, fmtDate, fmtInt, fmtRelative, fmtUsd } from '@/lib/format';
import { inFlightCapRefusal, keyFormOf, keyInputOf, modelRulesRefusal, type CaptureMode } from '@/lib/forms';
import { useAsync, useNow, type Loaded } from '@/lib/hooks';

const STATUS_TONE: Record<KeyStatus, 'critical' | 'good' | 'neutral' | 'warning'> = { active: 'good', disabled: 'warning', expired: 'neutral', revoked: 'critical' };
/** A key with no override of its own captures for as long as its pool says. */
export const effectiveCaptureHours = (key: Pick<ApiKey, 'bodyCaptureHours'>, pool: Pick<Pool, 'bodyCaptureHours'> | null): number => key.bodyCaptureHours ?? pool?.bodyCaptureHours ?? 0;
const captureLabel = (hours: number) => (hours > 0 ? `bodies captured, ${hours}h` : 'bodies not captured');
const CAPTURE_INHERIT_HINT = 'This key sets no capture of its own, so it follows its pool.';
const MODES: { label: string; value: ModelRulesMode }[] = [
  { label: 'Extend pool rules', value: 'extend' },
  { label: 'Replace pool rules', value: 'replace' },
];
const RETIRED_SECRET_HINT = 'Sessions whose newest request presented the previous secret; a session that registered a companion and sent no request since counts as switched.';
const IN_FLIGHT_CAP_HINT = 'Past this many requests in flight the key is refused with a 429 and a Retry-After until one finishes.';

export function KeysPage() {
  const keys = useAsync(() => api.listKeys(), []);
  const pools = useAsync(() => api.listPools(), []);
  const now = useNow();
  const ink = useChartInk();
  const { canMutate } = useAppSettings();
  const [query, setQuery] = useState('');
  const [status, setStatus] = useState<'all' | KeyStatus>('all');
  const [dialog, setDialog] = useState<{ key: ApiKey | null } | null>(null);
  // Held only while the dialog that shows it is open, because the router keeps no copy of a secret.
  const [created, setCreated] = useState<{ key: CreatedApiKey; title: string } | null>(null);
  const [confirm, setConfirm] = useState<{ body: string; danger?: boolean; label: string; onConfirm: () => void; title: string } | null>(null);
  const [toast, setToast] = useState<string | null>(null);

  const poolList = useMemo(() => pools.data ?? [], [pools.data]);
  const poolById = useMemo(() => new Map(poolList.map((p) => [p.id, p])), [poolList]);
  const rows = useMemo(
    () =>
      (keys.data ?? []).filter((k) => {
        if (status !== 'all' && k.status !== status) return false;
        const q = query.trim().toLowerCase();
        return !q || k.name.toLowerCase().includes(q) || k.prefix.includes(q) || k.poolId.includes(q);
      }),
    [keys.data, query, status],
  );

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
      setToast(e instanceof Error ? e.message : String(e));
      return;
    }
    keys.reload();
  };

  return (
    <div className="stack">
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
                        {fmtUsd(k.lifetime.cost)} <span className="muted">{fmtCompact(k.lifetime.requests)} requests</span>
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
            <SearchInput placeholder="Search keys" value={query} onChange={(e) => setQuery(e.target.value)} />
            <Select value={status} onChange={(e) => setStatus(e.target.value as 'all' | KeyStatus)} aria-label="Status">
              <option value="all">All statuses</option>
              <option value="active">Active</option>
              <option value="disabled">Disabled</option>
              <option value="expired">Expired</option>
              <option value="revoked">Revoked</option>
            </Select>
            {canMutate && (
              <Button variant="primary" icon={<IconPlus size={14} />} onClick={() => setDialog({ key: null })} style={{ marginLeft: 'auto' }}>
                Create API key
              </Button>
            )}
          </div>
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Prefix</th>
                  <th>Pool</th>
                  <th>Models</th>
                  <th className="right">Usage, 7d</th>
                  <th>Expiry</th>
                  <th>Status</th>
                  {canMutate && <th />}
                </tr>
              </thead>
              <tbody>
                {!keys.data
                  ? keys.error
                    ? (
                        <tr>
                          <td colSpan={8}>
                            <ErrorState title="Could not load the API keys" body={keys.error} action={<Button size="sm" onClick={keys.reload}>Try again</Button>} />
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
                      <tr key={k.id}>
                        <td>
                          <b>{k.name}</b>
                          <span className="sub">
                            {captureLabel(effectiveCaptureHours(k, poolById.get(k.poolId) ?? null))}
                            {k.bodyCaptureHours === null && (
                              <span className="muted" title={CAPTURE_INHERIT_HINT}>
                                {' '}
                                inherited
                              </span>
                            )}
                          </span>
                          {k.keepaliveOptOut && <span className="sub">keepalive off</span>}
                          {k.inFlightCap !== null && (
                            <span className="sub" title={IN_FLIGHT_CAP_HINT}>
                              at most {fmtInt(k.inFlightCap)} in flight
                            </span>
                          )}
                        </td>
                        <td className="mono">{k.prefix}…</td>
                        <td className="mono">{k.poolId}</td>
                        <td>{k.modelRules.length ? <span className="mono" title={k.modelRules.map((r) => (r.not ? `NOT ${r.pattern}` : r.pattern)).join(', ')}>{k.modelRules.length === 1 ? '1 rule' : `${k.modelRules.length} rules`}{k.modelRulesMode === 'replace' && ' (replace)'}</span> : <span className="muted">{k.modelRulesMode === 'replace' ? 'No rules' : 'Pool rules'}</span>}</td>
                        <td className="right num">
                          {fmtCompact(k.usage7d.tokens)} tok
                          <span className="sub">{fmtUsd(k.usage7d.cost)}</span>
                        </td>
                        <td title={k.expiresAt ? fmtAbsolute(k.expiresAt) : undefined}>{k.expiresAt ? fmtDate(k.expiresAt) : <span className="muted">Never</span>}</td>
                        <td>
                          <Badge tone={STATUS_TONE[k.status]} dot>
                            {k.status}
                          </Badge>
                          {k.lastUsedAt && <span className="sub">used {fmtRelative(k.lastUsedAt, now)}</span>}
                          {/* The route answers 0 once the rotation grace has passed, so the page needs no clock of its own. */}
                          {k.sessionsOnRetiredSecret > 0 && (
                            <span className="sub" title={k.retiredSecretLastUsedAt ? `${RETIRED_SECRET_HINT} Previous secret last used ${fmtRelative(k.retiredSecretLastUsedAt, now)}.` : RETIRED_SECRET_HINT}>
                              {k.sessionsOnRetiredSecret} {k.sessionsOnRetiredSecret === 1 ? 'session' : 'sessions'} still on the previous secret
                            </span>
                          )}
                        </td>
                        {canMutate && (
                          <td className="right">
                            <Menu
                              items={[
                                { label: 'Edit', onClick: () => setDialog({ key: k }) },
                                {
                                  label: 'Rotate',
                                  onClick: () =>
                                    setConfirm({
                                      title: `Rotate ${k.name}`,
                                      body: 'A new secret is issued and shown once. The old secret keeps working for 15 minutes so running sessions can switch.',
                                      label: 'Rotate',
                                      onConfirm: () =>
                                        void run(async () => {
                                          setCreated({ key: await api.rotateKey(k.id), title: 'API key rotated' });
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
                                          setToast('Key revoked');
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
                                          setToast('Key deleted');
                                        }),
                                    }),
                                },
                              ]}
                            />
                          </td>
                        )}
                      </tr>
                    ))}
              </tbody>
            </table>
          </div>
        </Card>
      </div>

      <KeyDialog
        open={dialog !== null}
        existing={dialog?.key ?? null}
        onClose={() => setDialog(null)}
        pools={poolList}
        onCreated={(c) => {
          setDialog(null);
          setCreated({ key: c, title: 'API key created' });
          keys.reload();
        }}
        onUpdated={() => {
          setDialog(null);
          setToast('Key saved');
          keys.reload();
        }}
      />
      <Dialog
        open={created !== null}
        onClose={() => setCreated(null)}
        title={created?.title ?? 'API key created'}
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

/** What the key dialog reads of a pool: its capture default, the rules a key inherits, and whose catalogs to suggest from. */
export type KeyDialogPool = Pick<Pool, 'bodyCaptureHours' | 'id' | 'modelRules' | 'name'> & { members: Pick<Pool['members'][number], 'accountId'>[] };

/**
 * Create or edit an API key. `existing` switches the dialog to edit mode;
 * `initialPoolId` with `lockPool` pins the pool when the dialog is opened from
 * inside a pool's own dialog.
 */
export function KeyDialog({ open, onClose, onCreated, onUpdated, pools, existing = null, initialPoolId, lockPool = false }: { existing?: ApiKey | null; initialPoolId?: string; lockPool?: boolean; onClose: () => void; onCreated: (c: CreatedApiKey) => void; onUpdated?: (k: ApiKey) => void; open: boolean; pools: KeyDialogPool[] }) {
  const [name, setName] = useState('');
  const [poolId, setPoolId] = useState('all');
  const [longContextTwins, setLongContextTwins] = useState(true);
  const [modelRules, setModelRules] = useState<ModelRule[]>([]);
  const [modelRulesMode, setModelRulesMode] = useState<ModelRulesMode>('extend');
  const [expiry, setExpiry] = useState('never');
  const [inFlightCap, setInFlightCap] = useState('');
  const [notes, setNotes] = useState('');
  const [captureMode, setCaptureMode] = useState<CaptureMode>('inherit');
  const [captureHours, setCaptureHours] = useState(6);
  const [optOut, setOptOut] = useState(false);
  const [enabled, setEnabled] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const errorRef = useRef<HTMLDivElement>(null);
  const settings = useAsync(() => api.getSettings(), []);
  const pool = pools.find((p) => p.id === poolId) ?? null;
  const poolName = pool?.name ?? poolId;
  const poolHours = pool?.bodyCaptureHours ?? 0;
  const maxHours = settings.data?.retention.bodyCaptureMaxHours ?? 72;
  const effectiveHours = captureMode === 'inherit' ? poolHours : captureMode === 'off' ? 0 : captureHours;
  const suggestions = useCatalogSuggestions(pool?.members.map((m) => m.accountId) ?? [], open);
  const rulesRefusal = modelRulesRefusal(modelRules);
  const capRefusal = inFlightCapRefusal(inFlightCap);
  const processed = useAsync(() => (open && existing ? api.getKeyModels(existing.id) : Promise.resolve(null)), [open, existing?.id]);

  useEffect(() => {
    if (!open) return;
    // A key cannot exist without a pool, so the first pool stands in until one is picked.
    const form = keyFormOf(existing, initialPoolId ?? pools[0]?.id ?? '');
    setName(form.name);
    setPoolId(form.poolId);
    setLongContextTwins(form.longContextTwins);
    setModelRules(form.modelRules);
    setModelRulesMode(form.modelRulesMode);
    setExpiry(form.expiry);
    setInFlightCap(form.inFlightCap);
    setNotes(form.notes);
    setCaptureMode(form.captureMode);
    setCaptureHours(form.captureHours);
    setOptOut(form.optOut);
    setEnabled(form.enabled);
    setBusy(false);
    setError(null);
  }, [open, existing, initialPoolId, pools]);

  // The callout leads the first section, and the operator may be reading another one when the save is refused.
  // jsdom leaves scrollIntoView undefined.
  useEffect(() => {
    if (error) errorRef.current?.scrollIntoView?.({ block: 'nearest' });
  }, [error]);

  const moving = existing !== null && poolId !== existing.poolId;

  const submit = async () => {
    setBusy(true);
    setError(null);
    const input = keyInputOf({ captureHours, captureMode, enabled, expiry, inFlightCap, longContextTwins, modelRules, modelRulesMode, name, notes, optOut, poolId }, Date.now());
    try {
      if (existing) {
        const updated = await api.updateKey(existing.id, input);
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
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const sections: DialogSection[] = [
    {
      id: 'general',
      label: 'General',
      body: (
        <>
          {error && (
            <div ref={errorRef} className="callout callout--critical" role="alert">
              <IconWarning size={16} />
              <div>{error}</div>
            </div>
          )}
          <div className="form-row">
            <Field label="Name">
              <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="workstation" autoFocus />
            </Field>
            <Field label="Pool" hint={lockPool ? `Created in pool ${poolName}` : 'A key reaches only the members of the pool it belongs to.'}>
              <Select value={poolId} onChange={(e) => setPoolId(e.target.value)} disabled={lockPool}>
                {pools.map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.name}
                  </option>
                ))}
              </Select>
            </Field>
          </div>
          {moving && (
            <div className="callout callout--warning" role="status">
              <IconInfo size={16} />
              <div>Moving this key ends its current session pins.</div>
            </div>
          )}
          {existing && (
            <div className="field--inline field">
              <span className="field__text">
                <span className="field__label">Enabled</span>
                <p className="field__hint">Off refuses the key at routing and keeps it, so it can be switched back on without a new secret.</p>
              </span>
              {/* The route refuses to re-enable a revoked key, so its switch says so rather than offering it. */}
              <span title={existing.status === 'revoked' ? 'a revoked key cannot be re-enabled' : undefined}>
                <Toggle checked={enabled} onChange={setEnabled} label="Enabled" disabled={existing.status === 'revoked'} />
              </span>
            </div>
          )}
          <Field label="Expiry">
            <Select value={expiry} onChange={(e) => setExpiry(e.target.value)} style={{ width: 220 }} aria-label="Expiry">
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
          <Field label="Requests in flight" hint={IN_FLIGHT_CAP_HINT}>
            <Input type="number" min={1} step={1} value={inFlightCap} onChange={(e) => setInFlightCap(e.target.value)} placeholder="No cap" style={{ width: 120 }} aria-label="Requests in flight" />
          </Field>
          {capRefusal && (
            <div className="callout callout--critical" role="alert">
              <IconWarning size={16} />
              <div>{capRefusal}</div>
            </div>
          )}
          <Field label="Notes">
            <textarea className="input" value={notes} onChange={(e) => setNotes(e.target.value)} placeholder="Where this key lives and who holds it" />
          </Field>
        </>
      ),
    },
    {
      id: 'models',
      label: 'Models',
      body: (
        <>
          {/* Field's frame without its label element: a label forwards a click on its text to the first control
              inside, which here would flip the mode. */}
          <div className="field">
            <div className="field__control">
              <span className="field__label">Model rules</span>
              <div>
                <Segmented<ModelRulesMode> ariaLabel="How this key's rules meet the pool's" value={modelRulesMode} options={MODES} onChange={setModelRulesMode} />
              </div>
              <PatternEditor label="Model rules" value={modelRules} onChange={setModelRules} inherited={modelRulesMode === 'extend' ? (pool?.modelRules ?? []) : []} suggestions={suggestions} />
            </div>
            <p className="field__hint">
              {modelRulesMode === 'extend' ? "Top to bottom, first match wins; the pool's rules, in grey, apply after this key's own." : "Top to bottom, first match wins; the pool's rules do not apply to this key."}
            </p>
          </div>
          {rulesRefusal && (
            <div className="callout callout--critical" role="alert">
              <IconWarning size={16} />
              <div>{rulesRefusal}</div>
            </div>
          )}
          <div className="field--inline field">
            <span className="field__text">
              <span className="field__label">List [1m] twins in the models listing</span>
              <p className="field__hint">Claude Code sizes its context window by the id; the twin lets it use the long-context window the upstream reports.</p>
            </span>
            <Toggle checked={longContextTwins} onChange={setLongContextTwins} label="List [1m] twins in the models listing" />
          </div>
          {existing && <EffectiveList processed={processed} />}
        </>
      ),
    },
    {
      id: 'keepalive',
      label: 'Cache warming',
      body: (
        <div className="capture">
          <div className="field--inline field">
            <span className="field__text">
              <span className="field__label">Cache warming opt-out</span>
              <p className="field__hint">Sessions on this key never receive cache-warming probes, even when the pool keeps caches warm.</p>
            </span>
            <Toggle checked={optOut} onChange={setOptOut} label="Cache warming opt-out" />
          </div>
        </div>
      ),
    },
    {
      id: 'capture',
      label: 'Capture',
      body: (
        <div className="capture">
          <span className="field__label">Capture request and response bodies</span>
          <p className="field__hint">Stores the full JSON of each exchange so the request drawer can show it.</p>
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
                  onChange={(e) => setCaptureHours(Number(e.target.value))}
                  onFocus={() => setCaptureMode('override')}
                  style={{ width: 90 }}
                  aria-label="Capture hours for this key"
                />
                hours
              </span>
            </label>
            <label className={`radio-list__item ${captureMode === 'off' ? 'is-active' : ''}`}>
              <input type="radio" name={`capture-${existing?.id ?? 'new'}`} checked={captureMode === 'off'} onChange={() => setCaptureMode('off')} />
              <span>Off for this key</span>
            </label>
          </div>
          {effectiveHours > 0 && (
            <div className="callout callout--warning">
              <IconWarning size={16} />
              <div>Bodies contain the prompts, file contents, and tool results of every session using this key. Only the admin secret can read them.</div>
            </div>
          )}
        </div>
      ),
    },
  ];

  return (
    <SectionedDialog
      open={open}
      onClose={onClose}
      title={existing ? `Edit ${existing.name}` : 'Create API key'}
      subtitle="A key belongs to one pool and can only reach that pool's members."
      sections={sections}
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" disabled={!name.trim() || !poolId || busy || rulesRefusal !== null || capRefusal !== null} onClick={submit}>
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
        <p className="muted">No rules: every model the pool's upstreams serve is allowed.</p>
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
                    {cooling} {cooling === 1 ? 'upstream' : 'upstreams'} cooling
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
