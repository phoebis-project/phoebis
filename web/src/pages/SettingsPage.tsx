import { useState } from 'react';
import { api, MOCK } from '@/api';
import type { FastPrice, ModelPrice, Settings } from '@/api/types';
import { useSession } from '@/app/session';
import { useAppSettings, type ThemeChoice } from '@/app/settings';
import { IconMonitor, IconMoon, IconSun, IconX } from '@/components/ui/icons';
import { Badge, Button, Card, ConfirmDialog, CopyButton, ErrorState, Input, KeyValue, Segmented, Skeleton, Toast, Toggle } from '@/components/ui/primitives';
import { fmtDate } from '@/lib/format';
import type { TimeFormat } from '@/lib/format';
import { useAsync } from '@/lib/hooks';
import { snippetFor } from '@/lib/snippet';

const OPTIONAL_SNIPPET = `CC_KEEPALIVE_OWNER_ID=$(uuidgen)
ANTHROPIC_CUSTOM_HEADERS="X-Keepalive-Owner-Id: $CC_KEEPALIVE_OWNER_ID"`;

const ENV_TERMS: { body: string; term: string }[] = [
  {
    term: 'ANTHROPIC_BASE_URL',
    body: "Points Claude Code at the router's proxy listener, at the URL that reaches it from the client's machine; the address under the block is where the router binds it. Every model request and the cache-keepalive companion's reports go here.",
  },
  {
    term: 'CLAUDE_CODE_OAUTH_TOKEN',
    body: 'Holds the router key. Claude Code treats it as a subscription credential, which keeps the one-hour cache TTL, the unified rate-limit headers, and the automatic continue after a usage window resets; an ANTHROPIC_API_KEY would switch those off.',
  },
  {
    term: '_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1',
    body: 'Makes Claude Code send its first-party request shape to the router: the billing hash, the cache diagnostics beta, and the first-party betas. Without it the router relays the reduced shape as is. It also switches off gateway model discovery, and being an internal variable a future build may drop it.',
  },
  {
    term: 'CLAUDE_CODE_GATEWAY_HINT_HEADERS=1',
    body: 'Makes the client send its gateway hint headers (the agent type, the compaction marks and the tool durations), which the request log and the analytics read; under the first-party flag above they arrive anyway.',
  },
  {
    term: 'DISABLE_TELEMETRY=1',
    body: "Stops the client's telemetry batches, which it posts to api.anthropic.com with the credential it holds, behind the router the key's secret.",
  },
  {
    term: 'CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1',
    body: 'Lets fast mode work behind the router: without it the client asks api.anthropic.com whether the organization has fast mode, the check fails with a router key, and fast mode stays off.',
  },
  {
    term: 'CC_KEEPALIVE_OWNER_ID and ANTHROPIC_CUSTOM_HEADERS',
    body: "Optional hardening. The router already fences cache warming by the companion's session registration; these bind each model request to the process that registered, so a second process resuming the same session id cannot inherit its recipes. They need a wrapper script, since the env block of Claude Code's settings file holds static values.",
  },
  {
    term: 'CC_KEEPALIVE_MANAGEMENT_URL and CC_KEEPALIVE_TOKEN',
    body: 'Not needed. The companion defaults them to ANTHROPIC_BASE_URL and CLAUDE_CODE_OAUTH_TOKEN.',
  },
  {
    term: 'CLAUDE_CODE_PROMPT_CACHE_TTL=5m',
    body: 'Worth setting for Fable 5.1 sessions in a pool with cache warming on: at a 0.025x read price a five-minute entry kept warm by probes stays cheaper than a one-hour entry. Leave it unset without warming.',
  },
];

/** These come from the bootstrap file and the admin API only reads them, so each says where it is set. */
const bootstrapNote = (setting: string) => `Set by ${setting} in the bootstrap file`;

type PriceField = 'cacheReadPerM' | 'cacheWrite1hPerM' | 'cacheWrite5mPerM' | 'inputPerM' | 'outputPerM';

/** Table order, which is the order a turn spends tokens in, not alphabetical. */
const PRICE_FIELDS: { key: PriceField; label: string }[] = [
  { key: 'inputPerM', label: 'Input' },
  { key: 'cacheReadPerM', label: 'Cache read' },
  { key: 'cacheWrite5mPerM', label: 'Cache write 5m' },
  { key: 'cacheWrite1hPerM', label: 'Cache write 1h' },
  { key: 'outputPerM', label: 'Output' },
];

interface PriceDraft extends Record<PriceField, string> {
  /** Stable while the model id is being typed, so an edit cannot lose its row. */
  id: string;
  model: string;
  /** Rows added by hand carry an editable model id; the ones already in the table do not. */
  isNew: boolean;
}

type Pricing = Settings['pricing'];

