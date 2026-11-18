import { useEffect, useId, useState, type ReactNode } from 'react';
import { api } from '@/api';
import { effectiveLabel } from '@/api/decode';
import type { FeatureSettings } from '@/api/types';
import { useAppSettings, type PaletteChoice, type ThemeChoice } from '@/app/settings';
import { IconMonitor, IconMoon, IconSun } from '@/components/ui/icons';
import { Badge, Card, CopyButton, ErrorState, KeyValue, Segmented, Select, Skeleton, Tabs } from '@/components/ui/primitives';
import { UnitLegend, WeekSwitch } from '@/components/ui/UnitControls';
import { revisionAt } from '@/lib/costs';
import { docsHref } from '@/lib/docs';
import { fmtDuration, plural, type TimeFormat } from '@/lib/format';
import { useAsync, useNow } from '@/lib/hooks';
import { SETUP_LABEL, SETUPS, snippetFor, type Setup } from '@/lib/snippet';
import { positionOf, UNIT_LABEL, UNIT_POSITIONS } from '@/lib/unit';

/**
 * The list under the shown block names the terms of the block's lines, in this order, which is the blocks' own. The
 * prompt cache's entry comes last and is listed under every setup, since no block holds its line. `feature` is the
 * sentence a term gains under a feature setup.
 */
const ENV_TERMS: { always?: boolean; body: string; feature?: string; term: string }[] = [
  {
    term: 'ANTHROPIC_BASE_URL',
    body: "Points Claude Code at the router's proxy listener, at the URL that reaches it from the client's machine. The address under the block is where the router binds it. Every model request and the companion mod's reports go here.",
  },
  {
    term: 'CLAUDE_CODE_OAUTH_TOKEN',
    body: 'Holds the router key. Claude Code treats it as a subscription credential, which keeps the one-hour cache TTL, the unified rate-limit headers, and the automatic continue after a usage window resets. An ANTHROPIC_API_KEY would switch those off.',
  },
  {
    term: 'CLAUDE_CODE_SUBSCRIPTION_TYPE=max',
    body: "Marks the session a consumer subscription, which no organization policy governs, so the client skips the policy check that under the first-party variable goes to api.anthropic.com with the router key, fails, and refuses the companion mod's reports. The client documents it nowhere, and a future build may drop it.",
    feature: 'The claude.ai features need this line, as max or pro.',
  },
  {
    term: '_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1',
    body: 'Makes Claude Code send its first-party request shape to the router: the billing hash, the cache diagnostics beta, and the first-party betas. Without it the client sends its reduced shape and the router restores the fields before relaying it. It also switches off gateway model discovery, and being an internal variable a future build may drop it.',
    feature: 'The claude.ai features need this line.',
  },
  {
    term: 'CLAUDE_CODE_GATEWAY_HINT_HEADERS=1',
    body: 'Makes the client send its gateway hint headers (the agent type, the compaction marks and the tool durations), which the request log and the analytics read. Under the first-party flag above they arrive anyway.',
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
    term: 'CLAUDE_CODE_OAUTH_SCOPES',
    body: 'The scopes the client believes its token holds. The connectors open under user:mcp_servers.',
  },
  {
    term: 'CLAUDE_ENV_FILE',
    body: 'Names the file the launch script writes, which gives each Bash command back the proxy and trust settings the shell had.',
  },
  {
    term: 'HTTPS_PROXY',
    body: "Sends the client's other HTTPS calls through phoebis-proxy, which hands the two feature hosts to the router. Replace the address where the proxy runs elsewhere.",
  },
  {
    term: 'NODE_EXTRA_CA_CERTS',
    body: "Names the feature CA's certificate, which the launch script fetches from the router and checks against the fingerprint.",
  },
  {
    term: 'NO_PROXY',
    body: 'Keeps the model requests and the CA fetch on their direct road to the router. Replace the placeholder host as in the base URL, and add no other host: an entry that matches a feature host sends the router key to the upstream.',
  },
  {
    term: 'PHOEBIS_FEATURE_CA_SHA256',
    body: "The launch script's input for its fingerprint compare. The client reads no such variable.",
  },
  {
    term: 'CLAUDE_CODE_PROMPT_CACHE_TTL=5m',
    body: 'Worth setting for Fable 5.2 and Fable 5.1 sessions in a pool with cache warming on: at a 0.025x read price a five-minute entry kept warm by probes stays cheaper than a one-hour entry. Leave it unset without warming.',
    always: true,
  },
];

