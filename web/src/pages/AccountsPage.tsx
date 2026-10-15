import { useEffect, useMemo, useRef, useState } from 'react';
import { Link, useLocation, useNavigate, useParams } from 'react-router-dom';
import { CartesianGrid, Line, LineChart, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts';
import { api, ApiError } from '@/api';
import type { Account, PairingPreview, PairingSession, QuotaHistory } from '@/api/types';
import { useSession } from '@/app/session';
import { CatalogDialog } from '@/components/CatalogDialog';
import { useChartInk } from '@/components/charts/useChartInk';
import { ChartTooltip, Legend } from '@/components/charts/Tooltip';
import { CoolingDown } from '@/components/CoolingDown';
import { FastLane } from '@/components/FastLane';
import { FilterBar } from '@/components/FilterBar';
import { IconPencil, IconPlus, IconWarning } from '@/components/ui/icons';
import { Badge, Button, Card, ConfirmDialog, CopyButton, Descriptions, Dialog, EmptyState, ErrorState, Field, Input, KeyValue, Meter, Segmented, Select, Skeleton } from '@/components/ui/primitives';
import { WindowTriple } from '@/components/ui/QuotaBars';
import { ACCOUNTS_CODEC } from '@/lib/address';
import { ACCOUNTS_DIMENSIONS } from '@/lib/filterBar';
import { fmtAbsolute, fmtCredits, fmtDate, fmtDayShort, fmtDuration, fmtEndsIn, fmtInt, fmtRelative } from '@/lib/format';
import { accountPatchOf, advancesOwnToken, derivedId, subscriptionEndFormOf, subscriptionEndRefusal, tickCancelled, type SubscriptionEndForm } from '@/lib/forms';
import { useAsync, useNow, useQueryFilters } from '@/lib/hooks';
import { ACCOUNT_STATUSES, STATUS_LABEL, STATUS_TONE, isFastScope } from '@/lib/quota';
import { aroundSubject, logHref } from '@/lib/routingLog';
import { joinTrend } from '@/lib/trend';

/** The Status dimension's options, in the labels' order. */
const STATUS_OPTIONS = { status: ACCOUNT_STATUSES.map((s) => ({ label: STATUS_LABEL[s], value: s })) };
/** A save the router refused, as the editor's callout draws it. `stale` marks a row that moved under the editor. */
type EditorRefusal = { message: string; stale: boolean };
const END_HINT = "The date the plan's billing settings show as its end. An annual plan is marked cancelled in its final month only.";

export function AccountsPage() {
  const { accountId } = useParams();
  const location = useLocation();
  const navigate = useNavigate();
  const { address, filters, update } = useQueryFilters(ACCOUNTS_CODEC);
  const [addOpen, setAddOpen] = useState(false);
  const [pairing, setPairing] = useState<{ start: PairingTarget; label: string; repair: boolean } | null>(null);
  const [detailBusy, setDetailBusy] = useState(false);
  const [intervalMs, setIntervalMs] = useState(5_000);
  const [barOpen, setBarOpen] = useState(false);
  // A save in flight or an editor open in the detail holds the list as a dialog does.
  const dialogOpen = addOpen || pairing !== null || detailBusy;
  const accounts = useAsync(() => api.listAccounts(), [], { paused: dialogOpen || barOpen, refreshMs: intervalMs });
  const now = useNow();
  const { notify } = useSession();

  const list = useMemo(() => {
    const { search, sort, status } = filters;
    const rows = (accounts.data ?? []).filter((a) => {
      if (status !== 'all' && a.status !== status) return false;
      const q = search.trim().toLowerCase();
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
  }, [accounts.data, filters, now]);

  const selected = accounts.data?.find((a) => a.id === accountId) ?? null;
  // The row a confirmed pairing created: the read that lists it lands after the URL names it, so the redirect below
  // leaves that URL alone for the span of that read. The guard follows the read's loading flag rather than the list
  // changing, since a failed read keeps the old list and would otherwise hold the guard until the next refresh.
  const [created, setCreated] = useState<{ id: string; reading: boolean } | null>(null);
  if (created !== null) {
    if (accounts.loading) {
      if (!created.reading) setCreated({ ...created, reading: true });
    } else if (created.reading) setCreated(null);
  }
  // A bare /accounts, and a deleted account's URL that names a row the reloaded list no longer holds, lead to the
  // router's first account the list shows, so the detail never opens on a row the filters hide. Where they hide every
  // row it is the router's first. The query and the hash go along, since the address hook keeps the location for its
  // own writes alone.
  const first = accounts.data?.find((a) => list.includes(a)) ?? accounts.data?.[0];
  useEffect(() => {
    if (created !== null && accountId === created.id) return;
    if (accounts.data && !selected && first !== undefined) navigate({ hash: location.hash, pathname: `/accounts/${first.id}`, search: location.search }, { replace: true });
  }, [accounts.data, accountId, created, first, location.hash, location.search, navigate, selected]);

  return (
    <div className="accounts">
      <FilterBar
        address={address}
        count={accounts.data === null ? '' : `${list.length} ${list.length === 1 ? 'account' : 'accounts'}`}
        dimensions={ACCOUNTS_DIMENSIONS}
        filters={filters}
        onOpenChange={setBarOpen}
        options={STATUS_OPTIONS}
        placeholder="Search accounts"
        refresh={{ intervalMs, onIntervalChange: setIntervalMs, onRefresh: accounts.reload, paused: dialogOpen ? 'dialog' : barOpen ? 'menu' : null, refreshTitle: 'Reads the list again now' }}
        update={update}
      >
        <label className="row">
          <span className="muted">Sort</span>
          <Select value={filters.sort} onChange={(e) => update({ sort: e.target.value })} aria-label="Sort">
            <option value="name">Name</option>
            <option value="reset">Soonest reset</option>
            <option value="usage">Most used</option>
            <option value="status">Status</option>
          </Select>
        </label>
        <Button variant="primary" icon={<IconPlus size={14} />} onClick={() => setAddOpen(true)}>
          Add account
        </Button>
      </FilterBar>
      <aside className="accounts__list">
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
                        <Button size="sm" onClick={() => setAddOpen(true)}>
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
            {/* Keyed by the account, so a save still running when the operator moves on answers into its own
                unmounted detail and never moves the token of an editor another account opened. */}
            <AccountDetail key={selected.id} account={selected} now={now} onBusyChange={setDetailBusy} onToast={notify} onPair={(repair) => setPairing({ start: { accountId: selected.id }, label: selected.label, repair })} onChanged={accounts.reload} />
          </Descriptions>
        ) : accounts.data === null ? (
          <Skeleton height={400} />
        ) : accounts.data.length === 0 ? (
          <EmptyState title="No account to show" body="Add an account to see its detail here." />
        ) : null}
      </section>
      <AddAccountDialog
        open={addOpen}
        existingIds={new Set((accounts.data ?? []).map((a) => a.id))}
        onClose={() => setAddOpen(false)}
        onDone={(done) => {
          setAddOpen(false);
          // An OAuth account is written when its pairing confirms, so there is nothing to reload yet.
          if (done.kind === 'oauth') setPairing({ start: { id: done.id, session: done.session }, label: done.label, repair: false });
          else {
            accounts.reload();
            notify('API key added');
          }
        }}
      />
      <PairingDialog
        open={pairing !== null}
        start={pairing?.start ?? { accountId: '' }}
        label={pairing?.label ?? ''}
        repair={pairing?.repair ?? false}
        onClose={() => setPairing(null)}
        onPaired={(preview) => {
          const start = pairing?.start ?? null;
          setPairing(null);
          accounts.reload();
          notify(`Paired ${preview.email}`);
          if (start !== null && 'id' in start) {
            setCreated({ id: start.id, reading: false });
            navigate({ hash: location.hash, pathname: `/accounts/${start.id}`, search: location.search });
          }
        }}
      />
    </div>
  );
}

function AccountRow({ account, active, now }: { account: Account; active: boolean; now: number }) {
  // The row keeps the list's query and hash, so opening an account leaves the filters standing.
  const location = useLocation();
  return (
    <li>
      <Link to={{ hash: location.hash, pathname: `/accounts/${account.id}`, search: location.search }} className={`account-row ${active ? 'is-active' : ''}`}>
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
          <span className="muted">{account.pool ? `pool ${account.pool.poolId}` : 'no pool'}</span>
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
  // The revision the title editor opened with; the end editor's form carries its own, and this editor has no form.
  const [labelRevision, setLabelRevision] = useState(0);
  // Null while the subscription end is not being edited.
  const [endForm, setEndForm] = useState<SubscriptionEndForm | null>(null);
  // One refusal per editor, so neither shows the other's.
  const [labelRefused, setLabelRefused] = useState<EditorRefusal | null>(null);
  const [endRefused, setEndRefused] = useState<EditorRefusal | null>(null);
  // A save's answer until `onChanged`'s reload replaces the row, so an editor opened in between opens on the row the
  // save wrote rather than the one it replaced.
  const [answered, setAnswered] = useState<Account | null>(null);
  const [answeredFor, setAnsweredFor] = useState(account);
  if (answeredFor !== account) {
    setAnsweredFor(account);
    setAnswered(null);
  }
  const current = answered ?? account;
  // The list holds its refresh while anything here is open or running. The cleanup reports false too: Delete account
  // navigates away inside its action, so the detail can unmount before `busy` clears.
  const held = busy || catalogOpen || confirm !== null || editing || endForm !== null;
  useEffect(() => {
    onBusyChange(held);
    return () => onBusyChange(false);
  }, [held, onBusyChange]);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
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
  /**
   * The router's answer once it took the change, null once it refused. An editor's refusal reaches its own callout, or
   * the shell's toast naming the account once the operator left it, since the callout left with the detail.
   */
  const run = async <T,>(action: () => Promise<T>, done: string | ((result: T) => string), onRefused?: (refusal: EditorRefusal) => void): Promise<{ answer: T } | null> => {
    setBusy(true);
    try {
      const answer = await action();
      onToast(typeof done === 'string' ? done : done(answer));
      onChanged();
      return { answer };
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      // A stale refusal says the row moved under the editor: the edit is about values that are gone, so the operator
      // reloads and re-applies it rather than saving over what someone else wrote.
      if (onRefused && !mounted.current) onToast(`Save on ${account.label} refused: ${message}`);
      else if (onRefused) onRefused({ message, stale: e instanceof ApiError && e.status === 409 && e.code === 'stale_edit' });
      else onToast(message);
      return null;
    } finally {
      setBusy(false);
    }
  };
  const act = (title: string, body: string, label: string, action: () => Promise<unknown>, done: string, danger = false) => setConfirm({ title, body, label, danger, onConfirm: () => void run(action, done) });
  // The editors close only once the router took the change, so a refusal leaves what was typed in front of the operator.
  // A save the router took moves the row's token one past the one it sent, which the other open editor holds too when
  // both opened on the same row, so that editor takes the answer's token rather than meet the fence over the operator's
  // own write.
  const save = async () => {
    const sent = labelRevision;
    setLabelRefused(null);
    const saved = await run(() => api.renameAccount(account.id, draft.trim(), sent), 'Label saved', setLabelRefused);
    if (!saved) return;
    const { revision } = saved.answer;
    setAnswered(saved.answer);
    setEndForm((form) => (form && advancesOwnToken(sent, revision, form.revision) ? { ...form, revision } : form));
    setEditing(false);
  };
  const saveEnd = async (form: SubscriptionEndForm) => {
    setEndRefused(null);
    const saved = await run(() => api.updateAccount(account.id, accountPatchOf(form), form.revision), 'Subscription end saved', setEndRefused);
    if (!saved) return;
    const { revision } = saved.answer;
    setAnswered(saved.answer);
    setLabelRevision((held) => (advancesOwnToken(form.revision, revision, held) ? revision : held));
    setEndForm(null);
  };
  // The two editors' refusal callouts. A stale one offers Reload, which closes the editor and reads the lists again.
  const staleCallout = (message: string, onReload: (() => void) | null) => (
    <div className="callout callout--critical" role="alert">
      <IconWarning size={16} />
      <div>
        {message}
        {onReload && (
          <>
            {' '}
            <Button size="sm" variant="ghost" onClick={onReload}>
              Reload
            </Button>
          </>
        )}
      </div>
    </div>
  );

  return (
    <div className="detail">
      <header className="detail__head">
        <div className="detail__ident">
          <h2 className="detail__title">
            {editing ? (
              <span className="row">
                {/* Held while a save runs: its answer closes the editor, which would drop anything typed meanwhile. */}
                <Input value={draft} onChange={(e) => setDraft(e.target.value)} style={{ width: 200 }} autoFocus disabled={busy} />
                <Button size="sm" variant="primary" disabled={busy || draft.trim() === ''} onClick={() => void save()}>
                  Save
                </Button>
                <Button
                  size="sm"
                  variant="ghost"
                  disabled={busy}
                  onClick={() => {
                    setEditing(false);
                    if (labelRefused?.stale) onChanged();
                    setLabelRefused(null);
                  }}
                >
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
                    setDraft(current.label);
                    setLabelRevision(current.revision);
                    setLabelRefused(null);
                    setEditing(true);
                  }}
                >
                  <IconPencil size={14} />
                </button>
              </>
            )}
          </h2>
          {/* Under the title rather than in it: an h2 holds inline content alone. */}
          {editing &&
            labelRefused &&
            staleCallout(
              labelRefused.message,
              labelRefused.stale
                ? () => {
                    setEditing(false);
                    setLabelRefused(null);
                    onChanged();
                  }
                : null,
            )}
          {account.email && <p className="detail__sub mono">{account.email}</p>}
          <div className="detail__badges">
            <Badge tone={STATUS_TONE[account.status]} dot>
              {STATUS_LABEL[account.status]}
            </Badge>
            {account.credential.quarantined && (
              <Badge tone="critical" title="the last refresh's outcome is uncertain, so re-pair or wait for the next start to resolve it">
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
                'Paused accounts stay in their pool but receive no new sessions. Pinned sessions migrate on their next request.',
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
                `Deletes ${account.label}'s credential. Pool memberships and request history stay, and re-pairing stores the credential again.`,
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
                `Removes ${account.label} from its pool and deletes it. Request history stays in the ledger.`,
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
              <Button
                size="sm"
                disabled={endForm !== null}
                onClick={() => {
                  setEndRefused(null);
                  setEndForm(subscriptionEndFormOf(current));
                }}
              >
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
                    { label: 'Type', value: 'API key' },
                    { label: 'Windows', value: 'None reported, billed per token' },
                  ]
            }
          />
          {endForm && (
            <div className="capture" style={{ marginTop: 12 }}>
              <label className="row">
                <input type="checkbox" checked={endForm.cancelled} disabled={busy} onChange={(e) => setEndForm(tickCancelled(endForm, e.target.checked))} />
                <span className="field__label">Cancelled</span>
              </label>
              <Field label="Ends" hint={END_HINT}>
                <Input type="datetime-local" value={endForm.endsAt} disabled={busy} onChange={(e) => setEndForm({ ...endForm, endsAt: e.target.value })} style={{ width: 220 }} aria-label="Ends" />
              </Field>
              {endRefusal && (
                <div className="callout callout--critical" role="alert">
                  <IconWarning size={16} />
                  <div>{endRefusal}</div>
                </div>
              )}
              {endRefused &&
                staleCallout(
                  endRefused.message,
                  endRefused.stale
                    ? () => {
                        setEndForm(null);
                        setEndRefused(null);
                        onChanged();
                      }
                    : null,
                )}
              <span className="row">
                <Button size="sm" variant="primary" disabled={busy || endRefusal !== null} onClick={() => void saveEnd(endForm)}>
                  Save
                </Button>
                <Button
                  size="sm"
                  variant="ghost"
                  disabled={busy}
                  onClick={() => {
                    setEndForm(null);
                    if (endRefused?.stale) onChanged();
                    setEndRefused(null);
                  }}
                >
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
            <WindowTriple account={account} now={now} others />
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
        <CoolingDown
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
          <Card title="Pool">
            {account.pool === null ? (
              <p className="muted">Not a member of a pool.</p>
            ) : (
              <ul className="plain-list">
                <li className="row">
                  <Link to="/pools" className="mono">
                    {account.pool.poolId}
                  </Link>
                  <span className="muted">priority {account.pool.priority}</span>
                </li>
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
          <Card title="Requests" actions={<Link to={logHref({ accounts: [account.id], ...aroundSubject(null, now) })}>Routing log</Link>}>
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

const ID_HINT = 'Lowercase letters, digits, and dashes, a letter or digit first, 64 at most. It names the account in every log and URL.';
const ID_MALFORMED = 'not an id the router takes';
// The route's rule on an id, mirrored so the common refusal is held here rather than answered after the start.
const ID_SHAPE = /^[a-z0-9][a-z0-9-]{0,63}$/;
const ID_TAKEN = 'taken, customize the id';
const ID_UNNAMED = 'none yet, the label needs a letter or digit';
// The id line reads a state after `Id:`, where the customized field's hint leads with it as a sentence of its own.
const sentence = (fragment: string) => `${fragment.charAt(0).toUpperCase()}${fragment.slice(1)}.`;

function AddAccountDialog({ open, existingIds, onClose, onDone }: { existingIds: ReadonlySet<string>; onClose: () => void; onDone: (done: { kind: 'api_key'; created: Account } | { id: string; kind: 'oauth'; label: string; session: PairingSession }) => void; open: boolean }) {
  const [kind, setKind] = useState<'api_key' | 'oauth'>('oauth');
  const [label, setLabel] = useState('');
  // Null while the id follows the label. A string once customize revealed the input, '' following the label again.
  const [customId, setCustomId] = useState<string | null>(null);
  const [secret, setSecret] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // A reopened dialog starts blank: the id it last created is taken, and the router refuses a second account under it.
  const [wasOpen, setWasOpen] = useState(open);
  if (wasOpen !== open) {
    setWasOpen(open);
    if (open) {
      setKind('oauth');
      setLabel('');
      setCustomId(null);
      setSecret('');
      setError(null);
    }
  }

  const derived = derivedId(label);
  const custom = customId?.trim() ?? '';
  const id = custom || derived.id;
  const unnamed = !custom && derived.unnamed;
  // Held before the route's 409, as the key and pool dialogs hold a taken derived id. The route stays the backstop.
  const taken = !unnamed && existingIds.has(id);
  const malformed = !unnamed && !ID_SHAPE.test(id);
  const unsaved = label.trim() !== '' || (customId ?? '') !== '' || secret !== '';
  const held = busy || !label.trim() || unnamed || taken || malformed || (kind === 'api_key' && !secret);

  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      const created = await api.createAccount({ id, kind: 'api_key', label: label.trim(), secret });
      onDone({ kind: 'api_key', created });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  // An OAuth account is written by the pairing's confirm. The start runs here rather than in the pairing dialog so a
  // refused start leaves the typed draft in reach, with the route's sentence under it.
  const start = async () => {
    setBusy(true);
    setError(null);
    try {
      const session = await api.startPairing({ id, label: label.trim() });
      onDone({ id, kind: 'oauth', label: label.trim(), session });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  const proceed = () => {
    if (kind === 'oauth') void start();
    else void create();
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title="Add account"
      subtitle="Subscription logins share your Claude.ai quota. API keys bill per token against a Console workspace."
      unsaved={unsaved}
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" disabled={held} onClick={proceed}>
            {kind === 'oauth' ? 'Start pairing' : 'Add API key'}
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
            { value: 'api_key', label: 'API key' },
          ]}
        />
      </div>
      <Field label="Label" hint="Short name used in pools and logs.">
        <Input value={label} onChange={(e) => setLabel(e.target.value)} placeholder={kind === 'oauth' ? 'home' : 'Office workspace'} />
      </Field>
      {customId === null ? (
        <p className="field__hint">
          <span className="mono">Id: {unnamed ? ID_UNNAMED : id}</span>
          {taken && ` ${ID_TAKEN}`}{' '}
          <button type="button" className="link-btn" onClick={() => setCustomId(unnamed ? '' : id)}>
            customize
          </button>
          <br />
          {ID_HINT}
        </p>
      ) : (
        <Field label="Id" hint={unnamed ? `${sentence(ID_UNNAMED)} ${ID_HINT}` : taken ? `${sentence(ID_TAKEN)} ${ID_HINT}` : malformed ? `${sentence(ID_MALFORMED)} ${ID_HINT}` : ID_HINT}>
          <Input value={customId} onChange={(e) => setCustomId(e.target.value)} placeholder={unnamed ? '' : derived.id} className="mono" aria-invalid={taken || malformed || undefined} />
        </Field>
      )}
      {kind === 'api_key' && (
        <Field label="API key">
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

type PairStep = 1 | 2;

const PAIR_STEPS: { label: string; step: PairStep }[] = [
  { step: 1, label: 'Log in and paste the address' },
  { step: 2, label: 'Confirm' },
];

/**
 * What the dialog opens on: an OAuth row to start a pairing for, or a pairing Add account already started for the row
 * its confirm creates under `id`.
 */
type PairingTarget = { accountId: string } | { id: string; session: PairingSession };

function PairingDialog({ open, start, label, repair, onClose, onPaired }: { label: string; onClose: () => void; onPaired: (p: PairingPreview) => void; open: boolean; repair: boolean; start: PairingTarget }) {
  const [session, setSession] = useState<PairingSession | null>(null);
  const [step, setStep] = useState<PairStep>(1);
  const [callback, setCallback] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [preview, setPreview] = useState<PairingPreview | null>(null);
  const now = useNow(1_000);

  // The start by value, so the placeholder the page rebuilds for the closed dialog every render restarts nothing. An
  // opening, or a new start while open, resets the steps in the render that brings it.
  const startKey = JSON.stringify(start);
  const opening = open ? startKey : null;
  const [openedFor, setOpenedFor] = useState<string | null>(null);
  if (openedFor !== opening) {
    setOpenedFor(opening);
    if (opening !== null) {
      setSession('session' in start ? start.session : null);
      setStep(1);
      setCallback('');
      setError(null);
      setPreview(null);
    }
  }
  useEffect(() => {
    if (!open) return;
    // Read back from the key the effect runs on, so the start it pairs is the one that key names.
    const target = JSON.parse(startKey) as PairingTarget;
    if ('session' in target) return;
    let alive = true;
    api.startPairing(target).then(
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
  }, [open, startKey]);

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
      setStep(2);
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
      unsaved={callback.trim() !== '' || preview !== null}
      footer={
        step === 1 ? (
          <>
            <Button variant="ghost" onClick={dismiss}>
              Cancel
            </Button>
            <Button variant="primary" disabled={!session || !callback.trim() || busy || expired} onClick={submit}>
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
          <p className="secondary">Open the authorization link, sign in to Claude.ai, and approve the router. The browser then lands on a callback address that will not load, since nothing listens on that port. Paste the URL from the address bar, or the code alone.</p>
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
          <Field label="Paste the URL your browser landed on, or the code">
            {/* Remounted as the session lands so the autofocus runs then: the paste is the dialog's one typed step,
                and there is nothing to paste before the link exists. */}
            <Input key={session ? 'ready' : 'pending'} value={callback} onChange={(e) => setCallback(e.target.value)} placeholder={`${session?.redirectUri ?? 'http://localhost:…/callback'}?code=…&state=…`} className="mono" autoFocus={session !== null} />
          </Field>
          {session && (
            <p className="muted">
              Registered redirect: <code>{session.redirectUri}</code>
            </p>
          )}
        </div>
      )}

      {step === 2 && preview && (
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
  // The visible line ticks every second under a minute; a screen reader hears the time in whole minutes alone, from the
  // region, so the line is hidden from it rather than read beside a region it disagrees with mid-minute.
  const minutes = Math.ceil(remaining / 60_000);
  return (
    <>
      {expired ? (
        <span className="pairing__expired">This link has expired. Close the dialog and start again.</span>
      ) : (
        <span className="muted num" aria-hidden="true">
          Link expires in {fmtDuration(remaining)}
        </span>
      )}
      <span className="sr-only" aria-live="polite">
        {expired ? 'This link has expired.' : `Link expires in ${minutes} ${minutes === 1 ? 'minute' : 'minutes'}`}
      </span>
    </>
  );
}