/** Two decimals, extended to at most four when a price needs them. */
const fmtPrice = (n: number) => `$${n.toFixed(4).replace(/(\.\d\d[1-9]?)0+$/, '$1')}`;

const toDraft = (m: ModelPrice): PriceDraft => ({
  cacheReadPerM: String(m.cacheReadPerM),
  cacheWrite1hPerM: String(m.cacheWrite1hPerM),
  cacheWrite5mPerM: String(m.cacheWrite5mPerM),
  id: m.model,
  inputPerM: String(m.inputPerM),
  isNew: false,
  model: m.model,
  outputPerM: String(m.outputPerM),
});

/**
 * `pricing.source` is `shipped`, `file` for the operator's `stats.pricing_path` table, or `edited by hand`, which
 * only the mock build's hand edit below applies; shown as it reads.
 */
function sourceLabel(source: string): string {
  return source;
}

/** The admin secret reads configured while the settings load, since no session reaches this page without one. */
function SecretBadge({ configured }: { configured: boolean }) {
  return <Badge tone={configured ? 'good' : 'warning'}>{configured ? 'configured' : 'not configured'}</Badge>;
}

export function SettingsPage() {
  const settings = useAsync(() => api.getSettings(), []);
  const { theme, setTheme, timeFormat, setTimeFormat, role, canMutate } = useAppSettings();
  const { setPreviewRole } = useSession();
  const [toast, setToast] = useState<string | null>(null);
  const s = settings.data;
  const snippet = snippetFor();

  return (
    <div className="settings">
      <Card
        title="Downstream client configuration"
        subtitle="Put it in the env block of the client's settings file or export it in the shell that launches the client, and replace the token with a key from the Keys page."
        actions={<CopyButton text={snippet} label="Copy snippet" size="md" />}
      >
        <pre className="snippet">{snippet}</pre>
        {s && (
          <p className="muted snippet__hint">
            Inference is served on <code>{s.listeners.proxy}</code>, the address the proxy listener binds. Replace the
            placeholder with the URL that reaches it: a bind address cannot name the public endpoint, since a TLS proxy,
            a port mapping or another host name can stand between Claude Code and the listener.
          </p>
        )}
        <div className="snippet-group">
          <div className="snippet-group__head">
            <h3 className="section-title">Optional</h3>
            <CopyButton text={OPTIONAL_SNIPPET} label="Copy" />
          </div>
          <pre className="snippet snippet--sm">{OPTIONAL_SNIPPET}</pre>
        </div>
        <dl className="envdl">
          {ENV_TERMS.map((t) => (
            <div className="envdl__row" key={t.term}>
              <dt>
                <code>{t.term}</code>
              </dt>
              <dd>{t.body}</dd>
            </div>
          ))}
        </dl>
      </Card>

      <PricingCard canMutate={canMutate} error={settings.error} pricing={s ? s.pricing : null} onRetry={settings.reload} onToast={setToast} />

      <Card title="Appearance" subtitle="Stored in this browser only.">
        <div className="setting-row">
          <div>
            <b>Theme</b>
            <p className="muted">System follows the operating system preference.</p>
          </div>
          <Segmented<ThemeChoice>
            ariaLabel="Theme"
            value={theme}
            onChange={setTheme}
            options={[
              { value: 'light', label: <><IconSun size={14} /> Light</> },
              { value: 'dark', label: <><IconMoon size={14} /> Dark</> },
              { value: 'system', label: <><IconMonitor size={14} /> System</> },
            ]}
          />
        </div>
        <div className="setting-row">
          <div>
            <b>Time format</b>
            <p className="muted">Applies to every timestamp and axis tick.</p>
          </div>
          <Segmented<TimeFormat>
            ariaLabel="Time format"
            value={timeFormat}
            onChange={setTimeFormat}
            options={[
              { value: '12h', label: '12h' },
              { value: '24h', label: '24h' },
            ]}
          />
        </div>
      </Card>

      <Card title="Access" subtitle="Two shared secrets, both set in the router's environment.">
        <div className="setting-row">
          <div>
            <b>Admin secret</b>
            <p className="muted">
              The secret the variable <code>admin.secret_env</code> names. Full read and write, including captured bodies.
            </p>
          </div>
          <SecretBadge configured={s?.access.adminConfigured ?? true} />
        </div>
        <div className="setting-row">
          <div>
            <b>Viewer secret</b>
            <p className="muted">
              The secret the variable <code>admin.viewer_secret_env</code> names. Reads everything except captured bodies and live previews. Cannot create, edit, or delete.
            </p>
          </div>
          <SecretBadge configured={s?.access.viewerConfigured ?? false} />
        </div>
        {MOCK && (
          <div className="setting-row setting-row--demo">
            <div>
              <b>Preview as viewer</b>
              <p className="muted">Hides every mutating control and the captured-bodies section, as the viewer secret would see it.</p>
            </div>
            <label className="row">
              <span className="secondary">{role === 'viewer' ? 'viewer' : 'admin'}</span>
              <Toggle checked={role === 'viewer'} onChange={(v) => setPreviewRole(v ? 'viewer' : 'admin')} label="Preview as viewer" />
            </label>
          </div>
        )}
      </Card>

      <Card title="About">
        {s ? (
          <>
            <KeyValue
              rows={[
                { label: 'Version', value: s.about.version, mono: true },
                { label: 'Profile', value: s.about.profile, mono: true },
                { label: 'Metrics listener', value: s.listeners.metrics, mono: true, title: 'The Prometheus scrape endpoint.' },
                { label: 'Ledger path', value: s.storage.ledgerPath, mono: true, title: bootstrapNote('stats.ledger_path') },
                { label: 'Pricing path', value: s.storage.pricingPath ?? 'not configured', mono: s.storage.pricingPath !== null, title: bootstrapNote('stats.pricing_path') },
                { label: 'Storage path', value: s.storage.path, mono: true, title: bootstrapNote('storage.path') },
              ]}
            />
            <p className="muted" style={{ marginTop: 10 }}>
              The values the configuration file sets are on the installation page; <code>phoebis check-config</code> prints this router&apos;s.
            </p>
          </>
        ) : settings.error ? (
          <ErrorState title="Could not load the settings" body={settings.error} action={<Button size="sm" onClick={settings.reload}>Try again</Button>} />
        ) : (
          <Skeleton height={60} />
        )}
      </Card>
      <Toast message={toast} onDone={() => setToast(null)} />
    </div>
  );
}

