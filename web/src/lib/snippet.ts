import type { FeatureSettings } from '@/api/types';

/** What the operator replaces with the host name that reaches the router's inference listener. */
const PLACEHOLDER_HOST = 'router.example.com';

/** The client setups the card renders a block for: the plain base URL, and two that reach the claude.ai features. */
export const SETUPS = ['base', 'proxy', 'redirect'] as const;
export type Setup = (typeof SETUPS)[number];
export const SETUP_LABEL: Record<Setup, string> = { base: 'Base URL', proxy: 'Base URL and proxy', redirect: 'Base URL and DNS redirect' };

/** Quoted in the line, since the list holds spaces and the block shows the lines as the launch script sets them. */
const FEATURE_SCOPES = 'user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload';

/**
 * The lines of one setup: the seven that point Claude Code at the router, and for a feature setup the lines the launch
 * script sets beside them.
 *
 * `ANTHROPIC_BASE_URL` takes its scheme from whether the proxy listener terminates TLS and its port from the
 * listener's bind, the two parts the router knows. The host stays a placeholder, since the router knows no host a
 * client reaches it at. Without a settings answer the URL reads the whole placeholder, https with no port, and a bind
 * that names no port reads none. The router knows no client's proxy and no client's files either, so those lines carry
 * the proxy's default address and marked places.
 *
 * Null for a feature setup without the answer's `features` member, which is a router without the feature listener or
 * an answer that has not arrived. The page tells the two apart by its own read's state.
 */
export function snippetFor(setup: Setup, listeners: { proxy: string; proxyTls: boolean } | null, features: FeatureSettings | null): string | null {
  const port = listeners === null ? '' : (/:(\d+)$/.exec(listeners.proxy)?.[1] ?? '');
  const scheme = listeners === null || listeners.proxyTls ? 'https' : 'http';
  const baseUrl = `${scheme}://${PLACEHOLDER_HOST}${port === '' ? '' : `:${port}`}`;
  const base = [`ANTHROPIC_BASE_URL=${baseUrl}`, 'CLAUDE_CODE_OAUTH_TOKEN=<key>', 'CLAUDE_CODE_SUBSCRIPTION_TYPE=max', '_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1', 'CLAUDE_CODE_GATEWAY_HINT_HEADERS=1', 'DISABLE_TELEMETRY=1', 'CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1'];
  if (setup === 'base') return base.join('\n');
  if (features === null) return null;
  // The redirect leads the two feature hosts to the router by name resolution, so it sets no proxy at all.
  const proxied = setup === 'proxy';
  return [
    ...base,
    `CLAUDE_CODE_OAUTH_SCOPES="${FEATURE_SCOPES}"`,
    'CLAUDE_ENV_FILE=<path of the env file>',
    ...(proxied ? ['HTTPS_PROXY=http://127.0.0.1:8318'] : []),
    'NODE_EXTRA_CA_CERTS=<path of feature-ca.pem>',
    ...(proxied ? [`NO_PROXY=${PLACEHOLDER_HOST},localhost,127.0.0.1,::1`] : []),
    `PHOEBIS_FEATURE_CA_SHA256=${features.caFingerprint}`,
  ].join('\n');
}
