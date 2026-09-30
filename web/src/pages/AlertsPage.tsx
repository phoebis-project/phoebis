import { useEffect, useRef, useState, type ReactNode } from 'react';
import { api } from '@/api';
import type { Account, AlertEvent, AlertEventDetail, AlertKind, AlertRule, AlertRuleInput, ApiKey, AskUserQuestionDetail, ClientDriftDetail, LifecycleDelivery, LifecycleEventName, PollBackoffDetail, Pool, RefreshFailedDetail, WindowId } from '@/api/types';
import { useAppSettings } from '@/app/settings';
import { Badge, Button, Card, ConfirmDialog, Dialog, ErrorState, Field, Input, Menu, Select, Skeleton, Toast, Toggle } from '@/components/ui/primitives';
import { IconPlus } from '@/components/ui/icons';
import { questionLine } from '@/lib/alerts';
import { fmtAbsolute, fmtPct, fmtRelative, shortId, urlHost } from '@/lib/format';
import { useAsync, useNow } from '@/lib/hooks';
import { WINDOW_LABEL } from '@/lib/windows';

const KIND_LABEL: Record<AlertKind, string> = {
  ask_user_question: 'Waiting for user input',
  client_drift: 'Client drift',
  funding_required: 'Funding required',
  keepalive_cost_cap: 'Probe cost cap reached',
  poll_backoff: 'Usage poll backing off',
  pool_exhausted: 'Pool exhausted',
  reauth_required: 'Re-auth required',
  refresh_failed: 'Credential refresh failed',
  session_active: 'Session active',
  session_idle: 'Session idle',
  session_parked: 'Session parked',
  subagent_active: 'Subagent active',
  utilization_above: 'Utilization above threshold',
};
const KINDS = Object.keys(KIND_LABEL).sort() as AlertKind[];
const KIND_HINT: Record<AlertKind, string> = {
  ask_user_question:
    "Fires when a response relayed to a client carries an AskUserQuestion tool call, so the agent is blocked on an answer. It resolves on the session's next request, which is the first sign the user replied, or when the session expires.",
  client_drift:
    'Fires when the billing-hash self-check fails for a client request: the client stamps a hash this build cannot reproduce. Re-pin the router to that client build, then resolve the event here.',
  funding_required: 'Fires when an API-key upstream is refused for billing reasons. It resolves when an operator clears the funding flag.',
  keepalive_cost_cap: "Fires when a pool's daily probe cost cap is reached; probes pause until the next UTC day, which resolves it.",
  poll_backoff:
    "Fires when an OAuth upstream's usage poll has been backing off from the endpoint's 429 answers for thirty minutes. It resolves on the upstream's next clean poll.",
  pool_exhausted: 'Fires when a request finds no eligible upstream in the pool. It resolves on the next request the pool serves.',
  reauth_required: 'Fires when an upstream enters reauth_required. It resolves once the upstream is paired or verified again.',
  refresh_failed: 'Fires when a credential refresh fails twice in a row. It resolves on the next refresh that succeeds.',
  session_active: "Posts session_started on a session's first request, or on the companion's session_start, and session_ended when the companion ends the session or it expires unseen; one delivery per event, nothing to resolve.",
  session_idle: 'Posts session_idle once no request has arrived on the session for the number of minutes chosen here; the next request arms it again.',
  session_parked: 'Posts session_parked when the keepalive companion reports the main agent parked and session_resumed when it is busy or idle again; a session without a companion never parks.',
  subagent_active: "Posts subagent_started on a subagent's first request, or on the companion's agent_start, and subagent_ended when the agent is released, the session ends, or it expires.",
  utilization_above: 'Fires when a usage sample for the chosen window reaches the threshold. It resolves once a sample reads five points below it.',
};
type RuleWindow = NonNullable<AlertRule['windowId']>;
/** A rule names the header sampler's `7d_oi`; the shared label map is keyed by the window id the pages draw. */
const RULE_WINDOWS: Record<RuleWindow, WindowId> = { '5h': '5h', '7d': '7d', '7d_oi': '7d_fable' };
/** Shortest window first, the order every other page draws them in. */
const RULE_WINDOW_IDS: RuleWindow[] = ['5h', '7d', '7d_oi'];
const ruleWindowLabel = (w: RuleWindow) => WINDOW_LABEL[RULE_WINDOWS[w]];
// Broadest scope first, so the option a kind is usually written with is the one the form opens on.
const ALLOWED_SCOPES: Record<AlertKind, AlertRule['scope']['type'][]> = {
  ask_user_question: ['all', 'key', 'pool'],
  client_drift: ['all'],
  funding_required: ['any_account', 'account'],
  keepalive_cost_cap: ['all', 'pool'],
  poll_backoff: ['any_account', 'account'],
  pool_exhausted: ['all', 'pool'],
  reauth_required: ['any_account', 'account'],
  refresh_failed: ['any_account', 'account'],
  session_active: ['all', 'key', 'pool'],
  session_idle: ['all', 'key', 'pool'],
  session_parked: ['all', 'key', 'pool'],
  subagent_active: ['all', 'key', 'pool'],
  utilization_above: ['all', 'any_account', 'account', 'key', 'pool'],
};
/** The kinds that post one delivery per session event and open none, so they are read in the log, not the events table. */
const LIFECYCLE_KINDS: AlertKind[] = ['session_active', 'session_idle', 'session_parked', 'subagent_active'];
const EVENT_LABEL: Record<LifecycleEventName, string> = {
  session_ended: 'Session ended',
  session_idle: 'Session idle',
  session_parked: 'Session parked',
  session_resumed: 'Session resumed',
  session_started: 'Session started',
  subagent_ended: 'Subagent ended',
  subagent_started: 'Subagent started',
};
const deliveryTone = (d: { status: number | null }) => (d.status !== null && d.status < 300 ? 'good' : 'critical');
const TRIGGER_LABEL: Record<string, string> = { hash: 'the billing-hash check' };
/** A trigger this build does not know still reads as itself rather than as a gap in the sentence. */
const triggerLabel = (trigger: string) => TRIGGER_LABEL[trigger] ?? `the ${trigger} check`;

