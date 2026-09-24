import { useState } from 'react';
import { api, MOCK } from '@/api';
import type { ModelPrice, PriceImportPreview, Settings } from '@/api/types';
import { useSession } from '@/app/session';
import { useAppSettings, type ThemeChoice } from '@/app/settings';
import { IconMonitor, IconMoon, IconSun, IconX } from '@/components/ui/icons';
import { Badge, Button, Card, ConfirmDialog, CopyButton, Dialog, ErrorState, Input, KeyValue, Segmented, Skeleton, Toast, Toggle } from '@/components/ui/primitives';
import { fmtDate, fmtInt } from '@/lib/format';
import type { TimeFormat } from '@/lib/format';
import { useAsync } from '@/lib/hooks';
import { snippetFor } from '@/lib/snippet';

const OPTIONAL_SNIPPET = `CC_KEEPALIVE_OWNER_ID=$(uuidgen)
ANTHROPIC_CUSTOM_HEADERS="X-Keepalive-Owner-Id: $CC_KEEPALIVE_OWNER_ID"`;

const ENV_TERMS: { body: string; term: string }[] = [
  {
    term: 'ANTHROPIC_BASE_URL',
    body: "Points Claude Code at the router. Every model request and the cache-keepalive companion's reports go here.",
  },
  {
    term: 'CLAUDE_CODE_OAUTH_TOKEN',
    body: 'Holds the router API key. Claude Code treats it as a subscription credential, which keeps the one-hour cache TTL, the unified rate-limit headers, and the automatic continue after a usage window resets; an ANTHROPIC_API_KEY would switch those off.',
  },
  {
    term: '_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1',
    body: 'Makes Claude Code send its first-party request shape to the router: the billing hash, the cache diagnostics beta, and the first-party betas. Without it the router relays the reduced shape as is. It also switches off gateway model discovery, and being an internal variable a future build may drop it.',
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

const IMPORT_HINT =
  'Accepts the LiteLLM model_prices_and_context_window.json or a models.dev catalog. Per-token costs are converted to USD per million; the 1h write price comes from the above-1hr field. Only models the router serves are imported, and nothing is written until you confirm the diff.';

const EXPIRY_PRESETS: { label: string; seconds: number }[] = [
  { label: '1h', seconds: 3_600 },
  { label: '6h', seconds: 21_600 },
  { label: '24h', seconds: 86_400 },
  { label: '7d', seconds: 604_800 },
];

const expiryLabel = (seconds: number) => EXPIRY_PRESETS.find((p) => p.seconds === seconds)?.label ?? `${seconds}s`;

/** These come from the bootstrap file and the admin API only reads them, so each says where it is set. */
const bootstrapNote = (setting: string) => `Set by ${setting} in the bootstrap file`;
const EXPIRY_NOTE = bootstrapNote('sessions.expire_after_seconds');

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

interface DiffRow {
  current: number | null;
  field: string;
  id: string;
  imported: number;
  isNew: boolean;
  model: string;
}

type Pricing = Settings['pricing'];

/** Two decimals, extended to at most four when an imported price needs them. */
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
 * An imported table's source is the URL the router was handed, query string and all, so only its host is shown.
 * Anything else is a label the router wrote and reads as it stands.
 */
function sourceLabel(source: string): string {
  let url: URL;
  try {
    url = new URL(source);
  } catch {
    return source;
  }
  return url.protocol === 'http:' || url.protocol === 'https:' ? `imported from ${url.host}` : source;
}

/**
 * The change count is the diff on screen rather than the router's, because the
 * table here may carry price edits the router has never seen.
 */
const importSubtitle = (preview: PriceImportPreview, changes: number) =>
  `${preview.inFile} models in the file, ${preview.served} the router serves; ${changes} change${changes === 1 ? '' : 's'}. Nothing is written until you confirm.`;

/** One line per changed field; a model the table does not carry yet contributes every field. */
function diffRows(current: ModelPrice[], imported: ModelPrice[]): DiffRow[] {
  const byModel = new Map(current.map((m) => [m.model, m]));
  const out: DiffRow[] = [];
  for (const m of imported) {
    const before = byModel.get(m.model) ?? null;
    for (const f of PRICE_FIELDS) {
      if (before && before[f.key] === m[f.key]) continue;
      out.push({ current: before ? before[f.key] : null, field: f.label, id: `${m.model}-${f.key}`, imported: m[f.key], isNew: before === null, model: m.model });
    }
  }
  return out;
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
  const snippet = snippetFor(s?.listeners.proxy ?? null);

  return (
    <div className="settings">
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
              The secret the variable <code>admin.viewer_secret_env</code> names. Reads everything except captured bodies, live previews, question text, and webhook URLs beyond their host. Cannot create, edit, or delete.
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

      <PricingCard canMutate={canMutate} error={settings.error} pricing={s ? s.pricing : null} onToast={setToast} />

      <Card title="Sessions" subtitle="A session leaves the Sessions view once it has gone this long without a request. Its pin and any cache-warming recipe expire with it, so the next request picks an upstream afresh.">
        {s ? (
          <>
            <KeyValue rows={[{ label: 'Session expiry', value: expiryLabel(s.sessions.expireAfterSeconds), mono: true, title: EXPIRY_NOTE }]} />
            <p className="muted" style={{ marginTop: 10 }}>
              {EXPIRY_NOTE}
            </p>
          </>
        ) : (
          <Skeleton height={60} />
        )}
      </Card>

      <Card title="Retention" subtitle="Older rows are deleted by the hourly purge.">
        {s ? (
          <>
            <KeyValue
              rows={[
                { label: 'Ledger (days)', value: s.retention.ledgerDays === 0 ? 'forever' : fmtInt(s.retention.ledgerDays), mono: true, title: 'Per-request rows, attempts, and events; 0 keeps them forever.' },
                { label: 'Rollups (days)', value: s.retention.rollupDays === 0 ? 'forever' : fmtInt(s.retention.rollupDays), mono: true, title: 'Hourly rollups; 0 keeps them forever.' },
                { label: 'Quota samples (days)', value: fmtInt(s.retention.quotaSamplesDays), mono: true, title: '5-minute utilization samples per upstream.' },
                { label: 'Body capture, max hours', value: fmtInt(s.retention.bodyCaptureMaxHours), mono: true, title: "Upper bound for any pool's or key's capture retention." },
              ]}
            />
            <p className="muted" style={{ marginTop: 10 }}>
              {bootstrapNote('stats.retention_days, stats.rollup_retention_days, quota.sample_retention_days, and stats.body_capture_max_hours')}
            </p>
          </>
        ) : (
          <Skeleton height={60} />
        )}
      </Card>

      <Card title="Keepalive" subtitle="Ceilings the router holds cache warming to, whatever a pool asks for.">
        {s ? (
          <>
            <KeyValue
              rows={[
                { label: 'Lease (seconds)', value: fmtInt(s.keepalive.leaseSeconds), mono: true, title: 'How long a companion registration stands without a report.' },
                { label: 'Max agents', value: fmtInt(s.keepalive.maxAgents), mono: true, title: 'Recipes the router holds across every session.' },
                { label: 'Max request bytes', value: fmtInt(s.keepalive.maxRequestBytes), mono: true, title: 'Largest body a companion may report.' },
                { label: 'Max retained bytes', value: fmtInt(s.keepalive.maxRetainedBytes), mono: true, title: 'Total size of the prefixes held for replay.' },
                { label: 'Max sessions', value: fmtInt(s.keepalive.maxSessions), mono: true, title: 'Sessions that may carry a recipe at once.' },
                { label: 'Max workers', value: fmtInt(s.keepalive.maxWorkers), mono: true, title: 'Probes the router runs in parallel.' },
                { label: 'OAuth prewarm verified', value: s.keepalive.oauthPrewarmVerified ? 'yes' : 'no', mono: true, title: 'Whether a subscription upstream has been checked to accept a prewarm probe.' },
              ]}
            />
            <p className="muted" style={{ marginTop: 10 }}>
              {bootstrapNote('the keepalive block')}
            </p>
          </>
        ) : (
          <Skeleton height={60} />
        )}
      </Card>

      <Card title="Listeners" subtitle="Addresses the router binds at startup.">
        {s ? (
          <>
            <KeyValue
              rows={[
                { label: 'Admin', value: s.listeners.admin, mono: true, title: 'This interface and the admin API.' },
                { label: 'Metrics', value: s.listeners.metrics, mono: true, title: 'The Prometheus scrape endpoint.' },
                { label: 'Proxy', value: s.listeners.proxy, mono: true, title: 'Where Claude Code sends its model requests.' },
              ]}
            />
            <p className="muted" style={{ marginTop: 10 }}>
              {bootstrapNote('admin.listen, proxy.listen, and stats.metrics_listen')}
            </p>
            <h3 className="section-title" style={{ marginTop: 14 }}>
              Transport
            </h3>
            <p className="muted">The proxy and admin listeners speak HTTP/1.1 only.</p>
          </>
        ) : (
          <Skeleton height={60} />
        )}
      </Card>

      <Card
        title="Downstream client configuration"
        subtitle="Export these in the shell that launches Claude Code. Replace the token with a key from the API keys page."
        actions={<CopyButton text={snippet} label="Copy snippet" size="md" />}
      >
        <pre className="snippet">{snippet}</pre>
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

      <Card title="Compatibility" subtitle="What the bootstrap file's compatibility block sets; a restart applies a change.">
        {s ? (
          <KeyValue
            rows={[
              { label: 'Cache diagnostics', value: s.compatibility.cacheDiagnostics ? 'on' : 'off', title: bootstrapNote('compatibility.cache_diagnostics') },
              { label: 'Outbound gzip', value: s.compatibility.outboundGzip ? 'on' : 'off', title: bootstrapNote('compatibility.outbound_gzip') },
              { label: 'Profile', value: s.compatibility.profile, mono: true },
              { label: 'Usage entrypoint', value: s.compatibility.usageEntrypoint, mono: true },
            ]}
          />
        ) : (
          <Skeleton height={60} />
        )}
      </Card>

      <Card title="Quota" subtitle="What the bootstrap file's quota block sets; a restart applies a change.">
        {s ? (
          <KeyValue
            rows={[
              { label: 'Poll interval (seconds)', value: fmtInt(s.quota.pollIntervalSeconds), mono: true, title: bootstrapNote('quota.poll_interval_seconds') },
              { label: 'Sample retention (days)', value: fmtInt(s.quota.sampleRetentionDays), mono: true, title: bootstrapNote('quota.sample_retention_days') },
            ]}
          />
        ) : (
          <Skeleton height={60} />
        )}
      </Card>

      <Card title="Routing" subtitle="What the bootstrap file's routing block sets; a restart applies a change.">
        {s ? (
          <KeyValue
            rows={[
              { label: 'Catalog sync interval (seconds)', value: fmtInt(s.routing.catalogSyncIntervalSeconds), mono: true, title: bootstrapNote('routing.catalog_sync_interval_seconds') },
              { label: 'Cooldown fallback (seconds)', value: fmtInt(s.routing.cooldownFallbackSeconds), mono: true, title: bootstrapNote('routing.cooldown_fallback_seconds') },
              { label: 'Model rejection (seconds)', value: fmtInt(s.routing.modelRejectionSeconds), mono: true, title: bootstrapNote('routing.model_rejection_seconds') },
              { label: 'Max attempts', value: fmtInt(s.routing.maxAttempts), mono: true, title: bootstrapNote('routing.max_attempts') },
            ]}
          />
        ) : (
          <Skeleton height={60} />
        )}
      </Card>

      <Card title="Storage" subtitle="The files the bootstrap file points the router at; a restart applies a change.">
        {s ? (
          <KeyValue
            rows={[
              { label: 'Ledger path', value: s.storage.ledgerPath, mono: true, title: bootstrapNote('stats.ledger_path') },
              { label: 'Pricing path', value: s.storage.pricingPath ?? 'not configured', mono: s.storage.pricingPath !== null, title: bootstrapNote('stats.pricing_path') },
              { label: 'Storage path', value: s.storage.path, mono: true, title: bootstrapNote('storage.path') },
            ]}
          />
        ) : (
          <Skeleton height={60} />
        )}
      </Card>

      <Card title="Usage" subtitle="What the bootstrap file's usage block sets; a restart applies a change.">
        {s ? (
          <KeyValue
            rows={[
              { label: 'Cache TTL (seconds)', value: fmtInt(s.usage.cacheTtlSeconds), mono: true, title: bootstrapNote('usage.cache_ttl_seconds') },
              { label: 'Refresh concurrency', value: fmtInt(s.usage.refreshConcurrency), mono: true, title: bootstrapNote('usage.refresh_concurrency') },
            ]}
          />
        ) : (
          <Skeleton height={60} />
        )}
      </Card>

      <Card title="About">
        {s ? (
          <KeyValue
            rows={[
              { label: 'Version', value: s.about.version, mono: true },
              { label: 'Profile', value: s.about.profile, mono: true },
              { label: 'Public URL', value: s.about.publicUrl ?? 'not configured', mono: true },
            ]}
          />
        ) : (
          <Skeleton height={60} />
        )}
      </Card>
      <Toast message={toast} onDone={() => setToast(null)} />
    </div>
  );
}

function PricingCard({ canMutate, error: failed, onToast, pricing: served }: { canMutate: boolean; error: string | null; onToast: (message: string) => void; pricing: Pricing | null }) {
  const [applied, setApplied] = useState<Pricing | null>(null);
  const [importError, setImportError] = useState<string | null>(null);
  const [draft, setDraft] = useState<PriceDraft[] | null>(null);
  const [editing, setEditing] = useState<{ field: PriceField; id: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [removing, setRemoving] = useState<PriceDraft | null>(null);
  const [url, setUrl] = useState('');
  const [review, setReview] = useState<{ preview: PriceImportPreview; rows: DiffRow[] } | null>(null);

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
      const price = { model } as ModelPrice;
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

  const confirmImport = async () => {
    if (!review) return;
    try {
      setApplied(await api.confirmPriceImport(review.preview.token));
      setDraft(null);
      setEditing(null);
      setError(null);
      setReview(null);
      setUrl('');
      onToast('Price list imported');
    } catch (e) {
      onToast(e instanceof Error ? e.message : String(e));
    }
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
              rows.map((row) => (
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
                </tr>
              ))
            ) : (
              <tr>
                <td colSpan={6}>
                  {failed ? <ErrorState title="Could not load the price table" body={failed} /> : <Skeleton />}
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
      {editable && (
        <div className="price-import">
          <form
            className="row"
            onSubmit={(e) => {
              e.preventDefault();
              void api.previewPriceImport(url).then(
                (preview) => {
                  setImportError(null);
                  setReview({ preview, rows: diffRows(pricing?.models ?? [], preview.models) });
                },
                // A mistyped URL is the common way this fails, and the button looked dead without this.
                (failure: unknown) => setImportError(failure instanceof Error ? failure.message : String(failure)),
              );
            }}
          >
            <Input placeholder="Import from URL" value={url} onChange={(e) => setUrl(e.target.value)} style={{ width: 320 }} />
            <Button type="submit" size="md" disabled={!url}>
              Import
            </Button>
          </form>
          <p className="muted price-import__hint">{IMPORT_HINT}</p>
          {importError && (
            <p className="price-import__error" role="alert">
              {importError}
            </p>
          )}
        </div>
      )}

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

      <Dialog
        open={review !== null}
        onClose={() => setReview(null)}
        title="Review import"
        subtitle={review ? importSubtitle(review.preview, review.rows.length) : undefined}
        width={720}
        footer={
          <>
            <Button variant="ghost" onClick={() => setReview(null)}>
              Cancel
            </Button>
            <Button variant="primary" disabled={!review || review.rows.length === 0} onClick={() => void confirmImport()}>
              Confirm
            </Button>
          </>
        }
      >
        {review && review.rows.length === 0 ? (
          <p className="secondary">Every price in that catalog already matches the table, so there is nothing to write.</p>
        ) : (
          <table className="table diff-table">
            <thead>
              <tr>
                <th>Model</th>
                <th>Field</th>
                <th className="right">Current</th>
                <th className="right">Imported</th>
              </tr>
            </thead>
            <tbody>
              {(review?.rows ?? []).map((r) => (
                <tr key={r.id}>
                  <td className="mono">
                    {r.model}
                    {r.isNew && (
                      <Badge tone="accent" title="This model is not in the table yet; confirming adds it.">
                        new
                      </Badge>
                    )}
                  </td>
                  <td>{r.field}</td>
                  <td className="right num">{r.current === null ? <span className="muted">-</span> : fmtPrice(r.current)}</td>
                  <td className="right num">{fmtPrice(r.imported)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Dialog>
    </Card>
  );
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