/**
 * The feature CA's fingerprint with its copy control, and the certificate offered as a file. The URL is made inside the
 * effect that revokes it: under StrictMode a mount runs its cleanup once, which would revoke a URL made in render under
 * the link that keeps it.
 */
function FeatureCaLine({ features }: { features: FeatureSettings }) {
  const [href, setHref] = useState<string | null>(null);
  useEffect(() => {
    const url = URL.createObjectURL(new Blob([features.caPem], { type: 'application/x-pem-file' }));
    // eslint-disable-next-line react-hooks/set-state-in-effect -- made beside the cleanup that revokes it
    setHref(url);
    return () => URL.revokeObjectURL(url);
  }, [features.caPem]);
  return (
    <div className="row feature-ca">
      <b>Feature CA fingerprint</b>
      <code>{features.caFingerprint}</code>
      <CopyButton text={features.caFingerprint} label="Copy fingerprint" />
      {href !== null && (
        <a href={href} download="feature-ca.pem">
          Download feature-ca.pem
        </a>
      )}
    </div>
  );
}

/** These come from the configuration file and the admin API only reads them, so each says where it is set. */
const bootstrapNote = (setting: string) => `Set by ${setting} in the configuration file`;

/** About's words for a retention key's day count, where 0 keeps the rows forever. */
const keptFor = (days: number) => (days === 0 ? 'kept forever' : `kept for ${plural(days, 'day')}`);

/**
 * A table's date in force, its revision count and the next date while one is pending, read at one instant by the rule
 * the cost reference's select reads. An answer that lists no revision names the date it sent alone. The next date
 * keeps to one line, since the row it ends can wrap at a desktop width.
 */
const revisionsNote = (revisions: { effective: string; effectiveDate: string | null }[], sent: string, now: number): ReactNode => {
  const inForce = revisionAt(revisions, now);
  if (inForce === null) return `effective ${effectiveLabel(sent)}`;
  const pending = revisions.find((r) => r !== inForce && r.effectiveDate !== null && Date.parse(r.effectiveDate) > now);
  const count = plural(revisions.length, 'revision');
  const next = pending === undefined ? null : <>, next <span className="nowrap">{effectiveLabel(pending.effective)}</span></>;
  return <>effective {effectiveLabel(inForce.effective)}, {count}{next}</>;
};

/** A price row's source words, in the mono face where they name the file read. With no path set the router can only
 *  have read the table it ships. */
const sourceOf = (source: string, path: string | null): { mono: boolean; text: string } =>
  path === null ? { mono: false, text: 'shipped table (not configured)' } : source === 'shipped' ? { mono: false, text: `shipped table (no file at ${path})` } : { mono: true, text: path };

function SecretBadge({ configured }: { configured: boolean }) {
  return <Badge tone={configured ? 'good' : 'warning'}>{configured ? 'configured' : 'not configured'}</Badge>;
}