/** The lists a scope's id is resolved against, so a rule names an upstream, a key, or a pool the way its own page does. */
interface Named {
  accounts: Account[];
  keys: ApiKey[];
  pools: Pool[];
}

function scopeLabel(scope: AlertRule['scope'], named: Named): string {
  switch (scope.type) {
    case 'account':
      return `Upstream ${named.accounts.find((a) => a.id === scope.id)?.label ?? scope.id}`;
    case 'all':
      return 'All traffic';
    case 'any_account':
      return 'Any upstream';
    case 'key':
      return `Key ${named.keys.find((k) => k.id === scope.id)?.name ?? scope.id}`;
    case 'pool':
      return `Pool ${named.pools.find((p) => p.id === scope.id)?.name ?? scope.id}`;
  }
}

/** The scope select carries `type` alone for the two that take no id, and `type:id` for the three that need one. */
const parseScope = (value: string): AlertRule['scope'] => {
  const [type, id] = value.split(':');
  return { id: id ?? null, type: type as AlertRule['scope']['type'] };
};

export function AlertsPage() {
  const rules = useAsync(() => api.listAlertRules(), []);
  const [stateFilter, setStateFilter] = useState<'all' | 'open' | 'resolved'>('all');
  const events = useAsync(() => api.listAlertEvents(stateFilter === 'all' ? undefined : stateFilter), [stateFilter]);
  const accounts = useAsync(() => api.listAccounts(), []);
  const keys = useAsync(() => api.listKeys(), []);
  const pools = useAsync(() => api.listPools(), []);
  const { canMutate } = useAppSettings();
  const [dialog, setDialog] = useState<{ rule: AlertRule | null } | null>(null);
  const [confirm, setConfirm] = useState<{ body: string; danger?: boolean; label: string; onConfirm: () => void; title: string } | null>(null);
  const [kindFilter, setKindFilter] = useState<AlertKind | 'all'>('all');
  const [lifecycleKind, setLifecycleKind] = useState<AlertKind | 'all'>('all');
  const lifecycle = useAsync(() => api.listLifecycleDeliveries(lifecycleKind === 'all' ? undefined : lifecycleKind), [lifecycleKind]);
  const moreGeneration = useRef(0);
  // `nextCursor` stays `undefined` until a load-more reply lands in this generation; a page can be exactly full
  // with nothing beyond it, so the route answers a cursor whenever the page is full, not only while rows remain
  // (`crates/store/src/alerts.rs`), and a landed `null` must win over the first page's cursor rather than fall
  // back to it forever.
  const [more, setMore] = useState<{ generation: number; loading: boolean; nextCursor: string | null | undefined; rows: LifecycleDelivery[] }>({ generation: 0, loading: false, nextCursor: undefined, rows: [] });
  useEffect(() => {
    moreGeneration.current += 1;
    setMore({ generation: moreGeneration.current, loading: false, nextCursor: undefined, rows: [] });
  }, [lifecycleKind]);
  const lifecycleCursor = more.nextCursor !== undefined ? more.nextCursor : (lifecycle.data?.nextCursor ?? null);
  const [toast, setToast] = useState<string | null>(null);
  const now = useNow();
  const named: Named = { accounts: accounts.data ?? [], keys: keys.data ?? [], pools: pools.data ?? [] };
  const shown = events.data ? events.data.filter((e) => !LIFECYCLE_KINDS.includes(e.kind) && (kindFilter === 'all' || e.kind === kindFilter)) : null;

  /** Every mutation on this page reports the same way: the outcome in a toast, then a re-read of the list it changed. */
  const run = async (action: () => Promise<unknown>, message: string, reload: () => void) => {
    try {
      await action();
      setToast(message);
      reload();
    } catch (e) {
      setToast(e instanceof Error ? e.message : String(e));
    }
  };

  const loadMoreLifecycle = async () => {
    const generation = more.generation;
    const cursor = lifecycleCursor;
    if (cursor === null) return;
    setMore((m) => ({ ...m, loading: true }));
    try {
      const page = await api.listLifecycleDeliveries(lifecycleKind === 'all' ? undefined : lifecycleKind, cursor);
      // A reply for a kind the operator has already moved past would misattribute its rows and cursor to the new one.
      setMore((m) => (m.generation === generation ? { generation, loading: false, nextCursor: page.nextCursor, rows: [...m.rows, ...page.deliveries] } : m));
    } catch (e) {
      setMore((m) => (m.generation === generation ? { ...m, loading: false } : m));
      setToast(e instanceof Error ? e.message : String(e));
    }
  };

  const test = async (rule: AlertRule) => {
    try {
      const result = await api.testAlertRule(rule.id);
      setToast(result.status === null ? `Test delivery failed: ${result.error ?? 'no answer'}` : `Test delivery answered ${result.status}`);
    } catch (e) {
      setToast(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <div className="stack">
      <Card
        title="Rules"
        subtitle="A rule posts a signed JSON payload to its webhook when its condition opens and resolves, or, for the four session kinds, once per session event."
        actions={canMutate && (
          <Button variant="primary" size="sm" icon={<IconPlus size={14} />} onClick={() => setDialog({ rule: null })}>
            Create rule
          </Button>
        )}
        padded={false}
        className="card--table"
      >
        <div className="table-wrap">
          <table className="table">
            <thead>
              <tr>
                <th>Condition</th>
                <th>Scope</th>
                <th>Threshold</th>
                <th>Webhook</th>
                <th>Secret</th>
                <th>State</th>
                {canMutate && <th />}
              </tr>
            </thead>
            <tbody>
              {rules.data
                ? rules.data.map((rule) => (
                    <RuleRow
                      key={rule.id}
                      rule={rule}
                      // A webhook URL may carry its token in the path, and a viewer may not read one.
                      webhook={canMutate ? rule.webhookUrl : urlHost(rule.webhookUrl)}
                      scope={scopeLabel(rule.scope, named)}
                      actions={
                        canMutate ? (
                          <Menu
                            label={`Actions for rule ${rule.id}`}
                            items={[
                              { label: 'Edit', onClick: () => setDialog({ rule }) },
                              { label: 'Test', onClick: () => void test(rule) },
                              {
                                label: 'Delete',
                                danger: true,
                                onClick: () =>
                                  setConfirm({
                                    title: 'Delete rule',
                                    body: `The ${KIND_LABEL[rule.kind]} rule for ${scopeLabel(rule.scope, named)} stops firing and its events are removed with it.`,
                                    label: 'Delete',
                                    danger: true,
                                    onConfirm: () => void run(() => api.deleteAlertRule(rule.id), 'Rule deleted', rules.reload),
                                  }),
                              },
                            ]}
                          />
                        ) : null
                      }
                    />
                  ))
                : rules.error
                  ? (
                      <tr>
                        <td colSpan={canMutate ? 7 : 6}>
                          <ErrorState title="Could not load the alert rules" body={rules.error} action={<Button size="sm" onClick={rules.reload}>Try again</Button>} />
                        </td>
                      </tr>
                    )
                  : [0, 1, 2].map((i) => (
                      <tr key={i}>
                        <td colSpan={canMutate ? 7 : 6}>
                          <Skeleton />
                        </td>
                      </tr>
                    ))}
            </tbody>
          </table>
        </div>
      </Card>

      <Card
        title="Events"
        subtitle="Open events stay listed until the condition clears."
        actions={
          <>
            <label className="row">
              <span className="muted">Condition</span>
              <Select value={kindFilter} onChange={(e) => setKindFilter(e.target.value as AlertKind | 'all')} aria-label="Filter by condition">
                <option value="all">Any condition</option>
                {KINDS.filter((k) => !LIFECYCLE_KINDS.includes(k)).map((k) => (
                  <option key={k} value={k}>
                    {KIND_LABEL[k]}
                  </option>
                ))}
              </Select>
            </label>
            <label className="row">
              <span className="muted">State</span>
              <Select value={stateFilter} onChange={(e) => setStateFilter(e.target.value as 'all' | 'open' | 'resolved')} aria-label="Filter by state">
                <option value="all">Any state</option>
                <option value="open">Open</option>
                <option value="resolved">Resolved</option>
              </Select>
            </label>
          </>
        }
        padded={false}
        className="card--table"
      >
        <div className="table-wrap">
          <table className="table">
            <thead>
              <tr>
                <th>State</th>
                <th>Condition</th>
                <th>Subject</th>
                <th>Opened</th>
                <th>Resolved</th>
                <th>Deliveries</th>
              </tr>
            </thead>
            <tbody>
              {shown === null ? (
                [0, 1, 2, 3].map((i) => (
                  <tr key={i}>
                    <td colSpan={6}>
                      <Skeleton />
                    </td>
                  </tr>
                ))
              ) : shown.length === 0 ? (
                <tr>
                  <td colSpan={6} className="muted">
                    {kindFilter === 'all' ? 'No events.' : `No ${KIND_LABEL[kindFilter].toLowerCase()} events.`}
                  </td>
                </tr>
              ) : (
                shown.map((e) => (
                  <EventRow
                    key={e.id}
                    event={e}
                    now={now}
                    canMutate={canMutate}
                    onResolve={() =>
                      setConfirm({
                        title: 'Resolve event',
                        body: `Marks "${e.subject}" resolved and posts the resolve delivery.${e.kind === 'client_drift' ? ' Use this after re-pinning the router to the client build the event names.' : ''}`,
                        label: 'Resolve',
                        onConfirm: () => void run(() => api.resolveAlertEvent(e.id), 'Event resolved', events.reload),
                      })
                    }
                  />
                ))
              )}
            </tbody>
          </table>
        </div>
      </Card>

      <Card
        title="Lifecycle"
        subtitle="Session events as they were posted, newest first; a session rule opens nothing and has nothing to resolve."
        actions={
          <label className="row">
            <span className="muted">Condition</span>
            <Select
              value={lifecycleKind}
              onChange={(e) => setLifecycleKind(e.target.value as AlertKind | 'all')}
              aria-label="Filter lifecycle by condition"
            >
              <option value="all">Any session condition</option>
              {LIFECYCLE_KINDS.map((k) => (
                <option key={k} value={k}>
                  {KIND_LABEL[k]}
                </option>
              ))}
            </Select>
          </label>
        }
        padded={false}
        className="card--table"
      >
        <div className="table-wrap">
          <table className="table">
            <thead>
              <tr>
                <th>Time</th>
                <th>Event</th>
                <th>Session</th>
                <th>Agent</th>
                <th>Rule</th>
                <th>Delivery</th>
              </tr>
            </thead>
            <tbody>
              {lifecycle.error ? (
                <tr>
                  <td colSpan={6}>
                    <ErrorState
                      title="Could not load the lifecycle log"
                      body={lifecycle.error}
                      action={
                        <Button size="sm" onClick={lifecycle.reload}>
                          Try again
                        </Button>
                      }
                    />
                  </td>
                </tr>
              ) : lifecycle.data === null ? (
                [0, 1, 2].map((i) => (
                  <tr key={i}>
                    <td colSpan={6}>
                      <Skeleton />
                    </td>
                  </tr>
                ))
              ) : lifecycle.data.deliveries.length === 0 ? (
                <tr>
                  <td colSpan={6} className="muted">
                    {lifecycleKind === 'all' ? 'No session events.' : `No ${KIND_LABEL[lifecycleKind].toLowerCase()} events.`}
                  </td>
                </tr>
              ) : (
                [...lifecycle.data.deliveries, ...more.rows].map((d) => <LifecycleRow key={d.id} delivery={d} now={now} />)
              )}
            </tbody>
          </table>
        </div>
        {lifecycleCursor !== null && (
          <footer className="pager">
            <Button size="sm" onClick={() => void loadMoreLifecycle()} disabled={lifecycle.data === null || lifecycle.stale || more.loading}>
              Load more
            </Button>
          </footer>
        )}
      </Card>

      <RuleDialog
        open={dialog !== null}
        existing={dialog?.rule ?? null}
        accounts={named.accounts}
        keys={named.keys}
        pools={named.pools}
        onClose={() => setDialog(null)}
        onSaved={(message) => {
          setDialog(null);
          setToast(message);
          rules.reload();
        }}
        onError={setToast}
      />
      <ConfirmDialog
        open={confirm !== null}
        onClose={() => setConfirm(null)}
        title={confirm?.title ?? ''}
        body={confirm?.body ?? ''}
        confirmLabel={confirm?.label}
        danger={confirm?.danger}
        onConfirm={() => confirm?.onConfirm()}
      />
      <Toast message={toast} onDone={() => setToast(null)} />
    </div>
  );
}

function RuleRow({ rule, scope, webhook, actions }: { actions: ReactNode; rule: AlertRule; scope: string; webhook: string }) {
  return (
    <tr>
      <td>{KIND_LABEL[rule.kind]}</td>
      <td>{scope}</td>
      <td className="num">
        {rule.threshold === null ? (
          <span className="muted">-</span>
        ) : rule.kind === 'session_idle' ? (
          // The idle rule reuses the threshold column as a count of minutes, so a percentage would misread it.
          `${rule.threshold} min idle`
        ) : (
          `${fmtPct(rule.threshold)} on ${rule.windowId ? ruleWindowLabel(rule.windowId) : 'n/a'}`
        )}
      </td>
      <td className="mono">{webhook}</td>
      <td>{rule.hasSecret ? <Badge tone="good">Signed</Badge> : <Badge tone="warning">Unsigned</Badge>}</td>
      <td>
        <Badge tone={rule.enabled ? 'good' : 'neutral'} dot>
          {rule.enabled ? 'Enabled' : 'Disabled'}
        </Badge>
      </td>
      {actions && <td className="right">{actions}</td>}
    </tr>
  );
}

function EventRow({ event, now, canMutate, onResolve }: { canMutate: boolean; event: AlertEvent; now: number; onResolve: () => void }) {
  return (
    <tr>
      <td>
        <Badge tone={event.state === 'open' ? 'critical' : 'neutral'} dot>
          {event.state}
        </Badge>
      </td>
      <td>{KIND_LABEL[event.kind]}</td>
      <td>
        {event.subject}
        {event.detail && <EventDetail kind={event.kind} detail={event.detail} now={now} />}
      </td>
      <td title={fmtAbsolute(event.openedAt)}>{fmtRelative(event.openedAt, now)}</td>
      <td title={event.resolvedAt ? fmtAbsolute(event.resolvedAt) : undefined}>{event.resolvedAt ? fmtRelative(event.resolvedAt, now) : <span className="muted">-</span>}</td>
      <td>
        <ul className="deliveries">
          {event.deliveries.map((d, i) => (
            <li key={i} title={fmtAbsolute(d.at)}>
              {/* The deliverer counts attempts per run, so the transition is what tells two `#1` rows apart. */}
              <span className="muted">{d.state}</span>
              <DeliveryCell attempt={d.attempt} error={d.error} status={d.status} />
            </li>
          ))}
        </ul>
        {canMutate && event.state === 'open' && (
          <Button size="sm" onClick={onResolve}>
            Resolve
          </Button>
        )}
      </td>
    </tr>
  );
}

/**
 * One attempt's outcome, drawn the same in the events list and in the lifecycle log. A 2xx is the only answer
 * the deliverer counts as landed; everything else, including no answer at all, reads as a failure.
 */
function DeliveryCell({ attempt, error, status }: { attempt: number; error: string | null; status: number | null }) {
  return (
    <>
      <span className="muted">#{attempt}</span>{' '}
      <Badge tone={deliveryTone({ status })}>{status ?? 'no answer'}</Badge>{' '}
      {error && <span className="muted">{error}</span>}
    </>
  );
}

function LifecycleRow({ delivery, now }: { delivery: LifecycleDelivery; now: number }) {
  const { detail } = delivery;
  return (
    <tr>
      <td title={fmtAbsolute(delivery.at)}>{fmtRelative(delivery.at, now)}</td>
      <td>
        {EVENT_LABEL[delivery.event]}
        {delivery.event === 'session_idle' && <span className="sub">{typeof detail.idle_minutes === 'number' ? `idle for ${detail.idle_minutes} min` : 'idle'}</span>}
        {(delivery.event === 'session_started' || delivery.event === 'session_ended') && detail.source && <span className="sub">{detail.source}</span>}
      </td>
      <td className="mono" title={detail.session}>
        {detail.key}, {shortId(detail.session ?? '-', 8)}
      </td>
      <td className="mono">
        {detail.agent ?? 'main'}
        {delivery.event.startsWith('subagent') && <span className="sub">parent {detail.parent_agent ?? '-'}</span>}
      </td>
      <td>{KIND_LABEL[delivery.kind]}</td>
      <td>
        <DeliveryCell attempt={delivery.attempt} error={delivery.error} status={delivery.status} />
      </td>
    </tr>
  );
}

// A detail is the free-form object the signal built, so the event's kind says which keys it holds. The kinds
// below read as a sentence; every other kind is listed as it arrived, and so is a detail missing the keys its kind
// promises, so a shape a later build changes stays readable.
function EventDetail({ kind, detail, now }: { detail: AlertEventDetail; kind: AlertKind; now: number }) {
  if (detail === null) return null;
  if (kind === 'client_drift' && typeof detail.client_version === 'string') {
    const drift = detail as unknown as ClientDriftDetail;
    return (
      <span className="sub">
        client {drift.client_version}, caught by {triggerLabel(drift.trigger)}
      </span>
    );
  }
  if (kind === 'ask_user_question' && typeof detail.session === 'string') {
    const asked = detail as unknown as AskUserQuestionDetail;
    const line = questionLine(asked, now);
    return (
      <>
        <span className="sub mono" title={asked.session}>
          key {asked.key}, session {shortId(asked.session, 8)}
          {asked.agent && `, agent ${asked.agent}`}
        </span>
        <span className={line.muted ? 'sub muted' : 'sub'}>{line.text}</span>
      </>
    );
  }
  if (kind === 'poll_backoff' && typeof detail.backoff_since === 'number') {
    const backoff = detail as unknown as PollBackoffDetail;
    const at = (ms: number) => fmtAbsolute(new Date(ms).toISOString());
    // The detail is what the poll said when the event opened, so both instants read as fixed times.
    return (
      <span className="sub" title={`${backoff.rate_limited_in_a_row} rate-limited ${backoff.rate_limited_in_a_row === 1 ? 'answer' : 'answers'} in a row`}>
        backing off since {at(backoff.backoff_since)}, next attempt {backoff.next_attempt_at === null ? 'at its next poll' : `at ${at(backoff.next_attempt_at)}`}
      </span>
    );
  }
  if (kind === 'refresh_failed' && typeof detail.failures === 'number') {
    const refresh = detail as unknown as RefreshFailedDetail;
    return (
      <span className="sub">
        {refresh.failures} in a row, the last {refresh.outcome === 'uncertain' ? 'lost its answer' : 'refused'}
        {refresh.last_error ? `: ${refresh.last_error}` : ''}
      </span>
    );
  }
  return (
    <>
      {Object.entries(detail).map(([key, value]) => (
        <span className="sub" key={key}>
          {key}: {value === null ? '-' : String(value)}
        </span>
      ))}
    </>
  );
}

/**
 * Create or edit an alert rule. `existing` switches the dialog to edit mode, where an empty secret field leaves
 * the stored one alone. A refusal is reported without closing, so the form it was written in can be corrected.
 */
function RuleDialog({
  open,
  existing,
  accounts,
  keys,
  pools,
  onClose,
  onSaved,
  onError,
}: {
  accounts: Account[];
  existing: AlertRule | null;
  keys: ApiKey[];
  onClose: () => void;
  onError: (message: string) => void;
  onSaved: (message: string) => void;
  open: boolean;
  pools: Pool[];
}) {
  const [kind, setKind] = useState<AlertKind>('utilization_above');
  const [scope, setScope] = useState('all');
  const [threshold, setThreshold] = useState(90);
  const [idleMinutes, setIdleMinutes] = useState(30);
  const [windowId, setWindowId] = useState<RuleWindow>('7d');
  const [url, setUrl] = useState('');
  const [secret, setSecret] = useState('');
  const [clearSecret, setClearSecret] = useState(false);
  const [enabled, setEnabled] = useState(true);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setKind(existing?.kind ?? 'utilization_above');
    setScope(existing ? (existing.scope.id ? `${existing.scope.type}:${existing.scope.id}` : existing.scope.type) : 'all');
    // Two kinds share the threshold column, one as a fraction and one as minutes, so each field reads it only
    // for its own kind; a rule edited into the other kind would otherwise carry the wrong number across.
    setThreshold(existing?.kind === 'utilization_above' && existing.threshold !== null ? Math.round(existing.threshold * 100) : 90);
    setIdleMinutes(existing?.kind === 'session_idle' && existing.threshold !== null ? existing.threshold : 30);
    setWindowId(existing?.windowId ?? '7d');
    setUrl(existing?.webhookUrl ?? '');
    // Never prefilled: the router answers whether a rule is signed, never with what.
    setSecret('');
    setClearSecret(false);
    setEnabled(existing?.enabled ?? true);
    setBusy(false);
  }, [open, existing]);

  const scopeOptions = ALLOWED_SCOPES[kind].flatMap((type) => {
    if (type === 'account') return accounts.map((a) => ({ label: `Upstream ${a.label}`, value: `account:${a.id}` }));
    if (type === 'key') return keys.map((k) => ({ label: `Key ${k.name}`, value: `key:${k.id}` }));
    if (type === 'pool') return pools.map((p) => ({ label: `Pool ${p.name}`, value: `pool:${p.id}` }));
    return [{ label: scopeLabel({ id: null, type }, { accounts, keys, pools }), value: type }];
  });
  // Each kind allows its own scopes, so a pick left over from another kind falls back.
  const effectiveScope = scopeOptions.some((o) => o.value === scope) ? scope : (scopeOptions[0]?.value ?? 'all');

  const secretHint = existing?.hasSecret
    ? ' Leave empty to keep the stored secret; a new value replaces it; the box below removes it.'
    : existing
      ? ' Leave empty to keep the stored secret; a new value replaces it.'
      : '';

  const submit = async () => {
    setBusy(true);
    const input: AlertRuleInput = {
      enabled,
      kind,
      scope: parseScope(effectiveScope),
      // The route reads an absent secret as keep and an empty one as clear, so an edit sends the empty one only for the box.
      secret: clearSecret ? '' : secret === '' ? (existing ? undefined : '') : secret,
      threshold: kind === 'utilization_above' ? threshold / 100 : kind === 'session_idle' ? Math.round(idleMinutes) : null,
      webhookUrl: url.trim(),
      windowId: kind === 'utilization_above' ? windowId : null,
    };
    try {
      if (existing) {
        await api.updateAlertRule(existing.id, input);
        onSaved('Rule saved');
      } else {
        await api.createAlertRule(input);
        onSaved('Rule created');
      }
    } catch (e) {
      onError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={existing ? 'Edit alert rule' : 'Create alert rule'}
      subtitle="The webhook receives a POST with the event, the subject, and an HMAC signature when a secret is set."
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" disabled={!url || busy} onClick={() => void submit()}>
            {existing ? 'Save rule' : 'Create rule'}
          </Button>
        </>
      }
    >
      <h3 className="dialog-group__title">Rule</h3>
      <Field label="Enabled" beside>
        <Toggle checked={enabled} onChange={setEnabled} label="Enabled" />
      </Field>
      <h3 className="dialog-group__title">Condition</h3>
      <Field label="Condition" hint={KIND_HINT[kind]}>
        <Select value={kind} onChange={(e) => setKind(e.target.value as AlertKind)}>
          {KINDS.map((k) => (
            <option key={k} value={k}>
              {KIND_LABEL[k]}
            </option>
          ))}
        </Select>
      </Field>
      <Field label="Scope">
        <Select value={effectiveScope} onChange={(e) => setScope(e.target.value)} disabled={scopeOptions.length === 1}>
          {scopeOptions.map((o) => (
            <option key={o.value} value={o.value}>
              {o.label}
            </option>
          ))}
        </Select>
      </Field>
      {kind === 'utilization_above' && (
        <div className="form-row">
          <Field label="Threshold (%)">
            <Input type="number" min={1} max={100} value={threshold} onChange={(e) => setThreshold(Number(e.target.value))} />
          </Field>
          <Field label="Window">
            <Select value={windowId} onChange={(e) => setWindowId(e.target.value as RuleWindow)}>
              {RULE_WINDOW_IDS.map((w) => (
                <option key={w} value={w}>
                  {ruleWindowLabel(w)}
                </option>
              ))}
            </Select>
          </Field>
        </div>
      )}
      {kind === 'session_idle' && (
        <Field label="Idle after (minutes)">
          <Input type="number" min={1} max={10080} step={1} value={idleMinutes} onChange={(e) => setIdleMinutes(Number(e.target.value))} />
        </Field>
      )}
      <h3 className="dialog-group__title">Delivery</h3>
      <Field label="Webhook URL">
        <Input placeholder="https://hooks.example.com/phoebis" value={url} onChange={(e) => setUrl(e.target.value)} />
      </Field>
      <Field
        label="Signing secret"
        hint={`Optional. Sent as X-Router-Signature, an HMAC-SHA256 of the body.${secretHint}`}
      >
        <Input type="password" autoComplete="off" value={secret} onChange={(e) => setSecret(e.target.value)} disabled={clearSecret} />
      </Field>
      {existing?.hasSecret && (
        <label className="row">
          <input type="checkbox" checked={clearSecret} onChange={(e) => setClearSecret(e.target.checked)} />
          <span className="field__label">Remove the stored secret</span>
        </label>
      )}
    </Dialog>
  );
}