function PricingCard({ canMutate, error: failed, onRetry, onToast, pricing: served }: { canMutate: boolean; error: string | null; onRetry: () => void; onToast: (message: string) => void; pricing: Pricing | null }) {
  const [applied, setApplied] = useState<Pricing | null>(null);
  const [draft, setDraft] = useState<PriceDraft[] | null>(null);
  const [editing, setEditing] = useState<{ field: PriceField; id: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [removing, setRemoving] = useState<PriceDraft | null>(null);

  const pricing = applied ?? served;
  const saved = pricing ? pricing.models.map(toDraft) : [];
  const rows = draft ?? saved;
  const editable = canMutate && pricing !== null;
  // Editing a price by hand reaches no route on the router, so only the mock build offers the cells and the bar that saves them.
  const handEdit = editable && MOCK;
  const dirty = handEdit && draft !== null && JSON.stringify(draft) !== JSON.stringify(saved);

  const edit = (next: (rows: PriceDraft[]) => PriceDraft[]) => setDraft((d) => next(d ?? saved));
  const startEdit = (id: string, field: PriceField) => {
    if (!handEdit) return;
    edit((r) => r);
    setEditing({ field, id });
  };
  const setCell = (id: string, field: PriceField | 'model', value: string) => edit((r) => r.map((row) => (row.id === id ? { ...row, [field]: value } : row)));
  const discard = () => {
    setDraft(null);
    setEditing(null);
    setError(null);
  };

  // A hand edit changes the standard cells alone, so each row keeps the fast cells of the served row it came from.
  const fastOf = (id: string) => pricing?.models.find((m) => m.model === id)?.fast ?? null;
  const save = () => {
    const next: ModelPrice[] = [];
    const seen = new Set<string>();
    for (const row of rows) {
      const model = row.model.trim();
      if (!model) {
        setError('Every row needs a model id.');
        return;
      }
      if (seen.has(model)) {
        setError(`${model} appears twice; model ids must be unique.`);
        return;
      }
      seen.add(model);
      const price = { fast: fastOf(row.id), model } as ModelPrice;
      for (const f of PRICE_FIELDS) {
        const raw = row[f.key].trim();
        const value = Number(raw);
        if (raw === '' || !Number.isFinite(value) || value < 0) {
          setError(`${model}: ${f.label.toLowerCase()} must be a number of 0 or more.`);
          return;
        }
        price[f.key] = value;
      }
      next.push(price);
    }
    setApplied({ effectiveDate: new Date().toISOString(), models: next, source: 'edited by hand' });
    setDraft(null);
    setEditing(null);
    setError(null);
    onToast('Price table saved');
  };

  return (
    <Card
      title="Pricing"
      subtitle={pricing ? `Effective ${fmtDate(pricing.effectiveDate)}, source: ${sourceLabel(pricing.source)}` : undefined}
      actions={
        handEdit && (
          <Button
            size="md"
            onClick={() =>
              edit((r) => [
                ...r,
                { cacheReadPerM: '', cacheWrite1hPerM: '', cacheWrite5mPerM: '', id: `new-${Date.now()}`, inputPerM: '', isNew: true, model: '', outputPerM: '' },
              ])
            }
          >
            Add model
          </Button>
        )
      }
      padded={false}
      className="card--table"
    >
      <div className="table-wrap">
        <table className="table price-table">
          <thead>
            <tr>
              <th>Model</th>
              {PRICE_FIELDS.map((f) => (
                <th className="right" key={f.key}>
                  {f.label}
                </th>
              ))}
              {handEdit && <th />}
            </tr>
          </thead>
          <tbody>
            {pricing ? (
              rows.flatMap((row) => [
                <tr key={row.id}>
                  <td className="mono">
                    {row.isNew ? (
                      <Input aria-label="Model id" placeholder="model id" value={row.model} onChange={(e) => setCell(row.id, 'model', e.target.value)} style={{ width: 220 }} />
                    ) : (
                      row.model
                    )}
                  </td>
                  {PRICE_FIELDS.map((f) => (
                    <PriceCell
                      key={f.key}
                      editable={handEdit}
                      editing={editing?.id === row.id && editing.field === f.key}
                      label={f.label}
                      model={row.model}
                      onDone={() => setEditing(null)}
                      onEdit={() => startEdit(row.id, f.key)}
                      onChange={(v) => setCell(row.id, f.key, v)}
                      value={row[f.key]}
                    />
                  ))}
                  {handEdit && (
                    <td className="right">
                      <Button
                        size="sm"
                        variant="ghost"
                        icon={<IconX size={14} />}
                        aria-label={`Remove ${row.model || 'this row'}`}
                        title={`Remove ${row.model || 'this row'} from the price table`}
                        onClick={() => setRemoving(row)}
                      />
                    </td>
                  )}
                </tr>,
                ...fastRow(row.id, fastOf(row.id), handEdit),
              ])
            ) : (
              <tr>
                <td colSpan={6}>
                  {failed ? <ErrorState title="Could not load the price table" body={failed} action={<Button size="sm" onClick={onRetry}>Try again</Button>} /> : <Skeleton />}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
      {dirty && (
        <div className="save-bar">
          <span className={error ? 'save-bar__msg is-error' : 'save-bar__msg'}>{error ?? 'Unsaved price edits.'}</span>
          <span className="row">
            <Button variant="ghost" onClick={discard}>
              Discard
            </Button>
            <Button variant="primary" onClick={save}>
              Save
            </Button>
          </span>
        </div>
      )}
      <p className="muted" style={{ padding: '10px 14px' }}>
        USD per million tokens. Costs in the ledger are estimates computed at request time.
      </p>

      <ConfirmDialog
        open={removing !== null}
        onClose={() => setRemoving(null)}
        title="Remove model"
        confirmLabel="Remove"
        danger
        body={removing ? `${removing.model || 'The new row'} loses its prices. Requests on it are costed as unknown until the table carries it again.` : ''}
        onConfirm={() => {
          if (removing) edit((r) => r.filter((row) => row.id !== removing.id));
          setError(null);
        }}
      />
    </Card>
  );
}

/** The read-only fast-mode sub-row under a model that has the cells, in the table's column order. */
function fastRow(id: string, fast: FastPrice | null, handEdit: boolean) {
  if (!fast) return [];
  return [
    <tr key={`${id} fast`} className="price-table__fast">
      <td className="muted" style={{ paddingLeft: 28 }}>
        fast mode
      </td>
      {PRICE_FIELDS.map((f) => (
        <td className="right num" key={f.key}>
          {fmtPrice(fast[f.key])}
        </td>
      ))}
      {handEdit && <td />}
    </tr>,
  ];
}

function PriceCell({
  editable,
  editing,
  label,
  model,
  onChange,
  onDone,
  onEdit,
  value,
}: {
  editable: boolean;
  editing: boolean;
  label: string;
  model: string;
  onChange: (value: string) => void;
  onDone: () => void;
  onEdit: () => void;
  value: string;
}) {
  const number = Number(value);
  const shown = value.trim() === '' || !Number.isFinite(number) ? <span className="muted">-</span> : number === 0 ? <span className="muted">-</span> : fmtPrice(number);
  if (editing) {
    return (
      <td className="right num price-cell">
        <input
          aria-label={`${label} for ${model}`}
          autoFocus
          className="input input--price"
          min="0"
          step="0.0001"
          type="number"
          value={value}
          onChange={(e) => onChange(e.target.value)}
          onBlur={onDone}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === 'Escape') onDone();
          }}
        />
      </td>
    );
  }
  if (!editable) return <td className="right num">{shown}</td>;
  return (
    <td className="right num price-cell">
      <button type="button" className="price-cell__btn" title={`${label} for ${model}, USD per million tokens. Click to edit.`} onClick={onEdit}>
        {shown}
      </button>
    </td>
  );
}
