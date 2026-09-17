import { useState } from 'react';
import { api } from '@/api';
import type { AlertEvent, AlertEventDetail, AlertKind, AlertRule, ApiKey, Pool } from '@/api/types';
import { useAppSettings } from '@/app/settings';
import { Badge, Button, Card, Dialog, Field, Input, Select, Skeleton, Toast } from '@/components/ui/primitives';
import { IconPlus } from '@/components/ui/icons';
import { fmtAbsolute, fmtPct, fmtRelative, shortId } from '@/lib/format';
import { useAsync, useNow } from '@/lib/hooks';

const KIND_LABEL: Record<AlertKind, string> = {
  ask_user_question: 'Waiting for user input',
  client_drift: 'Client version drift',
  pool_exhausted: 'Pool exhausted',
  reauth_required: 'Re-auth required',
  utilization_above: 'Utilization above threshold',
};
const KINDS = Object.keys(KIND_LABEL).sort() as AlertKind[];
const KIND_HINT: Partial<Record<AlertKind, string>> = {
  ask_user_question:
    'Fires when a response relayed to a client carries an AskUserQuestion tool call, so the agent is blocked on an answer. It resolves on the session\'s next request, which is the first sign the user replied.',
  client_drift:
    'Fires when a client announces a version other than the pinned compatibility profile, or when the billing-hash check on its first-party headers disagrees with that profile. Re-pin the profile to the version the client actually sends to clear it.',
};
const TRIGGER_LABEL: Record<'billing_hash' | 'version', string> = { billing_hash: 'the billing-hash check', version: 'the version check' };

function scopeLabel(scope: AlertRule['scope']): string {
  if (scope.type === 'all') return 'All traffic';
  if (scope.type === 'any_account') return 'Any upstream';
  return `${scope.type} ${scope.id}`;
}

