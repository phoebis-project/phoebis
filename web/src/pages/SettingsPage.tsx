import { api } from '@/api';
import { effectiveLabel } from '@/api/decode';
import { useAppSettings, type ThemeChoice } from '@/app/settings';
import { IconMonitor, IconMoon, IconSun } from '@/components/ui/icons';
import { Badge, Button, Card, CopyButton, ErrorState, KeyValue, Segmented, Skeleton } from '@/components/ui/primitives';
import { fmtMiB } from '@/lib/format';
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

/** These come from the configuration file and the admin API only reads them, so each says where it is set. */
const bootstrapNote = (setting: string) => `Set by ${setting} in the configuration file`;
const restartNote = (key: string) => `${key}\nSet in the configuration file; a change takes a restart.`;

function SecretBadge({ configured }: { configured: boolean }) {
  return <Badge tone={configured ? 'good' : 'warning'}>{configured ? 'configured' : 'not configured'}</Badge>;
}

export function SettingsPage() {
  const settings = useAsync(() => api.getSettings(), []);
  const { theme, setTheme, timeFormat, setTimeFormat } = useAppSettings();
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

      <Card title="About">
        {s ? (
          <>
            <KeyValue
              rows={[
                { label: 'Version', value: s.about.version, mono: true },
                { label: 'Profile', value: s.about.profile, mono: true },
                { label: 'Metrics listener', value: s.listeners.metrics, mono: true, title: 'The Prometheus scrape endpoint.' },
                { label: 'Ledger path', value: s.storage.ledgerPath, mono: true, title: bootstrapNote('stats.ledger_path') },
                { label: 'API prices path', value: `${s.storage.apiPricesPath ?? 'not configured'}, effective ${effectiveLabel(s.apiPrices.effective)}`, mono: s.storage.apiPricesPath !== null, title: bootstrapNote('stats.api_prices_path') },
                { label: 'Subscription costs path', value: `${s.storage.subscriptionCostsPath ?? 'not configured'}${s.subscriptionCosts.problem === null ? `, effective ${effectiveLabel(s.subscriptionCosts.effective)}` : `. ${s.subscriptionCosts.problem}`}`, mono: s.subscriptionCosts.problem === null && s.storage.subscriptionCostsPath !== null, title: bootstrapNote('stats.subscription_costs_path') },
                { label: 'Storage path', value: s.storage.path, mono: true, title: bootstrapNote('storage.path') },
                { label: 'Keepalive sessions', value: String(s.keepalive.maxSessions), mono: true, title: restartNote('keepalive.max_sessions') },
                { label: 'Keepalive agents', value: String(s.keepalive.maxAgents), mono: true, title: restartNote('keepalive.max_agents') },
                { label: 'Keepalive retained bytes', value: fmtMiB(s.keepalive.maxRetainedBytes), mono: true, title: restartNote('keepalive.max_retained_bytes') },
                { label: 'Admin secret', value: <SecretBadge configured={s.access.adminConfigured} />, title: 'The secret the variable admin.secret_env names. It reaches every route.' },
              ]}
            />
            <p className="muted" style={{ marginTop: 10 }}>
              The values the configuration file sets are on the installation page, and <code>phoebis check-config</code> prints this router&apos;s.
            </p>
          </>
        ) : settings.error ? (
          <ErrorState title="Could not load the settings" body={settings.error} action={<Button size="sm" onClick={settings.reload}>Try again</Button>} />
        ) : (
          <Skeleton height={60} />
        )}
      </Card>
    </div>
  );
}
