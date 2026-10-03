import { useEffect, useMemo, useState } from 'react';
import { Link, useNavigate, useParams } from 'react-router-dom';
import { CartesianGrid, Line, LineChart, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts';
import { api } from '@/api';
import type { Account, AccountStatus, PairingPreview, PairingSession, QuotaHistory } from '@/api/types';
import { useSession } from '@/app/session';
import { CatalogDialog } from '@/components/CatalogDialog';
import { useChartInk } from '@/components/charts/useChartInk';
import { ChartTooltip, Legend } from '@/components/charts/Tooltip';
import { CoolingDown } from '@/components/CoolingDown';
import { FastLane } from '@/components/FastLane';
import { IconPencil, IconPlus, IconWarning } from '@/components/ui/icons';
import { Badge, Button, Card, ConfirmDialog, CopyButton, Descriptions, Dialog, EmptyState, ErrorState, Field, Input, KeyValue, Meter, SearchInput, Segmented, Select, Skeleton } from '@/components/ui/primitives';
import { WindowTriple } from '@/components/ui/QuotaBars';
import { fmtAbsolute, fmtCredits, fmtDate, fmtDayShort, fmtDuration, fmtEndsIn, fmtInt, fmtRelative } from '@/lib/format';
import { accountPatchOf, subscriptionEndFormOf, subscriptionEndRefusal, tickCancelled, type SubscriptionEndForm } from '@/lib/forms';
import { useAsync, useNow } from '@/lib/hooks';
import { STATUS_LABEL, STATUS_TONE, isFastScope } from '@/lib/quota';
import { joinTrend } from '@/lib/trend';

type SortKey = 'name' | 'reset' | 'status' | 'usage';
const END_HINT = "Prefilled from the plan's start and its monthly renewal; correct it from the date the plan's billing settings show. An annual plan is marked cancelled in its final month only.";

export function AccountsPage() {
  const { accountId } = useParams();
  const navigate = useNavigate();
  const [addOpen, setAddOpen] = useState(false);
  const [pairing, setPairing] = useState<{ accountId: string; label: string; repair: boolean } | null>(null);
  const [detailBusy, setDetailBusy] = useState(false);
  const accounts = useAsync(() => api.listAccounts(), [], { paused: addOpen || pairing !== null || detailBusy, refreshMs: 30_000 });
  const now = useNow();
  const [query, setQuery] = useState('');
  const [status, setStatus] = useState<'all' | AccountStatus>('all');
  const [sort, setSort] = useState<SortKey>('name');
  const { notify } = useSession();

  const list = useMemo(() => {
    const rows = (accounts.data ?? []).filter((a) => {
      if (status !== 'all' && a.status !== status) return false;
      const q = query.trim().toLowerCase();
      return !q || a.label.toLowerCase().includes(q) || (a.email ?? '').toLowerCase().includes(q) || (a.plan ?? '').toLowerCase().includes(q);
    });
    const nextReset = (a: Account) => Math.min(...a.windows.flatMap((w) => (w.resetsAt === null ? [] : [Date.parse(w.resetsAt)])).filter((t) => t > now), Number.POSITIVE_INFINITY);
    const usage = (a: Account) => Math.max(0, ...a.windows.map((w) => w.utilization));
    return rows.sort((a, b) => {
      if (sort === 'reset') return nextReset(a) - nextReset(b);
      if (sort === 'usage') return usage(b) - usage(a);
      if (sort === 'status') return a.status.localeCompare(b.status);
      return a.label.localeCompare(b.label);
    });
  }, [accounts.data, query, status, sort, now]);

  const selected = accounts.data?.find((a) => a.id === accountId) ?? null;
  // A deleted account's URL names a row the reloaded list no longer holds, which leads to the first one as a bare
  // /accounts does rather than to a detail that never loads.
  useEffect(() => {
    if (accounts.data && !selected && accounts.data.length) navigate(`/accounts/${accounts.data[0].id}`, { replace: true });
  }, [accounts.data, selected, navigate]);

  return (
    <div className="accounts">
      <aside className="accounts__list">
        <SearchInput placeholder="Search accounts" value={query} onChange={(e) => setQuery(e.target.value)} />
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
        <Button variant="primary" icon={<IconPlus size={14} />} onClick={() => setAddOpen(true)} className="accounts__add">
          Add account
        </Button>
        <ul className="account-rows">
          {!accounts.data
            ? accounts.error
              ? (
                  <li>
                    <ErrorState title="Could not load the accounts" body={accounts.error} action={<Button size="sm" onClick={accounts.reload}>Try again</Button>} />
                  </li>
                )
              : [0, 1, 2].map((i) => (
                  <li key={i} className="account-row">
                    <Skeleton width="70%" />
                    <Skeleton height={4} />
                  </li>
                ))
            : accounts.data.length === 0
              ? (
                  <li>
                    <EmptyState
                      title="The router holds no account yet"
                      body="Add an account to route requests."
                      action={
                        <Button size="sm" variant="primary" onClick={() => setAddOpen(true)}>
                          Add account
                        </Button>
                      }
                    />
                  </li>
                )
              : list.map((a) => <AccountRow key={a.id} account={a} active={a.id === accountId} now={now} />)}
          {accounts.data && accounts.data.length > 0 && list.length === 0 && (
            <li>
              <EmptyState title="No accounts match" body="Clear the search or the status filter." />
            </li>
          )}
        </ul>
      </aside>
      <section className="accounts__detail">
        {/* Accounts with none selected is a bare or stale URL the redirect above is about to replace, so nothing draws. */}
        {selected ? (
          <Descriptions>
            <AccountDetail account={selected} now={now} onBusyChange={setDetailBusy} onToast={notify} onPair={(repair) => setPairing({ accountId: selected.id, label: selected.label, repair })} onChanged={accounts.reload} />
          </Descriptions>
        ) : accounts.data === null ? (
          <Skeleton height={400} />
        ) : accounts.data.length === 0 ? (
          <EmptyState title="No account to show" body="Add an account to see its detail here." />
        ) : null}
      </section>
      <AddAccountDialog
        open={addOpen}
        onClose={() => setAddOpen(false)}
        onDone={(kind, created) => {
          setAddOpen(false);
          accounts.reload();
          if (kind === 'oauth') setPairing({ accountId: created.id, label: created.label, repair: false });
          else notify('Anthropic key added');
        }}
      />
      <PairingDialog
        open={pairing !== null}
        accountId={pairing?.accountId ?? ''}
        label={pairing?.label ?? ''}
        repair={pairing?.repair ?? false}
        onClose={() => setPairing(null)}
        onPaired={(preview) => {
          setPairing(null);
          accounts.reload();
          notify(`Paired ${preview.email}`);
        }}
      />
    </div>
  );
}

function AccountRow({ account, active, now }: { account: Account; active: boolean; now: number }) {
  return (
    <li>
      <Link to={`/accounts/${account.id}`} className={`account-row ${active ? 'is-active' : ''}`}>
        <div className="account-row__head">
          <span className="account-row__name">{account.label}</span>
          {account.plan && <Badge tone="neutral">{account.plan}</Badge>}
          <Badge tone={STATUS_TONE[account.status]} dot>
            {STATUS_LABEL[account.status]}
          </Badge>
        </div>
        {account.email && <div className="account-row__sub mono muted">{account.email}</div>}
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

function AccountDetail({ account, now, onBusyChange, onToast, onPair, onChanged }: { account: Account; now: number; onBusyChange: (busy: boolean) => void; onChanged: () => void; onPair: (repair: boolean) => void; onToast: (m: string) => void }) {
  const navigate = useNavigate();
  const history = useAsync(() => api.getQuotaHistory(account.id), [account.id]);
  const [busy, setBusy] = useState(false);
  const [catalogOpen, setCatalogOpen] = useState(false);
  const [confirm, setConfirm] = useState<{ body: string; danger?: boolean; label: string; onConfirm: () => void; title: string } | null>(null);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState('');
  // Null while the subscription end is not being edited.
  const [endForm, setEndForm] = useState<SubscriptionEndForm | null>(null);
  useEffect(() => {
    setEditing(false);
    setEndForm(null);
  }, [account.id]);
  // The list holds its refresh while anything here is open or running. The cleanup reports false too: Delete account
  // navigates away inside its action, so the detail can unmount before `busy` clears.
  const held = busy || catalogOpen || confirm !== null || editing || endForm !== null;
  useEffect(() => {
    onBusyChange(held);
    return () => onBusyChange(false);
  }, [held, onBusyChange]);
  const endRefusal = endForm ? subscriptionEndRefusal(endForm) : null;
  const endsAt = account.cancelled ? (account.subscription?.endsAt ?? null) : null;
  const trialEndsAt = account.subscription?.trialEndsAt ?? null;

  const isOauth = account.kind === 'oauth';
  // A flagged account reports its flag as the status, so routing is the only honest read of whether it takes requests.
  const paused = !account.routingEnabled;
  // A fast-lane row holds fast-mode requests alone, so the header counts the rows that cool the account.
  const cooldown = account.cooldowns.find((c) => !isFastScope(c.scope)) ?? null;
  const extra = account.extraUsage;
  const overage = account.overage;
  const run = async <T,>(action: () => Promise<T>, done: string | ((result: T) => string)) => {
    setBusy(true);
    try {
      const result = await action();
      onToast(typeof done === 'string' ? done : done(result));
      onChanged();
    } catch (e) {
      onToast(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  const act = (title: string, body: string, label: string, action: () => Promise<unknown>, done: string, danger = false) => setConfirm({ title, body, label, danger, onConfirm: () => void run(action, done) });
  // The editors close only once the router took the change, so a refusal leaves what was typed in front of the operator.
  const save = async () => {
    setBusy(true);
    try {
      await api.renameAccount(account.id, draft.trim());
      onToast('Label saved');
      onChanged();
      setEditing(false);
    } catch (e) {
      onToast(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  const saveEnd = async (form: SubscriptionEndForm) => {
    setBusy(true);
    try {
      await api.updateAccount(account.id, accountPatchOf(form));
      onToast('Subscription end saved');
      onChanged();
      setEndForm(null);
    } catch (e) {
      onToast(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="detail">
      <header className="detail__head">
        <div className="detail__ident">
          <h2 className="detail__title">
            {editing ? (
              <span className="row">
                <Input value={draft} onChange={(e) => setDraft(e.target.value)} style={{ width: 200 }} autoFocus />
                <Button size="sm" variant="primary" disabled={busy || draft.trim() === ''} onClick={() => void save()}>
                  Save
                </Button>
                <Button size="sm" variant="ghost" onClick={() => setEditing(false)}>
                  Cancel
                </Button>
              </span>
            ) : (
              <>
                {account.label}
                <button
                  type="button"
                  className="icon-btn"
                  aria-label="Edit label"
                  onClick={() => {
                    setDraft(account.label);
                    setEditing(true);
                  }}
                >
                  <IconPencil size={14} />
                </button>
              </>
            )}
          </h2>
          {account.email && <p className="detail__sub mono">{account.email}</p>}
          <div className="detail__badges">
            <Badge tone={STATUS_TONE[account.status]} dot>
              {STATUS_LABEL[account.status]}
            </Badge>
            {account.credential.quarantined && (
              <Badge tone="critical" title="the last refresh's outcome is uncertain; re-pair or wait for the next start to resolve it">
                quarantined
              </Badge>
            )}
            {cooldown && (
              <Badge tone="warning" title={fmtAbsolute(cooldown.until)}>
                cooldown ends {fmtRelative(cooldown.until, now)}
              </Badge>
            )}
            {extra?.inUse && <Badge tone="serious">Overage active</Badge>}
            {!account.routingEnabled && account.status !== 'paused' && <Badge tone="neutral">Routing paused</Badge>}
          </div>
          {/* A usage sample reports overage without a start, and a line counting from nothing would say nothing. */}
          {overage?.inUse && overage.since && (
            <p className="detail__sub" title={fmtAbsolute(overage.since)}>
              Overage since {fmtRelative(overage.since, now)}
            </p>
          )}
        </div>
        <div className="detail__actions">
          {isOauth ? (
            <Button variant={account.status === 'unpaired' || account.status === 'reauth_required' ? 'primary' : undefined} disabled={busy} onClick={() => onPair(Boolean(account.credential.refreshTokenExpiresAt))}>
              {account.credential.refreshTokenExpiresAt ? 'Re-pair' : 'Pair'}
            </Button>
          ) : (
            <Button
              variant="primary"
              disabled={busy}
              onClick={() =>
                act(
                  'Verify account',
                  'Sends a models request to the upstream with the stored key and reports the status.',
                  'Verify',
                  async () => {
                    const r = await api.verifyAccount(account.id);
                    if (!r.ok) throw new Error(`Upstream answered ${r.status}`);
                  },
                  'Verify: upstream answered 200',
                )
              }
            >
              Verify
            </Button>
          )}
          <Button
            disabled={busy}
            onClick={() =>
              act(
                paused ? 'Resume account' : 'Pause account',
                'Paused accounts stay in their pools but receive no new sessions. Pinned sessions migrate on their next request.',
                paused ? 'Resume' : 'Pause',
                paused ? () => api.resumeAccount(account.id) : () => api.pauseAccount(account.id),
                paused ? 'Resumed' : 'Paused',
              )
            }
          >
            {paused ? 'Resume' : 'Pause'}
          </Button>
          <Button onClick={() => setCatalogOpen(true)}>Catalog</Button>
          {account.status === 'funding_required' && (
            <Button
              disabled={busy}
              onClick={() =>
                act(
                  'Clear funding flag',
                  'Overage becomes eligible again on the next request. If the provider still refuses to fund it, the flag comes back with the new reason.',
                  'Clear',
                  () => api.clearFunding(account.id),
                  'Funding flag cleared',
                )
              }
            >
              Clear funding flag
            </Button>
          )}
          <Button
            variant="danger"
            disabled={busy}
            onClick={() =>
              act(
                'Disconnect account',
                `Deletes ${account.label}'s credential. Pool memberships and request history stay; re-pairing stores the credential again.`,
                'Disconnect',
                () => api.disconnectAccount(account.id),
                'Disconnected',
                true,
              )
            }
          >
            Disconnect
          </Button>
          <Button
            variant="danger"
            disabled={busy}
            onClick={() =>
              act(
                'Delete account',
                `Removes ${account.label} from every pool it is in and deletes it; request history stays in the ledger.`,
                'Delete',
                async () => {
                  await api.deleteAccount(account.id);
                  navigate('/accounts');
                },
                'Deleted',
                true,
              )
            }
          >
            Delete account
          </Button>
        </div>
      </header>

      {account.status === 'reauth_required' && (
        <div className="callout callout--critical">
          <IconWarning size={16} />
          <div>
            <b>Routing is off for this account.</b> The refresh token was rejected ({account.credential.lastError}). Re-pair to restore it.
          </div>
        </div>
      )}

      <div className="detail__pair">
        <Card
          title="Identity"
          className="card--fill"
          actions={
            // Kept while the editor is open, so the card's rows do not jump as the head loses its button.
            isOauth ? (
              <Button size="sm" disabled={endForm !== null} onClick={() => setEndForm(subscriptionEndFormOf(account))}>
                Edit end
              </Button>
            ) : undefined
          }
        >
          <KeyValue
            className="kv--spread"
            rows={
              // The kind decides the card, not the profile: an OAuth account has none until it is paired.
              isOauth
                ? [
                    { label: 'Type', value: 'OAuth login' },
                    {
                      label: 'Plan',
                      title: account.subscription?.rateLimitTier || undefined,
                      value: account.subscription ? `${account.plan ?? '-'}${account.subscription.since ? ` since ${fmtDate(account.subscription.since)}` : ''}` : 'unknown until paired',
                    },
                    { label: 'Rate-limit tier', value: account.subscription?.rateLimitTier ?? '-', mono: account.subscription !== null },
                    ...(endsAt ? [{ label: 'Ends', title: fmtAbsolute(endsAt), value: fmtEndsIn(endsAt, now) }] : []),
                    ...(trialEndsAt ? [{ label: 'Trial ends', title: fmtAbsolute(trialEndsAt), value: fmtEndsIn(trialEndsAt, now) }] : []),
                    { label: 'Organization', value: account.subscription?.organizationType ?? '-' },
                    { label: 'Billing type', value: account.subscription?.billingType ?? '-' },
                  ]
                : [
                    { label: 'Type', value: 'Anthropic key' },
                    { label: 'Windows', value: 'None reported; billed per token' },
                  ]
            }
          />
          {endForm && (
            <div className="capture" style={{ marginTop: 12 }}>
              <label className="row">
                <input type="checkbox" checked={endForm.cancelled} onChange={(e) => setEndForm(tickCancelled(endForm, e.target.checked, account.subscription?.derivedEndsAt ?? null))} />
                <span className="field__label">Cancelled</span>
              </label>
              <Field label="Ends" hint={END_HINT}>
                <Input type="datetime-local" value={endForm.endsAt} onChange={(e) => setEndForm({ ...endForm, endsAt: e.target.value })} style={{ width: 220 }} aria-label="Ends" />
              </Field>
              {endRefusal && (
                <div className="callout callout--critical" role="alert">
                  <IconWarning size={16} />
                  <div>{endRefusal}</div>
                </div>
              )}
              <span className="row">
                <Button size="sm" variant="primary" disabled={busy || endRefusal !== null} onClick={() => void saveEnd(endForm)}>
                  Save
                </Button>
                <Button size="sm" variant="ghost" onClick={() => setEndForm(null)}>
                  Cancel
                </Button>
              </span>
            </div>
          )}
          {isOauth && <p className="muted detail__note">Renewal date is not reported by the provider.</p>}
        </Card>

        <Card title="Usage" subtitle="Remaining share of each window" className="card--fill">
          {account.windows.length === 0 && !account.fableWindow ? (
            <p className="muted">This account does not report quota windows.</p>
          ) : (
            <WindowTriple account={account} now={now} />
          )}
          {extra && (extra.enabled || extra.disabledReason) && (
            <div className="extra-usage">
              {extra.enabled && (
                <>
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
                </>
              )}
              {extra.disabledReason && (
                <div className="callout callout--warning extra-usage__flag">
                  <IconWarning size={14} />
                  <div>{extra.disabledReason}</div>
                </div>
              )}
            </div>
          )}
          <FastLane account={account} now={now} />
        </Card>
      </div>

      {isOauth && (
        <Card title="7-day trend" subtitle="Utilization of each window, sampled every 5 minutes">
          <TrendChart history={history.data} loading={history.loading} hasFable={account.fableWindow !== null} />
        </Card>
      )}

      <div className="detail__grid">
        {/* One block per account, so each opens on its own rows rather than keeping the last one's toggle. */}
        <CoolingDown
          key={account.id}
          title="Cooling down"
          busy={busy}
          now={now}
          rows={account.cooldowns.map((c) => ({ accountId: account.id, reason: c.reason, scope: c.scope, until: c.until }))}
          onClear={(row) =>
            act(
              'Clear cooldown',
              'The account becomes eligible immediately. If the account is still rate limited, the next request re-enters cooldown.',
              'Clear',
              () => api.clearCooldown(account.id, row.scope),
              'Cooldown cleared',
            )
          }
        />
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
                      { label: 'Last error', value: account.credential.lastError ?? <span className="muted">none</span>, mono: Boolean(account.credential.lastError) },
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

      <CatalogDialog open={catalogOpen} accountId={account.id} label={account.label} now={now} onClose={() => setCatalogOpen(false)} />
      <ConfirmDialog open={confirm !== null} onClose={() => setConfirm(null)} title={confirm?.title ?? ''} body={confirm?.body ?? ''} confirmLabel={confirm?.label} danger={confirm?.danger} onConfirm={() => confirm?.onConfirm()} />
    </div>
  );
}

function TrendChart({ history, loading, hasFable }: { hasFable: boolean; history: QuotaHistory[] | null; loading: boolean }) {
  const ink = useChartInk();
  const data = useMemo(() => (history ? joinTrend(history, hasFable) : []), [history, hasFable]);
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
                    { color: ink.series[1], label: '7-day used', value: p.u7 === null ? 'no sample yet' : `${p.u7.toFixed(0)}%` },
                    ...(hasFable ? [{ color: ink.series[2], label: 'Fable 7-day used', value: p.uf === null ? 'no sample yet' : `${p.uf.toFixed(0)}%` }] : []),
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

function AddAccountDialog({ open, onClose, onDone }: { onClose: () => void; onDone: (kind: 'api_key' | 'oauth', created: Account) => void; open: boolean }) {
  const [kind, setKind] = useState<'api_key' | 'oauth'>('oauth');
  const [id, setId] = useState('');
  const [label, setLabel] = useState('');
  const [secret, setSecret] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // A reopened dialog starts blank: the id it last created is taken, and the router refuses a second account under it.
  useEffect(() => {
    if (!open) return;
    setKind('oauth');
    setId('');
    setLabel('');
    setSecret('');
    setError(null);
  }, [open]);

  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      const created = await api.createAccount({ id: id.trim(), kind, label: label.trim(), secret: kind === 'api_key' ? secret : null });
      onDone(kind, created);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title="Add account"
      subtitle="Subscription logins share your Claude.ai quota. Anthropic keys bill per token against a Console workspace."
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" disabled={busy || !id.trim() || !label.trim() || (kind === 'api_key' && !secret)} onClick={() => void create()}>
            {kind === 'oauth' ? 'Start pairing' : 'Add Anthropic key'}
          </Button>
        </>
      }
    >
      {/* The body is a flex column, which would stretch the segmented control across it. */}
      <div className="row">
        <Segmented<'api_key' | 'oauth'>
          ariaLabel="Account type"
          value={kind}
          onChange={setKind}
          options={[
            { value: 'oauth', label: 'OAuth login' },
            { value: 'api_key', label: 'Anthropic key' },
          ]}
        />
      </div>
      <div className="form-row">
        <Field label="Id" hint="Lowercase letters, digits, and dashes; it names the account in every log and URL">
          <Input value={id} onChange={(e) => setId(e.target.value)} placeholder="work" className="mono" />
        </Field>
        <Field label="Label" hint="Short name used in pools and logs.">
          <Input value={label} onChange={(e) => setLabel(e.target.value)} placeholder={kind === 'oauth' ? 'work' : 'Console workspace'} />
        </Field>
      </div>
      {kind === 'api_key' && (
        <Field label="Anthropic key">
          <Input type="password" autoComplete="off" value={secret} onChange={(e) => setSecret(e.target.value)} placeholder="sk-ant-…" className="mono" />
        </Field>
      )}
      {kind === 'oauth' && <p className="secondary">Pairing opens the Claude.ai login in your browser and asks you to paste the address it lands on. The router stores the refresh token encrypted and renews the access token on its own.</p>}
      {error && (
        <div className="callout callout--critical" role="alert">
          <IconWarning size={16} />
          <div>{error}</div>
        </div>
      )}
    </Dialog>
  );
}

type PairStep = 1 | 2 | 3;

const PAIR_STEPS: { label: string; step: PairStep }[] = [
  { step: 1, label: 'Log in' },
  { step: 2, label: 'Paste the address' },
  { step: 3, label: 'Confirm' },
];

function PairingDialog({ open, accountId, label, repair, onClose, onPaired }: { accountId: string; label: string; onClose: () => void; onPaired: (p: PairingPreview) => void; open: boolean; repair: boolean }) {
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
    api.startPairing(accountId).then(
      (s) => {
        // A pairing that lands after the dialog closed would hold the account until its own deadline.
        if (alive) setSession(s);
        else void api.cancelPairing(s.id).catch(() => undefined);
      },
      (e: unknown) => {
        if (alive) setError(e instanceof Error ? e.message : String(e));
      },
    );
    return () => {
      alive = false;
    };
  }, [open, accountId]);

  const dismiss = () => {
    // The router keeps the account busy until the pairing expires, so leaving unconfirmed gives the session back.
    if (session) void api.cancelPairing(session.id).catch(() => undefined);
    onClose();
  };

  const expired = session ? Date.parse(session.expiresAt) <= now : false;
  const remaining = session ? Math.max(0, Date.parse(session.expiresAt) - now) : 0;

  const submit = async () => {
    if (!session) return;
    setBusy(true);
    setError(null);
    try {
      const pasted = callback.trim();
      // A pasted callback URL carries its own state; a bare code borrows the one this pairing started under.
      const p = await api.completePairing(session.id, /^https?:\/\//i.test(pasted) ? { url: pasted } : { code: pasted, state: session.state });
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
    setError(null);
    try {
      await api.confirmPairing(session.id);
      onPaired(preview);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const title = repair ? `Re-pair ${label}` : `Pair ${label || 'new account'}`;
  return (
    <Dialog
      open={open}
      onClose={dismiss}
      title={title}
      width={600}
      footer={
        step === 1 ? (
          <>
            <Button variant="ghost" onClick={dismiss}>
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
            <Button variant="ghost" onClick={dismiss}>
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

      {error && (
        <div className="callout callout--critical" role="alert">
          <IconWarning size={16} />
          <div>{error}</div>
        </div>
      )}

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
          <PairingExpiry remaining={remaining} expired={expired} ready={session !== null} />
        </div>
      )}

      {step === 3 && preview && (
        <div className="stack">
          <p className="secondary">Confirm the identity below to store the refresh token for this account.</p>
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
  // The visible line ticks every second under a minute; a screen reader hears the time in whole minutes alone.
  const minutes = Math.ceil(remaining / 60_000);
  return (
    <>
      {expired ? <span className="pairing__expired">This link has expired. Close the dialog and start again.</span> : <span className="muted num">Link expires in {fmtDuration(remaining)}</span>}
      <span className="sr-only" aria-live="polite">
        {expired ? 'This link has expired.' : `Link expires in ${minutes} ${minutes === 1 ? 'minute' : 'minutes'}`}
      </span>
    </>
  );
}
