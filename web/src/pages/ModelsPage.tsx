import { useState, type ReactNode } from 'react';
import { Link } from 'react-router-dom';
import { api } from '@/api';
import type { KeyModels, ModelEvent, ProcessedRule } from '@/api/types';
import { useAppSettings } from '@/app/settings';
import { CoolingDown, scopeLabel, type CoolingRow } from '@/components/CoolingDown';
import { Badge, Button, Card, ConfirmDialog, ErrorState, Select, Skeleton, Toast } from '@/components/ui/primitives';
import { fmtAbsolute, fmtRelative } from '@/lib/format';
import { useAsync, useNow } from '@/lib/hooks';

const RULE_COLUMNS = 4;

/** Upstream links separated by commas, each titled when a title is given. */
function upstreamLinks(ids: string[], titleOf?: (id: string) => string): ReactNode {
  return ids.map((id, i) => (
    <span key={id}>
      {i > 0 && ', '}
      <Link to={`/upstreams/${id}`} title={titleOf?.(id)}>
        {id}
      </Link>
    </span>
  ));
}

function subtitleOf(models: KeyModels): string {
  const mode = models.modelRulesMode === 'replace' ? "Replaces the pool's rules" : "Extends the pool's rules";
  return `${mode}; the models listing shows ${models.longContextTwins ? '' : 'no '}[1m] twins`;
}

function CoolingCell({ rule }: { rule: ProcessedRule }) {
  const ids = [...new Set(rule.cooldowns.map((c) => c.accountId))];
  if (ids.length === 0) return <span className="muted">-</span>;
  return <>{upstreamLinks(ids, (id) => rule.cooldowns.filter((c) => c.accountId === id).map((c) => `${scopeLabel(c.scope)} until ${fmtAbsolute(c.until)}`).join(', '))}</>;
}

