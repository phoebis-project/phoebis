import { useEffect, useMemo, useState } from 'react';
import { Link, useNavigate, useParams } from 'react-router-dom';
import { CartesianGrid, Line, LineChart, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts';
import { api } from '@/api';
import type { Account, AccountStatus, CatalogModel, PairingPreview, PairingSession, QuotaHistory, ServedModel } from '@/api/types';
import { useAppSettings } from '@/app/settings';
import { useChartInk } from '@/components/charts/useChartInk';
import { ChartTooltip, Legend } from '@/components/charts/Tooltip';
import { IconPencil, IconPlus, IconRefresh, IconWarning } from '@/components/ui/icons';
import { Badge, Button, Card, ConfirmDialog, CopyButton, Dialog, EmptyState, Field, Input, KeyValue, Meter, SearchInput, Segmented, Select, Skeleton, Toast } from '@/components/ui/primitives';
import { WindowTriple } from '@/components/ui/QuotaBars';
import { fmtAbsolute, fmtCredits, fmtDate, fmtDayShort, fmtDuration, fmtInt, fmtRelative } from '@/lib/format';
import { useAsync, useNow } from '@/lib/hooks';
import { STATUS_LABEL, STATUS_TONE } from '@/lib/quota';

type SortKey = 'name' | 'reset' | 'status' | 'usage';

export function AccountsPage() {
  const { accountId } = useParams();
  const navigate = useNavigate();
  const accounts = useAsync(() => api.listAccounts(), []);
  const catalog = useAsync(() => api.listCatalog(), []);
  const now = useNow();
  const { canMutate } = useAppSettings();
  const [query, setQuery] = useState('');
  const [status, setStatus] = useState<'all' | AccountStatus>('all');
  const [sort, setSort] = useState<SortKey>('name');
  const [addOpen, setAddOpen] = useState(false);
  const [pairing, setPairing] = useState<{ accountId: string | null; label: string } | null>(null);
  const [toast, setToast] = useState<string | null>(null);

  const list = useMemo(() => {
    const rows = (accounts.data ?? []).filter((a) => {
      if (status !== 'all' && a.status !== status) return false;
      const q = query.trim().toLowerCase();
      return !q || a.label.toLowerCase().includes(q) || (a.email ?? '').toLowerCase().includes(q) || (a.plan ?? '').toLowerCase().includes(q);
    });
    const nextReset = (a: Account) => Math.min(...a.windows.map((w) => Date.parse(w.resetsAt)).filter((t) => t > now), Number.POSITIVE_INFINITY);
    const usage = (a: Account) => Math.max(0, ...a.windows.map((w) => w.utilization));
    return rows.sort((a, b) => {
      if (sort === 'reset') return nextReset(a) - nextReset(b);
      if (sort === 'usage') return usage(b) - usage(a);
      if (sort === 'status') return a.status.localeCompare(b.status);
      return a.label.localeCompare(b.label);
    });
  }, [accounts.data, query, status, sort, now]);

  const selected = accounts.data?.find((a) => a.id === accountId) ?? null;
  useEffect(() => {
    if (accounts.data && !accountId && accounts.data.length) navigate(`/upstreams/${accounts.data[0].id}`, { replace: true });
  }, [accounts.data, accountId, navigate]);

  return (
    <div className="accounts">
      <aside className="accounts__list">
        <SearchInput placeholder="Search upstreams" value={query} onChange={(e) => setQuery(e.target.value)} />
        <div className="accounts__filters">
          <Select value={status} onChange={(e) => setStatus(e.target.value as 'all' | AccountStatus)} aria-label="Status filter">
            <option value="all">All statuses</option>
            {(Object.keys(STATUS_LABEL) as AccountStatus[]).map((s) => (
              <option key={s} value={s}>
                {STATUS_LABEL[s]}
              </option>
            ))}
          </Select>
          <Select value={sort} onChange={(e) => setSort(e.target.value as SortKey)} aria-label="Sort">
            <option value="name">Name</option>
            <option value="reset">Soonest reset</option>
            <option value="usage">Most used</option>
            <option value="status">Status</option>
          </Select>
        </div>
        {canMutate && (
          <Button variant="primary" icon={<IconPlus size={14} />} onClick={() => setAddOpen(true)} className="accounts__add">
            Add upstream
          </Button>
        )}
        <ul className="account-rows">
          {!accounts.data
            ? [0, 1, 2].map((i) => (
                <li key={i} className="account-row">
                  <Skeleton width="70%" />
                  <Skeleton height={4} />
                </li>
              ))
            : list.map((a) => <AccountRow key={a.id} account={a} active={a.id === accountId} now={now} />)}
          {accounts.data && list.length === 0 && (
            <li>
              <EmptyState title="No upstreams match" body="Clear the search or the status filter." />
            </li>
          )}
        </ul>
      </aside>
      <section className="accounts__detail">
        {selected ? (
          <AccountDetail account={selected} catalog={catalog.data ?? []} now={now} onToast={setToast} onPair={() => setPairing({ accountId: selected.id, label: selected.label })} onChanged={accounts.reload} />
        ) : (
          <Skeleton height={400} />
        )}
      </section>
      <AddAccountDialog
        open={addOpen}
        onClose={() => setAddOpen(false)}
        onDone={(kind, label) => {
          setAddOpen(false);
          if (kind === 'oauth') setPairing({ accountId: null, label });
          else setToast('API key upstream added');
        }}
      />
      <PairingDialog
        open={pairing !== null}
        accountId={pairing?.accountId ?? null}
        label={pairing?.label ?? ''}
        onClose={() => setPairing(null)}
        onPaired={(preview) => {
          setPairing(null);
          setToast(`Paired ${preview.email}`);
        }}
      />
      <Toast message={toast} onDone={() => setToast(null)} />
    </div>
  );
}

function AccountRow({ account, active, now }: { account: Account; active: boolean; now: number }) {
  return (
    <li>
      <Link to={`/upstreams/${account.id}`} className={`account-row ${active ? 'is-active' : ''}`}>
        <div className="account-row__head">
          <span className="account-row__name">{account.label}</span>
          {account.plan && <Badge tone="neutral">{account.plan}</Badge>}
          <Badge tone={STATUS_TONE[account.status]} dot>
            {STATUS_LABEL[account.status]}
          </Badge>
        </div>
        <WindowTriple account={account} now={now} compact />
        <div className="account-row__foot">
          <span className="muted">
            {account.pools.length} {account.pools.length === 1 ? 'pool' : 'pools'}
          </span>
          {account.extraUsage?.inUse && <Badge tone="serious">Overage</Badge>}
          {account.extraUsage?.enabled && !account.extraUsage.inUse && <span className="muted">extra usage on</span>}
        </div>
      </Link>
    </li>
  );
}

function AccountDetail({ account, catalog, now, onToast, onPair, onChanged }: { account: Account; catalog: CatalogModel[]; now: number; onChanged: () => void; onPair: () => void; onToast: (m: string) => void }) {
  const { canMutate } = useAppSettings();
  const history = useAsync(() => api.getQuotaHistory(account.id), [account.id]);
  const [confirm, setConfirm] = useState<{ body: string; danger?: boolean; label: string; title: string } | null>(null);
  const [editing, setEditing] = useState(false);
  const [label, setLabel] = useState(account.label);
  useEffect(() => {
    setLabel(account.label);
    setEditing(false);
  }, [account.id, account.label]);

  const isOauth = account.kind === 'oauth';
  const cooldown = account.cooldowns[0] ?? null;
  const extra = account.extraUsage;
  const act = (title: string, body: string, label: string, danger = false) => setConfirm({ title, body, label, danger });

  return (
    <div className="detail">
      <header className="detail__head">
        <div className="detail__ident">
          <h2 className="detail__title">
            {editing ? (
              <span className="row">
                <Input value={label} onChange={(e) => setLabel(e.target.value)} style={{ width: 200 }} autoFocus />
                <Button
                  size="sm"
                  variant="primary"
                  onClick={() => {
                    setEditing(false);
                    onToast('Label saved');
                  }}
                >
                  Save
                </Button>
                <Button size="sm" variant="ghost" onClick={() => setEditing(false)}>
                  Cancel
                </Button>
              </span>
            ) : (
              <>
                {label}
                {canMutate && (
                  <button type="button" className="icon-btn" aria-label="Edit label" onClick={() => setEditing(true)}>
                    <IconPencil size={14} />
                  </button>
                )}
              </>
            )}
          </h2>
          <div className="detail__badges">
            <Badge tone={STATUS_TONE[account.status]} dot>
              {STATUS_LABEL[account.status]}
            </Badge>
            {cooldown && (
              <Badge tone="warning" title={fmtAbsolute(cooldown.until)}>
                cooldown ends {fmtRelative(cooldown.until, now)}
              </Badge>
            )}
            {extra?.inUse && <Badge tone="serious">Overage active</Badge>}
            {!account.routingEnabled && account.status !== 'paused' && <Badge tone="neutral">Routing paused</Badge>}
          </div>
        </div>
        {canMutate && (
          <div className="detail__actions">
            {isOauth ? (
              <Button variant="primary" onClick={onPair}>
                {account.credential.refreshTokenExpiresAt ? 'Re-pair' : 'Pair'}
              </Button>
            ) : (
              <Button variant="primary" onClick={() => act('Verify upstream', `Sends a count_tokens probe to the upstream and records the round trip.`, 'Verify')}>
                Verify
              </Button>
            )}
            <Button onClick={() => act(account.status === 'paused' ? 'Resume upstream' : 'Pause upstream', 'Paused upstreams stay in their pools but receive no new sessions. Pinned sessions migrate on their next request.', account.status === 'paused' ? 'Resume' : 'Pause')}>
              {account.status === 'paused' ? 'Resume' : 'Pause'}
            </Button>
            {account.cooldowns.length > 0 && <Button onClick={() => act('Clear cooldown', 'The upstream becomes eligible immediately. If the upstream is still rate limited, the next request re-enters cooldown.', 'Clear')}>Clear cooldown</Button>}
            {extra?.disabledReason && <Button onClick={() => act('Clear funding flag', 'Overage becomes eligible again on the next request. If the provider still refuses to fund it, the flag comes back with the new reason.', 'Clear')}>Clear funding flag</Button>}
            <Button variant="danger" onClick={() => act('Disconnect upstream', `Removes ${account.label} from every pool and deletes its credential. Request history stays in the ledger.`, 'Disconnect', true)}>
              Disconnect
            </Button>
          </div>
        )}
      </header>

      {account.status === 'reauth_required' && (
        <div className="callout callout--critical">
          <IconWarning size={16} />
          <div>
            <b>Routing is off for this upstream.</b> The refresh token was rejected ({account.credential.lastError}). Re-pair to restore it.
          </div>
        </div>
      )}
      {account.status === 'cooldown' && cooldown && (
        <div className="callout callout--warning">
          <IconWarning size={16} />
          <div>
            Cooling down until <b title={fmtAbsolute(cooldown.until)}>{fmtRelative(cooldown.until, now).replace('in ', '')} from now</b>: {cooldown.reason}. Sessions pinned here have migrated to other members.
          </div>
        </div>
      )}

      <div className="detail__pair">
        <Card title="Identity" className="card--fill">
          <KeyValue
            className="kv--spread"
            rows={
              isOauth && account.subscription
                ? [
                    { label: 'Plan', value: account.plan ?? '-' },
                    { label: 'Rate-limit tier', value: account.subscription.rateLimitTier, mono: true },
                    { label: 'Subscription status', value: account.subscription.status.replace('_', ' ') },
                    { label: 'Subscription since', value: fmtDate(account.subscription.since), title: fmtAbsolute(account.subscription.since) },
                    { label: 'Billing type', value: account.subscription.billingType },
                    { label: 'Trial end', value: account.subscription.trialEndsAt ? fmtDate(account.subscription.trialEndsAt) : <span className="muted">none</span>, title: fmtAbsolute(account.subscription.trialEndsAt) },
                  ]
                : [
                    { label: 'Type', value: 'API key' },
                    { label: 'Windows', value: 'None reported; billed per token' },
                  ]
            }
          />
          {isOauth && <p className="muted detail__note">Renewal date is not reported by the provider.</p>}
        </Card>

        <Card title="Usage" subtitle="Remaining share of each window" className="card--fill">
          {account.windows.length === 0 && !account.fableWindow ? (
            <p className="muted">This upstream does not report quota windows.</p>
          ) : (
            <WindowTriple account={account} now={now} />
          )}
          {extra?.enabled && (
            <div className="extra-usage">
              <div className="wbar__head">
                <span className="wbar__label">
                  Extra usage this month
                  {extra.inUse && (
                    <Badge tone="serious" title="Requests are currently billed as overage">
                      in use
                    </Badge>
                  )}
                </span>
                <span className="num">
                  {fmtCredits(extra.usedCredits)} of {fmtCredits(extra.monthlyLimitCredits)}
                </span>
              </div>
              <Meter value={extra.usedCredits / extra.monthlyLimitCredits} tone="accent" label="Extra usage spend" />
              {extra.disabledReason && (
                <div className="callout callout--warning extra-usage__flag">
                  <IconWarning size={14} />
                  <div>{extra.disabledReason}</div>
                </div>
              )}
            </div>
          )}
        </Card>
      </div>

      {isOauth && (
        <Card title="7-day trend" subtitle="Utilization of each window, sampled every 5 minutes">
          <TrendChart history={history.data} loading={history.loading} hasFable={account.fableWindow !== null} />
        </Card>
      )}

      <div className="detail__grid">
        <Card
          title="Served models"
          subtitle={
            <span title={fmtAbsolute(account.catalogSyncedAt)}>
              Catalog synced {fmtRelative(account.catalogSyncedAt, now)}
            </span>
          }
          actions={canMutate && (
            <Button size="sm" icon={<IconRefresh size={13} />} onClick={() => onToast('Catalog re-synced')}>
              Re-sync
            </Button>
          )}
        >
          <ServedModels account={account} catalog={catalog} canMutate={canMutate} onSaved={onChanged} />
        </Card>

        <div className="stack">
          <Card title="Pools">
            {account.pools.length === 0 ? (
              <p className="muted">Not a member of any pool.</p>
            ) : (
              <ul className="plain-list">
                {account.pools.map((p) => (
                  <li key={p.poolId} className="row">
                    <Link to="/pools" className="mono">
                      {p.poolId}
                    </Link>
                    <span className="muted">priority {p.priority}</span>
                  </li>
                ))}
              </ul>
            )}
          </Card>
          <Card title="Credential">
            <KeyValue
              rows={
                isOauth
                  ? [
                      { label: 'Access token', value: account.credential.accessTokenExpiresAt ? `expires ${fmtRelative(account.credential.accessTokenExpiresAt, now)}` : '-', title: fmtAbsolute(account.credential.accessTokenExpiresAt) },
                      { label: 'Refresh token', value: account.credential.refreshTokenExpiresAt ? `expires ${fmtRelative(account.credential.refreshTokenExpiresAt, now)}` : '-', title: fmtAbsolute(account.credential.refreshTokenExpiresAt) },
                      { label: 'Last refresh', value: fmtRelative(account.credential.lastRefreshAt, now), title: fmtAbsolute(account.credential.lastRefreshAt) },
                      { label: 'Last error', value: account.credential.lastError ?? <span className="muted">none</span>, mono: Boolean(account.credential.lastError) },
                    ]
                  : [
                      { label: 'Secret', value: 'stored encrypted at rest', mono: false },
                      { label: 'Last error', value: <span className="muted">none</span> },
                    ]
              }
            />
          </Card>
          <Card title="Requests">
            <div className="req-totals">
              <div>
                <span className="stat__label">Last 24h</span>
                <span className="stat__value">{fmtInt(account.requests.last24h)}</span>
              </div>
              <div>
                <span className="stat__label">Last 7d</span>
                <span className="stat__value">{fmtInt(account.requests.last7d)}</span>
              </div>
              <div>
                <span className="stat__label">Lifetime</span>
                <span className="stat__value">{fmtInt(account.requests.lifetime)}</span>
              </div>
            </div>
          </Card>
        </div>
      </div>

      <ConfirmDialog open={confirm !== null} onClose={() => setConfirm(null)} title={confirm?.title ?? ''} body={confirm?.body ?? ''} confirmLabel={confirm?.label} danger={confirm?.danger} onConfirm={() => onToast(`${confirm?.label ?? 'Done'}: queued`)} />
    </div>
  );
}

function ServedModels({ account, catalog, canMutate, onSaved }: { account: Account; canMutate: boolean; catalog: CatalogModel[]; onSaved: () => void }) {
  const [models, setModels] = useState<ServedModel[]>(account.servedModels);
  const [extra, setExtra] = useState('');
  useEffect(() => {
    setModels(account.servedModels);
  }, [account.id, account.servedModels]);

  const persist = async (next: ServedModel[]) => {
    setModels(next);
    await api.updateServedModels(account.id, next);
    onSaved();
  };
  const toggle = (id: string) => persist(models.map((m) => (m.id === id ? { ...m, selected: !m.selected } : m)));

  // Catalog rows come first in catalog order; manual and vanished ids follow.
  const catalogIds = catalog.map((m) => m.id);
  const ordered = [...models].sort((a, b) => {
    const ia = catalogIds.indexOf(a.id);
    const ib = catalogIds.indexOf(b.id);
    if (ia === -1 && ib === -1) return a.id.localeCompare(b.id);
    if (ia === -1) return 1;
    if (ib === -1) return -1;
    return ia - ib;
  });

  return (
    <div className="served">
      <ul className="served__list">
        {ordered.map((m) => (
          <li key={m.id}>
            <label>
              <input type="checkbox" checked={m.selected} onChange={() => toggle(m.id)} disabled={!canMutate} />
              <span className="mono">{m.id}</span>
              {m.isNew && !m.selected && (
                <Badge tone="accent" title="Discovered by a catalog sync after the upstream was added">
                  new
                </Badge>
              )}
              {m.longContext && <Badge tone="neutral">long context</Badge>}
              {!m.inCatalog && (
                <Badge tone="warning" title="Not in the last catalog sync">
                  not in catalog
                </Badge>
              )}
              {m.upstreamId !== m.canonicalId && <span className="muted mono">as {m.upstreamId}</span>}
            </label>
          </li>
        ))}
      </ul>
      {canMutate && (
        <form
          className="served__add"
          onSubmit={(e) => {
            e.preventDefault();
            const id = extra.trim();
            if (!id || models.some((m) => m.id === id)) return;
            persist([...models, { canonicalId: id, id, inCatalog: false, isNew: false, longContext: false, manual: true, selected: true, upstreamId: id }]);
            setExtra('');
          }}
        >
          <Input placeholder="Add model id" value={extra} onChange={(e) => setExtra(e.target.value)} className="mono" />
          <Button type="submit" size="md" disabled={!extra.trim()}>
            Add
          </Button>
        </form>
      )}
    </div>
  );
}

function TrendChart({ history, loading, hasFable }: { hasFable: boolean; history: QuotaHistory[] | null; loading: boolean }) {
  const ink = useChartInk();
  const data = useMemo(() => {
    if (!history) return [];
    const h5 = history.find((h) => h.windowId === '5h')?.samples ?? [];
    const by7 = new Map((history.find((h) => h.windowId === '7d')?.samples ?? []).map((s) => [s.at, s.utilization]));
    const byF = new Map((history.find((h) => h.windowId === '7d_fable')?.samples ?? []).map((s) => [s.at, s.utilization]));
    // 5-minute samples over 7 days is 2,016 points; every third is plenty for a 900px plot.
    return h5.filter((_, i) => i % 3 === 0).map((s) => ({ at: Date.parse(s.at), u5: s.utilization * 100, u7: (by7.get(s.at) ?? 0) * 100, uf: hasFable ? (byF.get(s.at) ?? 0) * 100 : null }));
  }, [history, hasFable]);
  if (loading || !history) return <Skeleton height={220} />;
  if (data.length === 0) return <EmptyState title="No samples yet" body="Quota samples appear after the first successful quota fetch." />;
  return (
    <>
      <ResponsiveContainer width="100%" height={220}>
        <LineChart data={data} margin={{ top: 8, right: 12, bottom: 0, left: 0 }}>
          <CartesianGrid vertical={false} stroke={ink.grid} />
          <XAxis dataKey="at" type="number" domain={['dataMin', 'dataMax']} tickFormatter={(v: number) => fmtDayShort(v)} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={60} />
          <YAxis domain={[0, 100]} ticks={[0, 25, 50, 75, 100]} tickFormatter={(v: number) => `${v}%`} tickLine={false} axisLine={false} width={40} />
          <Tooltip
            cursor={{ stroke: ink.axis }}
            content={({ payload }) => {
              const p = payload?.[0]?.payload as (typeof data)[number] | undefined;
              if (!p) return null;
              return (
                <ChartTooltip
                  title={fmtAbsolute(new Date(p.at).toISOString())}
                  rows={[
                    { color: ink.series[0], label: '5-hour used', value: `${p.u5.toFixed(0)}%` },
                    { color: ink.series[1], label: '7-day used', value: `${p.u7.toFixed(0)}%` },
                    ...(hasFable ? [{ color: ink.series[2], label: 'Fable 7-day used', value: `${(p.uf ?? 0).toFixed(0)}%` }] : []),
                  ]}
                />
              );
            }}
          />
          <Line type="linear" dataKey="u5" stroke={ink.series[0]} strokeWidth={1.5} dot={false} isAnimationActive={false} />
          <Line type="monotone" dataKey="u7" stroke={ink.series[1]} strokeWidth={2} dot={false} isAnimationActive={false} />
          {hasFable && <Line type="monotone" dataKey="uf" stroke={ink.series[2]} strokeWidth={2} strokeDasharray="5 4" dot={false} isAnimationActive={false} />}
        </LineChart>
      </ResponsiveContainer>
      <Legend
        items={[
          { color: ink.series[0], label: '5-hour window', shape: 'line' },
          { color: ink.series[1], label: '7-day window', shape: 'line' },
          ...(hasFable ? [{ color: ink.series[2], dashed: true, label: 'Fable 7-day window', shape: 'line' as const }] : []),
        ]}
      />
    </>
  );
}

function AddAccountDialog({ open, onClose, onDone }: { onClose: () => void; onDone: (kind: 'api_key' | 'oauth', label: string) => void; open: boolean }) {
  const [kind, setKind] = useState<'api_key' | 'oauth'>('oauth');
  const [label, setLabel] = useState('');
  const [secret, setSecret] = useState('');
  return (
    <Dialog
      open={open}
      onClose={onClose}
      title="Add upstream"
      subtitle="Subscription logins share your Claude.ai quota. API keys bill per token against a Console workspace."
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" disabled={!label.trim() || (kind === 'api_key' && !secret)} onClick={() => onDone(kind, label.trim())}>
            {kind === 'oauth' ? 'Start pairing' : 'Add API key'}
          </Button>
        </>
      }
    >
      <Segmented<'api_key' | 'oauth'>
        ariaLabel="Upstream type"
        value={kind}
        onChange={setKind}
        options={[
          { value: 'oauth', label: 'OAuth login' },
          { value: 'api_key', label: 'API key' },
        ]}
      />
      <Field label="Label" hint="Short name used in pools and logs.">
        <Input value={label} onChange={(e) => setLabel(e.target.value)} placeholder={kind === 'oauth' ? 'work' : 'Console workspace'} />
      </Field>
      {kind === 'api_key' && (
        <>
          <Field label="API key">
            <Input type="password" autoComplete="off" value={secret} onChange={(e) => setSecret(e.target.value)} placeholder="sk-ant-…" className="mono" />
          </Field>
        </>
      )}
      {kind === 'oauth' && <p className="secondary">Pairing opens the Claude.ai login in your browser and asks you to paste the address it lands on. The router stores the refresh token encrypted and renews the access token on its own.</p>}
    </Dialog>
  );
}