export function AlertsPage() {
  const rules = useAsync(() => api.listAlertRules(), []);
  const events = useAsync(() => api.listAlertEvents(), []);
  const keys = useAsync(() => api.listKeys(), []);
  const pools = useAsync(() => api.listPools(), []);
  const { canMutate } = useAppSettings();
  const [open, setOpen] = useState(false);
  const [kindFilter, setKindFilter] = useState<AlertKind | 'all'>('all');
  const [toast, setToast] = useState<string | null>(null);
  const now = useNow();
  const shown = events.data ? events.data.filter((e) => kindFilter === 'all' || e.kind === kindFilter) : null;

  return (
    <div className="stack">
      <Card
        title="Rules"
        subtitle="Each rule posts a signed JSON payload to its webhook when the condition opens and again when it resolves."
        actions={canMutate && (
          <Button variant="primary" size="sm" icon={<IconPlus size={14} />} onClick={() => setOpen(true)}>
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
              </tr>
            </thead>
            <tbody>
              {rules.data
                ? rules.data.map((r) => <RuleRow key={r.id} rule={r} />)
                : [0, 1, 2].map((i) => (
                    <tr key={i}>
                      <td colSpan={6}>
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
                    No {KIND_LABEL[kindFilter as AlertKind].toLowerCase()} events.
                  </td>
                </tr>
              ) : (
                shown.map((e) => <EventRow key={e.id} event={e} now={now} />)
              )}
            </tbody>
          </table>
        </div>
      </Card>

      <CreateRuleDialog
        open={open}
        onClose={() => setOpen(false)}
        keys={keys.data ?? []}
        pools={pools.data ?? []}
        onCreate={() => {
          setOpen(false);
          setToast('Rule created');
        }}
      />
      <Toast message={toast} onDone={() => setToast(null)} />
    </div>
  );
}

function RuleRow({ rule }: { rule: AlertRule }) {
  return (
    <tr>
      <td>{KIND_LABEL[rule.kind]}</td>
      <td>{scopeLabel(rule.scope)}</td>
      <td className="num">{rule.threshold !== null ? `${fmtPct(rule.threshold)} on ${rule.windowId}` : <span className="muted">-</span>}</td>
      <td className="mono">{rule.webhookUrl}</td>
      <td>{rule.hasSecret ? <Badge tone="good">Signed</Badge> : <Badge tone="warning">Unsigned</Badge>}</td>
      <td>
        <Badge tone={rule.enabled ? 'good' : 'neutral'} dot>
          {rule.enabled ? 'Enabled' : 'Disabled'}
        </Badge>
      </td>
    </tr>
  );
}

function EventRow({ event, now }: { event: AlertEvent; now: number }) {
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
        {event.detail && <EventDetail detail={event.detail} />}
      </td>
      <td title={fmtAbsolute(event.openedAt)}>{fmtRelative(event.openedAt, now)}</td>
      <td title={event.resolvedAt ? fmtAbsolute(event.resolvedAt) : undefined}>{event.resolvedAt ? fmtRelative(event.resolvedAt, now) : <span className="muted">-</span>}</td>
      <td>
        <ul className="deliveries">
          {event.deliveries.map((d, i) => (
            <li key={i} title={fmtAbsolute(d.at)}>
              <Badge tone={d.status === 200 ? 'good' : 'critical'}>{d.status ?? 'timeout'}</Badge>
              {d.error && <span className="muted">{d.error}</span>}
            </li>
          ))}
        </ul>
      </td>
    </tr>
  );
}

function EventDetail({ detail }: { detail: AlertEventDetail }) {
  if (detail.type === 'client_drift') {
    return (
      <span className="sub">
        client {detail.clientVersion}, caught by {TRIGGER_LABEL[detail.trigger]}
      </span>
    );
  }
  return (
    <>
      <span className="sub mono" title={detail.sessionId}>
        key {detail.keyId}, session {shortId(detail.sessionId, 8)}, agent {detail.agentId}
      </span>
      {detail.question ? <span className="sub">asked: {detail.question}</span> : <span className="sub muted">question not captured</span>}
    </>
  );
}

function CreateRuleDialog({ open, onClose, onCreate, keys, pools }: { keys: ApiKey[]; onClose: () => void; onCreate: () => void; open: boolean; pools: Pool[] }) {
  const [kind, setKind] = useState<AlertKind>('utilization_above');
  const [scope, setScope] = useState('pool:all');
  const [threshold, setThreshold] = useState(90);
  const [windowId, setWindowId] = useState<'5h' | '7d'>('7d');
  const [url, setUrl] = useState('');
  const [secret, setSecret] = useState('');
  const scopeOptions =
    kind === 'client_drift'
      ? [{ label: 'All traffic', value: 'all' }]
      : kind === 'ask_user_question'
        ? [
            { label: 'All traffic', value: 'all' },
            ...keys.map((k) => ({ label: `Key ${k.name}`, value: `key:${k.id}` })),
            ...pools.map((p) => ({ label: `Pool ${p.name}`, value: `pool:${p.id}` })),
          ]
        : [{ label: 'Any upstream', value: 'any_account' }, ...pools.map((p) => ({ label: `Pool ${p.name}`, value: `pool:${p.id}` }))];
  // Each kind allows its own scopes, so a pick left over from another kind falls back.
  const effectiveScope = scopeOptions.some((o) => o.value === scope) ? scope : scopeOptions[0].value;
  return (
    <Dialog
      open={open}
      onClose={onClose}
      title="Create alert rule"
      subtitle="The webhook receives a POST with the event, the subject, and an HMAC signature when a secret is set."
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" disabled={!url} onClick={onCreate}>
            Create rule
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
            <Select value={windowId} onChange={(e) => setWindowId(e.target.value as '5h' | '7d')}>
              <option value="5h">5-hour</option>
              <option value="7d">7-day</option>
            </Select>
          </Field>
        </div>
      )}
      <Field label="Webhook URL">
        <Input placeholder="https://hooks.example.com/phoebis" value={url} onChange={(e) => setUrl(e.target.value)} />
      </Field>
      <Field label="Signing secret" hint="Optional. Sent as X-Router-Signature, an HMAC-SHA256 of the body.">
        <Input type="password" autoComplete="off" value={secret} onChange={(e) => setSecret(e.target.value)} />
      </Field>
    </Dialog>
  );
}