export function ModelsPage() {
  // Both reads are taken again on the Sessions view's cadence, so a cooldown that lapses or a new rejection shows
  // without a reload.
  const tick = useNow(15_000);
  const now = useNow(5_000);
  const { canMutate } = useAppSettings();
  const keys = useAsync(() => api.listKeys(), []);
  const [chosen, setChosen] = useState<string | null>(null);
  const keyId = chosen ?? keys.data?.[0]?.id ?? null;
  const key = keys.data?.find((k) => k.id === keyId) ?? null;
  const models = useAsync(() => (keyId === null ? Promise.resolve(null) : api.getKeyModels(keyId)), [keyId, tick]);
  const state = useAsync(() => api.getModelsState(), [tick]);
  const [pending, setPending] = useState<CoolingRow | null>(null);
  const [busy, setBusy] = useState(false);
  const [toast, setToast] = useState<string | null>(null);
  // A switch leaves the previous key's answer in hand while the next one loads, and that list is never drawn under
  // this key's name.
  const processed = models.data !== null && models.data.keyId === keyId ? models.data : null;

  const onConfirmClear = () => {
    if (!pending) return;
    setBusy(true);
    void api
      .clearCooldown(pending.accountId, pending.scope)
      .then(
        () => {
          setToast('Cooldown cleared');
          state.reload();
          models.reload();
        },
        (e: unknown) => setToast(e instanceof Error ? e.message : String(e)),
      )
      .finally(() => setBusy(false));
  };

  const row = (content: ReactNode, muted = false) => (
    <tr>
      <td colSpan={RULE_COLUMNS} className={muted ? 'muted' : ''}>
        {content}
      </td>
    </tr>
  );
  const rulesBody = keys.error
    ? row(<ErrorState title="Could not load the API keys" body={keys.error} action={<Button size="sm" onClick={keys.reload}>Try again</Button>} />)
    : keys.data?.length === 0
      ? row('No API keys: a key\'s processed rules show here once one exists.', true)
      : processed === null
        ? models.error
          ? row(<ErrorState title="Could not load the key's rules" body={models.error} action={<Button size="sm" onClick={models.reload}>Try again</Button>} />)
          : row(<Skeleton />)
        : processed.rules.length === 0
          ? row("No rules: every model the pool's upstreams serve is allowed.", true)
          : processed.rules.map((r, i) => (
              <tr key={i}>
                <td>
                  <Badge tone={r.level === 'key' ? 'accent' : 'neutral'}>{r.level}</Badge>
                </td>
                <td>{r.not ? 'Deny' : 'Allow'}</td>
                <td className="mono">{r.pattern}</td>
                <td>
                  <CoolingCell rule={r} />
                </td>
              </tr>
            ));

  return (
    <div className="stack">
      <div className="page-head">
        <p className="page-head__meta">
          The rules a key's requests are judged by, the first match deciding, and the models upstreams refused: a refused model cools on that upstream alone while the request fails over.
        </p>
        {/* The select's wrapper may shrink to nothing, and a two-line description beside it would squeeze it there. */}
        <div style={{ flex: 'none' }}>
          <Select aria-label="Key" value={keyId ?? ''} disabled={!keys.data?.length} onChange={(e) => setChosen(e.target.value)}>
            {(keys.data ?? []).map((k) => (
              <option key={k.id} value={k.id}>
                {k.name === k.id ? k.id : `${k.name} (${k.id})`}
              </option>
            ))}
          </Select>
        </div>
      </div>

      <Card padded={false} className="card--table models__rules" title={key ? `Processed rules for ${key.name}` : 'Processed rules'} subtitle={processed ? subtitleOf(processed) : undefined}>
        <div className="table-wrap">
          <table className="table">
            <thead>
              <tr>
                <th>Level</th>
                <th>Rule</th>
                <th>Pattern</th>
                <th>Cooling</th>
              </tr>
            </thead>
            <tbody>{rulesBody}</tbody>
          </table>
        </div>
      </Card>

      {state.data === null && state.error ? (
        <Card>
          <ErrorState title="Could not load the model cooldowns and events" body={state.error} action={<Button size="sm" onClick={state.reload}>Try again</Button>} />
        </Card>
      ) : (
        <>
          {state.data === null ? (
            <Card title="Cooling down">
              <Skeleton height={40} />
            </Card>
          ) : state.data.cooldowns.length === 0 ? (
            <CoolingDown title="Cooling down" now={now} rows={[]} />
          ) : (
            // One block per model, so each opens on its own rows rather than keeping another model's toggle.
            state.data.cooldowns.map((g) => <CoolingDown key={g.model} title={g.model} busy={busy} now={now} rows={g.rows} showUpstream onClear={canMutate ? setPending : undefined} />)
          )}
          <RecentEvents title="Recent rejections" subtitle="The newest refusals of a model by an upstream" events={state.data?.rejections ?? null} empty="No upstream has refused a model." now={now} />
          <RecentEvents title="Recent permits" subtitle="The newest requests an upstream served after another refused the model" events={state.data?.permits ?? null} empty="No request was served after a refusal." now={now} permits />
        </>
      )}

      <ConfirmDialog
        open={pending !== null}
        onClose={() => setPending(null)}
        title="Clear cooldown"
        confirmLabel="Clear"
        body="The upstream becomes eligible immediately. If the upstream is still rate limited, the next request re-enters cooldown."
        onConfirm={onConfirmClear}
      />
      <Toast message={toast} onDone={() => setToast(null)} />
    </div>
  );
}

function RecentEvents({ empty, events, now, permits = false, subtitle, title }: { empty: string; events: ModelEvent[] | null; now: number; permits?: boolean; subtitle: string; title: string }) {
  const columns = permits ? 5 : 4;
  return (
    <Card padded={false} className="card--table" title={title} subtitle={subtitle}>
      <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th>Time</th>
              <th>Upstream</th>
              <th>Model</th>
              <th>Key</th>
              {permits && <th>Rejected by</th>}
            </tr>
          </thead>
          <tbody>
            {events === null ? (
              <tr>
                <td colSpan={columns}>
                  <Skeleton />
                </td>
              </tr>
            ) : events.length === 0 ? (
              <tr>
                <td colSpan={columns} className="muted">
                  {empty}
                </td>
              </tr>
            ) : (
              events.map((e, i) => (
                <tr key={`${e.at}-${e.accountId}-${i}`}>
                  <td title={fmtAbsolute(e.at)}>{e.requestId ? <Link to={`/requests/${e.requestId}`}>{fmtRelative(e.at, now)}</Link> : fmtRelative(e.at, now)}</td>
                  <td>{upstreamLinks([e.accountId])}</td>
                  <td>
                    <span className="mono">{e.model}</span>
                    {e.wireModel && e.wireModel !== e.model && (
                      <span className="sub">
                        sent as <span className="mono">{e.wireModel}</span>
                      </span>
                    )}
                  </td>
                  <td className="mono">{e.keyId ?? '-'}</td>
                  {permits && <td>{upstreamLinks(e.rejectedBy)}</td>}
                </tr>
              ))
            )}
          </tbody>
        </table>
      </div>
    </Card>
  );
}
