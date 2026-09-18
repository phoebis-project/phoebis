import { useState } from 'react';
import { api, MOCK } from '@/api';
import type { ModelPrice, PriceImportPreview, Settings } from '@/api/types';
import { useSession } from '@/app/session';
import { useAppSettings, type ThemeChoice } from '@/app/settings';
import { IconMonitor, IconMoon, IconSun, IconX } from '@/components/ui/icons';
import { Badge, Button, Card, ConfirmDialog, CopyButton, Dialog, Field, Input, KeyValue, Segmented, Skeleton, Toast, Toggle } from '@/components/ui/primitives';
import { fmtDate } from '@/lib/format';
import type { TimeFormat } from '@/lib/format';
import { useAsync } from '@/lib/hooks';

const SNIPPET = `ANTHROPIC_BASE_URL=https://router.example.com
CLAUDE_CODE_OAUTH_TOKEN=<api key>
_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1`;

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

/** The host a source line names, falling back to what was typed when it is not a URL. */
function hostOf(url: string): string {
  const trimmed = url.trim();
  for (const candidate of [trimmed, `https://${trimmed}`]) {
    try {
      return new URL(candidate).host;
    } catch {
      continue;
    }
  }
  return trimmed;
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
  const [expiry, setExpiry] = useState<number | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const s = settings.data;
  const expirySeconds = expiry ?? s?.sessions.expireAfterSeconds ?? null;
  const expiryLabel = EXPIRY_PRESETS.find((p) => p.seconds === expirySeconds)?.label ?? null;

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
              The secret the variable <code>admin.viewer_secret_env</code> names. Reads everything except captured request and response bodies. Cannot create, edit, or delete.
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

      <PricingCard canMutate={canMutate} pricing={s ? s.pricing : null} onToast={setToast} />

      <Card title="Sessions" subtitle="What the Sessions view lists.">
        <div className="setting-row">
          <div>
            <b>Session expiry</b>
            <p className="muted">
              A session leaves the Sessions view once it has gone this long without a request. Its pin and any cache-warming recipe expire with it, so the next request picks an upstream afresh.
            </p>
          </div>
          {expirySeconds === null ? (
            <Skeleton width={180} height={30} />
          ) : canMutate ? (
            <Segmented<string>
              ariaLabel="Session expiry"
              value={String(expirySeconds)}
              onChange={(v) => {
                const seconds = Number(v);
                setExpiry(seconds);
                // The Sessions view drops rows idle past this, so the mock has to
                // hold the choice rather than leave it in this page's state.
                void api.setSessionExpiry(seconds).then(() => setToast('Session expiry saved'));
              }}
              options={EXPIRY_PRESETS.map((p) => ({ value: String(p.seconds), label: p.label }))}
            />
          ) : (
            <span className="mono">{expiryLabel ?? `${expirySeconds}s`}</span>
          )}
        </div>
      </Card>

      <Card title="Retention" subtitle="Older rows are deleted by the nightly compaction.">
        {s ? (
          <div className="form-row form-row--3">
            <Field label="Ledger (days)" hint="Per-request rows and hourly rollups.">
              <Input type="number" defaultValue={s.retention.ledgerDays} disabled={!canMutate} />
            </Field>
            <Field label="Quota samples (days)" hint="5-minute utilization samples per upstream.">
              <Input type="number" defaultValue={s.retention.quotaSamplesDays} disabled={!canMutate} />
            </Field>
            <Field label="Body capture, max hours" hint="Upper bound for any pool's or key's capture retention.">
              <Input type="number" defaultValue={s.retention.bodyCaptureMaxHours} disabled={!canMutate} />
            </Field>
          </div>
        ) : (
          <Skeleton height={60} />
        )}
        {canMutate && (
          <div className="row" style={{ marginTop: 14, justifyContent: 'flex-end' }}>
            <Button variant="primary" onClick={() => setToast('Retention saved')}>
              Save retention
            </Button>
          </div>
        )}
      </Card>

      <Card
        title="Downstream client configuration"
        subtitle="Export these in the shell that launches Claude Code. Replace the token with a key from the API keys page."
        actions={<CopyButton text={SNIPPET} label="Copy snippet" size="md" />}
      >
        <pre className="snippet">{SNIPPET}</pre>
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

      <Card title="About">
        {s ? (
          <KeyValue
            rows={[
              { label: 'Version', value: s.about.version, mono: true },
              { label: 'Profile', value: s.about.profile, mono: true },
              { label: 'Public URL', value: s.about.publicUrl, mono: true },
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

function PricingCard({ canMutate, onToast, pricing: served }: { canMutate: boolean; onToast: (message: string) => void; pricing: Pricing | null }) {
  const [applied, setApplied] = useState<Pricing | null>(null);
  const [draft, setDraft] = useState<PriceDraft[] | null>(null);
  const [editing, setEditing] = useState<{ field: PriceField; id: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [removing, setRemoving] = useState<PriceDraft | null>(null);
  const [url, setUrl] = useState('');
  const [review, setReview] = useState<{ host: string; models: ModelPrice[]; preview: PriceImportPreview; rows: DiffRow[] } | null>(null);

  const pricing = applied ?? served;
  const saved = pricing ? pricing.models.map(toDraft) : [];
  const rows = draft ?? saved;
  const dirty = draft !== null && JSON.stringify(draft) !== JSON.stringify(saved);
  const editable = canMutate && pricing !== null;

  const edit = (next: (rows: PriceDraft[]) => PriceDraft[]) => setDraft((d) => next(d ?? saved));
  const startEdit = (id: string, field: PriceField) => {
    if (!editable) return;
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

  const confirmImport = () => {
    if (!review || !pricing) return;
    const imported = new Map(review.models.map((m) => [m.model, m]));
    const kept = pricing.models.map((m) => imported.get(m.model) ?? m);
    const added = review.models.filter((m) => !pricing.models.some((c) => c.model === m.model));
    setApplied({ effectiveDate: new Date().toISOString(), models: [...kept, ...added], source: `imported from ${review.host}` });
    setDraft(null);
    setEditing(null);
    setError(null);
    setReview(null);
    setUrl('');
    onToast('Price list imported');
  };

  return (
    <Card
      title="Pricing"
      subtitle={pricing ? `Effective ${fmtDate(pricing.effectiveDate)}, source: ${pricing.source}` : undefined}
      actions={
        editable && (
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
              {editable && <th />}
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
                      editable={editable}
                      editing={editing?.id === row.id && editing.field === f.key}
                      label={f.label}
                      model={row.model}
                      onDone={() => setEditing(null)}
                      onEdit={() => startEdit(row.id, f.key)}
                      onChange={(v) => setCell(row.id, f.key, v)}
                      value={row[f.key]}
                    />
                  ))}
                  {editable && (
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
                  <Skeleton />
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
              void api.previewPriceImport(url).then((preview) => setReview({ host: hostOf(url), models: preview.models, preview, rows: diffRows(pricing?.models ?? [], preview.models) }));
            }}
          >
            <Input placeholder="Import from URL" value={url} onChange={(e) => setUrl(e.target.value)} style={{ width: 320 }} />
            <Button type="submit" size="md" disabled={!url}>
              Import
            </Button>
          </form>
          <p className="muted price-import__hint">{IMPORT_HINT}</p>
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
            <Button variant="primary" disabled={!review || review.rows.length === 0} onClick={confirmImport}>
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