export function SettingsPage() {
  const settings = useAsync(() => api.getSettings(), []);
  const { theme, setTheme, palette, setPalette, timeFormat, setTimeFormat, unit, setUnit, week, setWeek } = useAppSettings();
  const now = useNow(60_000);
  const [setup, setSetup] = useState<Setup>('base');
  const setupId = useId();
  const s = settings.data;
  const snippet = snippetFor(setup, s?.listeners ?? null, s?.features ?? null);
  const named = new Set(snippet?.split('\n').map((line) => line.split('=')[0]));
  const featureSetup = setup !== 'base';
  const settingsPending = settings.error ? <ErrorState title="Could not load the settings" body={settings.error} retry={settings.reload} /> : <Skeleton height={60} />;

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
            <b>Palette</b>
            <p className="muted">GitHub follows the colors of github.com.</p>
          </div>
          <Segmented<PaletteChoice>
            ariaLabel="Palette"
            value={palette}
            onChange={setPalette}
            options={[
              { value: 'default', label: 'Default' },
              { value: 'github', label: 'GitHub' },
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
        <div className="setting-row">
          <div>
            <b>Unit</b>
            <p className="muted">The unit every view prices a request in. Native reads a subscription request as a share of the chosen week and a billed request in dollars.</p>
          </div>
          {/* The legend sits beside the mirror too, since the top bar gives its button up at a phone's width. */}
          <div className="unit-setting">
            <Select aria-label="Unit" value={unit} onChange={(e) => setUnit(positionOf(e.target.value))}>
              {UNIT_POSITIONS.map((p) => (
                <option key={p} value={p}>
                  {UNIT_LABEL[p]}
                </option>
              ))}
            </Select>
            <WeekSwitch week={week} onChange={setWeek} short={false} disabled={unit !== 'native'} />
            <UnitLegend />
          </div>
        </div>
      </Card>

      <Card title="About">
        {s ? (
          <>
            <KeyValue
              className="settings__about"
              rows={[
                { label: 'Version', value: s.about.version, mono: true },
                { label: 'Profile', value: s.about.profile, mono: true },
                { label: 'Metrics listener', value: s.listeners.metrics, mono: true, title: 'The Prometheus scrape endpoint.' },
                { label: 'Ledger path', value: s.storage.ledgerPath, mono: true, title: bootstrapNote('stats.ledger_path') },
                { label: 'API prices path', value: <>{sourceOf(s.apiPrices.source, s.storage.apiPricesPath).text}, {revisionsNote(s.apiPrices.revisions, s.apiPrices.effective, now)}</>, mono: sourceOf(s.apiPrices.source, s.storage.apiPricesPath).mono, title: bootstrapNote('stats.api_prices_path') },
                { label: 'Subscription costs path', value: <>{sourceOf(s.subscriptionCosts.source, s.storage.subscriptionCostsPath).text}{s.subscriptionCosts.problem === null ? <>, {revisionsNote(s.subscriptionCosts.revisions, s.subscriptionCosts.effective, now)}</> : `. ${s.subscriptionCosts.problem}`}</>, mono: s.subscriptionCosts.problem === null && sourceOf(s.subscriptionCosts.source, s.storage.subscriptionCostsPath).mono, title: bootstrapNote('stats.subscription_costs_path') },
                { label: 'Storage path', value: s.storage.path, mono: true, title: bootstrapNote('storage.path') },
                { label: 'Requests, samples and the routing log', value: keptFor(s.retention.retentionDays), title: bootstrapNote('stats.retention_days') },
                { label: 'Rollups', value: keptFor(s.retention.rollupRetentionDays), title: bootstrapNote('stats.rollup_retention_days') },
                { label: 'Body capture', value: `at most ${plural(s.retention.bodyCaptureMaxHours, 'hour')}`, title: bootstrapNote('stats.body_capture_max_hours') },
                { label: 'Session expiry', value: `${fmtDuration(s.sessions.expireAfterSeconds * 1000)} (${s.sessions.expireAfterSeconds} s)`, title: bootstrapNote('sessions.expire_after_seconds') },
                { label: 'Non-companion timeout', value: `${fmtDuration(s.sessions.nonCompanionIdleSeconds * 1000)} (${s.sessions.nonCompanionIdleSeconds} s)`, title: bootstrapNote('sessions.non_companion_idle_seconds') },
                { label: 'Reset warning', value: s.usage.limitResetWarningDays === 0 ? 'off' : `${plural(s.usage.limitResetWarningDays, 'day')} before a reset ends`, title: bootstrapNote('usage.limit_reset_warning_days') },
                { label: 'Admin secret', value: <SecretBadge configured={s.access.adminConfigured} />, title: 'The secret the variable admin.secret_env names. It reaches every route.' },
              ]}
            />
            <p className="muted" style={{ marginTop: 10 }}>
              The values the configuration file sets are on the installation page, and <code>phoebis check-config</code> prints this router&apos;s.
            </p>
          </>
        ) : settingsPending}
      </Card>

      {/* Both subtitles share one cell and the copy control keeps its room while unshown, so the head holds one height
          under each tab and state and the switch below it stays where the last click left it. */}
      <Card
        className="client-config"
        title="Downstream client configuration"
        subtitle={
          <span className="client-config__subtitles">
            <span className={featureSetup ? 'client-config__subtitle client-config__unshown' : 'client-config__subtitle'}>
              Put it in the env block of the client&apos;s settings file or export it in the shell that launches the client, and replace the token with a key from the Keys page.
            </span>
            <span className={snippet === null || !featureSetup ? 'client-config__subtitle client-config__unshown' : 'client-config__subtitle'}>
              The launch script sets these lines. Give the script the base URL, a key from the Keys page and the fingerprint below.
            </span>
          </span>
        }
        actions={
          <span className={snippet === null ? 'client-config__unshown' : undefined}>
            <CopyButton text={snippet ?? ''} label="Copy snippet" size="md" />
          </span>
        }
      >
        <Tabs<Setup> label="Setup" value={setup} onChange={setSetup} tabs={SETUPS.map((value) => ({ controls: value === setup ? `${setupId}-panel` : undefined, id: `${setupId}-${value}`, label: SETUP_LABEL[value], value }))} />
        <div id={`${setupId}-panel`} role="tabpanel" aria-labelledby={`${setupId}-${setup}`}>
          {snippet === null ? (
            s ? (
              <p className="muted">
                This router serves no feature call until its configuration names <code>features.listen</code>.{' '}
                <a href={docsHref('installation', 'features')} target="_blank" rel="noreferrer">
                  Installation
                </a>
              </p>
            ) : settingsPending
          ) : (
            <>
              <pre className="snippet">{snippet}</pre>
              {s && (
                <p className="muted snippet__hint">
                  Inference is served on <code>{s.listeners.proxy}</code>, the address the proxy listener binds, so the base
                  URL carries its scheme and port. Replace the placeholder host with the name that reaches the listener, and
                  the scheme and the port too where a TLS proxy or a port mapping stands between Claude Code and it.
                </p>
              )}
              {featureSetup && (
                <p className="muted snippet__hint">
                  <a href={docsHref('claude-ai-features', 'the-launch-script')} target="_blank" rel="noreferrer">
                    The launch script
                  </a>{' '}
                  of the claude.ai features page sets these lines. The script fetches the CA, compares the fingerprint,
                  checks the road and writes the env file before it starts the client.
                  {setup === 'redirect' && " The machine's name resolution must lead api.anthropic.com and mcp-proxy.anthropic.com to the router's feature listener on port 443."}
                </p>
              )}
              {featureSetup && s?.features && <FeatureCaLine features={s.features} />}
              <dl className="envdl">
                {ENV_TERMS.filter((t) => t.always || named.has(t.term.split('=')[0])).map((t) => (
                  <div className="envdl__row" key={t.term}>
                    <dt>
                      <code>{t.term}</code>
                    </dt>
                    <dd>{featureSetup && t.feature !== undefined ? `${t.body} ${t.feature}` : t.body}</dd>
                  </div>
                ))}
              </dl>
            </>
          )}
        </div>
      </Card>
    </div>
  );
}