type PairStep = 1 | 2 | 3;

const PAIR_STEPS: { label: string; step: PairStep }[] = [
  { step: 1, label: 'Log in' },
  { step: 2, label: 'Paste the address' },
  { step: 3, label: 'Confirm' },
];

function PairingDialog({ open, accountId, label, onClose, onPaired }: { accountId: string | null; label: string; onClose: () => void; onPaired: (p: PairingPreview) => void; open: boolean }) {
  const [session, setSession] = useState<PairingSession | null>(null);
  const [step, setStep] = useState<PairStep>(1);
  const [callback, setCallback] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [preview, setPreview] = useState<PairingPreview | null>(null);
  const now = useNow(1_000);

  useEffect(() => {
    if (!open) return;
    let alive = true;
    setSession(null);
    setStep(1);
    setCallback('');
    setError(null);
    setPreview(null);
    api.startPairing(accountId).then((s) => {
      if (alive) setSession(s);
    });
    return () => {
      alive = false;
    };
  }, [open, accountId]);

  const expired = session ? Date.parse(session.expiresAt) <= now : false;
  const remaining = session ? Math.max(0, Date.parse(session.expiresAt) - now) : 0;

  const submit = async () => {
    if (!session) return;
    setBusy(true);
    setError(null);
    try {
      const pasted = callback.trim();
      // A pasted callback URL carries its own state; a bare code borrows the one this pairing started under.
      const p = await api.completePairing(/^https?:\/\//i.test(pasted) ? { url: pasted } : { code: pasted, state: session.state });
      setPreview(p);
      setStep(3);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const confirm = async () => {
    if (!session || !preview) return;
    setBusy(true);
    await api.confirmPairing(session.state);
    setBusy(false);
    onPaired(preview);
  };

  const title = accountId ? `Re-pair ${label}` : `Pair ${label || 'new upstream'}`;
  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={title}
      width={600}
      footer={
        step === 1 ? (
          <>
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button variant="primary" disabled={!session || expired} onClick={() => setStep(2)}>
              I have logged in
            </Button>
          </>
        ) : step === 2 ? (
          <>
            <Button variant="ghost" onClick={() => setStep(1)}>
              Back
            </Button>
            <Button variant="primary" disabled={!callback.trim() || busy || expired} onClick={submit}>
              {busy ? 'Checking…' : 'Submit'}
            </Button>
          </>
        ) : (
          <>
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button variant="primary" disabled={busy} onClick={confirm}>
              {busy ? 'Saving…' : 'Confirm'}
            </Button>
          </>
        )
      }
    >
      <ol className="steps" aria-label="Pairing steps">
        {PAIR_STEPS.map((s) => (
          <li key={s.step} className={s.step === step ? 'is-current' : s.step < step ? 'is-done' : ''} aria-current={s.step === step ? 'step' : undefined}>
            <span className="steps__num">{s.step}</span>
            {s.label}
          </li>
        ))}
      </ol>

      {step === 1 && (
        <div className="stack">
          <p className="secondary">Open the authorization link, sign in to Claude.ai, and approve the router. The browser then lands on a callback address; keep that tab open for the next step.</p>
          {session ? (
            <div className="secret">
              <code className="pairing__url">{session.authorizeUrl}</code>
              <CopyButton text={session.authorizeUrl} label="Copy" />
            </div>
          ) : (
            <Skeleton height={40} />
          )}
          <div className="row">
            <a className="btn btn--secondary btn--md" href={session?.authorizeUrl ?? '#'} target="_blank" rel="noreferrer" aria-disabled={!session}>
              Open in browser
            </a>
            <PairingExpiry remaining={remaining} expired={expired} ready={session !== null} />
          </div>
        </div>
      )}

      {step === 2 && (
        <div className="stack">
          <Field label="Paste the URL your browser landed on, or the code">
            <Input value={callback} onChange={(e) => setCallback(e.target.value)} placeholder={`${session?.redirectUri ?? 'http://localhost:…/callback'}?code=…&state=…`} className="mono" autoFocus />
          </Field>
          {session && (
            <p className="muted">
              Registered redirect: <code>{session.redirectUri}</code>
            </p>
          )}
          <p className="muted">The page will not load; nothing listens on that port. The URL in the address bar is complete, and the code alone works too.</p>
          {error && (
            <div className="callout callout--critical" role="alert">
              <IconWarning size={16} />
              <div>{error}</div>
            </div>
          )}
          <PairingExpiry remaining={remaining} expired={expired} ready={session !== null} />
        </div>
      )}

      {step === 3 && preview && (
        <div className="stack">
          <p className="secondary">Confirm the identity below to store the refresh token for this upstream.</p>
          <KeyValue
            rows={[
              { label: 'Email', value: preview.email, mono: true },
              { label: 'Organization', value: preview.organization },
            ]}
          />
        </div>
      )}
    </Dialog>
  );
}

function PairingExpiry({ remaining, expired, ready }: { expired: boolean; ready: boolean; remaining: number }) {
  if (!ready) return <span className="muted">Preparing the login link…</span>;
  if (expired) return <span className="pairing__expired">This link has expired. Close the dialog and start again.</span>;
  return (
    <span className="muted num" aria-live="polite">
      Link expires in {fmtDuration(remaining)}
    </span>
  );
}
