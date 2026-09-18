import { useEffect, useState, type ReactNode } from 'react';
import { api } from '@/api';
import type { Account, AlertEvent, AlertEventDetail, AlertKind, AlertRule, AlertRuleInput, ApiKey, AskUserQuestionDetail, ClientDriftDetail, Delivery, Pool, WindowId } from '@/api/types';
import { useAppSettings } from '@/app/settings';
import { Badge, Button, Card, ConfirmDialog, Dialog, Field, Input, Menu, Select, Skeleton, Toast, Toggle } from '@/components/ui/primitives';
import { IconPlus } from '@/components/ui/icons';
import { fmtAbsolute, fmtPct, fmtRelative, shortId, urlHost } from '@/lib/format';
import { useAsync, useNow } from '@/lib/hooks';
import { WINDOW_LABEL } from '@/lib/windows';

const KIND_LABEL: Record<AlertKind, string> = {
  ask_user_question: 'Waiting for user input',
  client_drift: 'Client version drift',
  funding_required: 'Funding required',
  keepalive_cost_cap: 'Probe cost cap reached',
  pool_exhausted: 'Pool exhausted',
  reauth_required: 'Re-auth required',
  refresh_failed: 'Credential refresh failed',
  utilization_above: 'Utilization above threshold',
};
const KINDS = Object.keys(KIND_LABEL).sort() as AlertKind[];
const KIND_HINT: Record<AlertKind, string> = {
  ask_user_question:
    "Fires when a response relayed to a client carries an AskUserQuestion tool call, so the agent is blocked on an answer. It resolves on the session's next request, which is the first sign the user replied, or when the session expires.",
  client_drift:
    'Fires when the billing-hash self-check fails for a client request: the client stamps a hash this build cannot reproduce. Re-pin the router to that client version, then resolve the event here.',
  funding_required: 'Fires when an API-key upstream is refused for billing reasons. It resolves when an operator clears the funding flag.',
  keepalive_cost_cap: "Fires when a pool's daily probe cost cap is reached; probes pause until the next UTC day, which resolves it.",
  pool_exhausted: 'Fires when a request finds no eligible upstream in the pool. It resolves on the next request the pool serves.',
  reauth_required: 'Fires when an upstream enters reauth_required. It resolves once the upstream is paired or verified again.',
  refresh_failed: 'Fires when a credential refresh fails twice in a row. It resolves on the next refresh that succeeds.',
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
  pool_exhausted: ['all', 'pool'],
  reauth_required: ['any_account', 'account'],
  refresh_failed: ['any_account', 'account'],
  utilization_above: ['all', 'any_account', 'account', 'key', 'pool'],
};
/** A 2xx is the only answer the deliverer counts as landed; everything else, including no answer at all, reads as a failure. */
const deliveryTone = (d: Delivery) => (d.status !== null && d.status < 300 ? 'good' : 'critical');
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
  const [toast, setToast] = useState<string | null>(null);
  const now = useNow();
  const named: Named = { accounts: accounts.data ?? [], keys: keys.data ?? [], pools: pools.data ?? [] };
  const shown = events.data ? events.data.filter((e) => kindFilter === 'all' || e.kind === kindFilter) : null;

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
        subtitle="Each rule posts a signed JSON payload to its webhook when the condition opens and again when it resolves."
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
                {KINDS.map((k) => (
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
                        body: `Marks "${e.subject}" resolved and posts the resolve delivery.${e.kind === 'client_drift' ? ' Use this after re-pinning the router to the client version the event names.' : ''}`,
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
        {rule.threshold !== null ? `${fmtPct(rule.threshold)} on ${rule.windowId ? ruleWindowLabel(rule.windowId) : 'n/a'}` : <span className="muted">-</span>}
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
        {event.detail && <EventDetail kind={event.kind} detail={event.detail} />}
      </td>
      <td title={fmtAbsolute(event.openedAt)}>{fmtRelative(event.openedAt, now)}</td>
      <td title={event.resolvedAt ? fmtAbsolute(event.resolvedAt) : undefined}>{event.resolvedAt ? fmtRelative(event.resolvedAt, now) : <span className="muted">-</span>}</td>
      <td>
        <ul className="deliveries">
          {event.deliveries.map((d, i) => (
            <li key={i} title={fmtAbsolute(d.at)}>
              {/* The deliverer counts attempts per run, so the transition is what tells two `#1` rows apart. */}
              <span className="muted">{d.state}</span>
              <span className="muted">#{d.attempt}</span>
              <Badge tone={deliveryTone(d)}>{d.status ?? 'no answer'}</Badge>
              {d.error && <span className="muted">{d.error}</span>}
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

// A detail is the free-form object the signal built, so the event's kind says which keys it holds. The two kinds
// below read as a sentence; every other kind is listed as it arrived, and so is a detail missing the keys its kind
// promises, so a shape a later build changes stays readable.
function EventDetail({ kind, detail }: { detail: AlertEventDetail; kind: AlertKind }) {
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
    return (
      <>
        <span className="sub mono" title={asked.session}>
          key {asked.key}, session {shortId(asked.session, 8)}
          {asked.agent && `, agent ${asked.agent}`}
        </span>
        {asked.question ? <span className="sub">asked: {asked.question}</span> : <span className="sub muted">question not captured</span>}
      </>
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
  const [windowId, setWindowId] = useState<RuleWindow>('7d');
  const [url, setUrl] = useState('');
  const [secret, setSecret] = useState('');
  const [enabled, setEnabled] = useState(true);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setKind(existing?.kind ?? 'utilization_above');
    setScope(existing ? (existing.scope.id ? `${existing.scope.type}:${existing.scope.id}` : existing.scope.type) : 'all');
    setThreshold(existing?.threshold !== null && existing?.threshold !== undefined ? Math.round(existing.threshold * 100) : 90);
    setWindowId(existing?.windowId ?? '7d');
    setUrl(existing?.webhookUrl ?? '');
    // Never prefilled: the router answers whether a rule is signed, never with what.
    setSecret('');
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

  const submit = async () => {
    setBusy(true);
    const input: AlertRuleInput = {
      enabled,
      kind,
      scope: parseScope(effectiveScope),
      secret: secret === '' ? (existing ? undefined : '') : secret,
      threshold: kind === 'utilization_above' ? threshold / 100 : null,
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
      <Field label="Webhook URL">
        <Input placeholder="https://hooks.example.com/phoebis" value={url} onChange={(e) => setUrl(e.target.value)} />
      </Field>
      <Field
        label="Signing secret"
        hint={`Optional. Sent as X-Router-Signature, an HMAC-SHA256 of the body.${existing ? ' Leave empty to keep the stored secret; a new value replaces it.' : ''}`}
      >
        <Input type="password" autoComplete="off" value={secret} onChange={(e) => setSecret(e.target.value)} />
      </Field>
      <div className="field--inline field">
        <span className="field__text">
          <span className="field__label">Enabled</span>
        </span>
        <Toggle checked={enabled} onChange={setEnabled} label="Enabled" />
      </div>
    </Dialog>
  );
}
