import type { FeatureSettings } from '@/api/types';

/** What the operator replaces with the host name that reaches the router's inference listener. */
const PLACEHOLDER_HOST = 'router.example.com';

/** The client setups the card renders a block for: the plain base URL, and two that reach the claude.ai features. */
export const SETUPS = ['base', 'proxy', 'redirect'] as const;
export type Setup = (typeof SETUPS)[number];
export const SETUP_LABEL: Record<Setup, string> = { base: 'Base URL', proxy: 'Base URL and proxy', redirect: 'Base URL and DNS redirect' };

/**
 * The lines of one setup: the seven that point Claude Code at the router, and for a feature setup the eight input lines
 * of the launch script in its order, which a reader pastes over the inputs at the script's head. The script sets the
 * client's environment itself, so a feature setup prints none of it.
 *
 * The base URL takes its scheme from whether the proxy listener terminates TLS and its port from the listener's bind,
 * the two parts the router knows. The host stays a placeholder, since the router knows no host a client reaches it at.
 * Without a settings answer the URL reads the whole placeholder, https with no port, and a bind that names no port
 * reads none. A feature setup's lines carry the router's fingerprint and the tab's setup word. The key stays a
 * placeholder, since the router stores a hash of a key's secret and shows the secret once, and the proxy's address is
 * its default, since the router knows no client's proxy. The lines carry none of the script's comments, which keep
 * their one place in the script.
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
  return [
    `BASE_URL='${baseUrl}'`,
    "ROUTER_KEY='<key>'",
    `PHOEBIS_FEATURE_CA_SHA256='${features.caFingerprint}'`,
    `SETUP='${setup}'`,
    "PROXY_URL='http://127.0.0.1:8318'",
    "PRIVATE_HOSTS='no'",
    "PLAN='max'",
    "BASE_CA=''",
  ].join('\n');
}
