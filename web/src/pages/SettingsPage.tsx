import { api } from '@/api';
import { effectiveLabel } from '@/api/decode';
import { useAppSettings, type PaletteChoice, type ThemeChoice } from '@/app/settings';
import { IconMonitor, IconMoon, IconSun } from '@/components/ui/icons';
import { Badge, Card, CopyButton, ErrorState, KeyValue, Segmented, Select, Skeleton } from '@/components/ui/primitives';
import { UnitLegend, WeekSwitch } from '@/components/ui/UnitControls';
import { revisionAt } from '@/lib/costs';
import { fmtDuration, plural, type TimeFormat } from '@/lib/format';
import { useAsync, useNow } from '@/lib/hooks';
import { snippetFor } from '@/lib/snippet';
import { positionOf, UNIT_LABEL, UNIT_POSITIONS } from '@/lib/unit';

const ENV_TERMS: { body: string; term: string }[] = [
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
  },
  {
    term: '_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1',
    body: 'Makes Claude Code send its first-party request shape to the router: the billing hash, the cache diagnostics beta, and the first-party betas. Without it the client sends its reduced shape and the router restores the fields before relaying it. It also switches off gateway model discovery, and being an internal variable a future build may drop it.',
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
    term: 'CLAUDE_CODE_PROMPT_CACHE_TTL=5m',
    body: 'Worth setting for Fable 5.2 and Fable 5.1 sessions in a pool with cache warming on: at a 0.025x read price a five-minute entry kept warm by probes stays cheaper than a one-hour entry. Leave it unset without warming.',
  },
];

/** These come from the configuration file and the admin API only reads them, so each says where it is set. */
const bootstrapNote = (setting: string) => `Set by ${setting} in the configuration file`;

/** About's words for a retention key's day count, where 0 keeps the rows forever. */
const keptFor = (days: number) => (days === 0 ? 'kept forever' : `kept for ${plural(days, 'day')}`);

/**
 * A table's date in force, its revision count and the next date while one is pending, read at one instant by the rule
 * the cost reference's select reads. An answer that lists no revision names the date it sent alone.
 */
const revisionsNote = (revisions: { effective: string; effectiveDate: string | null }[], sent: string, now: number): string => {
  const inForce = revisionAt(revisions, now);
  if (inForce === null) return `effective ${effectiveLabel(sent)}`;
  const pending = revisions.find((r) => r !== inForce && r.effectiveDate !== null && Date.parse(r.effectiveDate) > now);
  const count = plural(revisions.length, 'revision');
  return `effective ${effectiveLabel(inForce.effective)}, ${count}${pending === undefined ? '' : `, next ${effectiveLabel(pending.effective)}`}`;
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
  const s = settings.data;
  const snippet = snippetFor(s?.listeners ?? null);

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
                { label: 'API prices path', value: `${sourceOf(s.apiPrices.source, s.storage.apiPricesPath).text}, ${revisionsNote(s.apiPrices.revisions, s.apiPrices.effective, now)}`, mono: sourceOf(s.apiPrices.source, s.storage.apiPricesPath).mono, title: bootstrapNote('stats.api_prices_path') },
                { label: 'Subscription costs path', value: `${sourceOf(s.subscriptionCosts.source, s.storage.subscriptionCostsPath).text}${s.subscriptionCosts.problem === null ? `, ${revisionsNote(s.subscriptionCosts.revisions, s.subscriptionCosts.effective, now)}` : `. ${s.subscriptionCosts.problem}`}`, mono: s.subscriptionCosts.problem === null && sourceOf(s.subscriptionCosts.source, s.storage.subscriptionCostsPath).mono, title: bootstrapNote('stats.subscription_costs_path') },
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
        ) : settings.error ? (
          <ErrorState title="Could not load the settings" body={settings.error} retry={settings.reload} />
        ) : (
          <Skeleton height={60} />
        )}
      </Card>

      <Card
        title="Downstream client configuration"
        subtitle="Put it in the env block of the client's settings file or export it in the shell that launches the client, and replace the token with a key from the Keys page."
        actions={<CopyButton text={snippet} label="Copy snippet" size="md" />}
      >
        <pre className="snippet">{snippet}</pre>
        {s && (
          <p className="muted snippet__hint">
            Inference is served on <code>{s.listeners.proxy}</code>, the address the proxy listener binds, so the base URL
            carries its scheme and port. Replace the placeholder host with the name that reaches the listener, and the
            scheme and the port too where a TLS proxy or a port mapping stands between Claude Code and it.
          </p>
        )}
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
    </div>
  );
}
