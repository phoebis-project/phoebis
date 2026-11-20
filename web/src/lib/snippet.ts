import type { FeatureSettings } from '@/api/types';

/** What the operator replaces with the host name that reaches the router's inference listener. */
const PLACEHOLDER_HOST = 'router.example.com';

/** The client setups the card renders a block for: the plain base URL, and two that reach the claude.ai features. */
export const SETUPS = ['base', 'proxy', 'redirect'] as const;
export type Setup = (typeof SETUPS)[number];
export const SETUP_LABEL: Record<Setup, string> = { base: 'Base URL', proxy: 'Base URL and proxy', redirect: 'Base URL and DNS redirect' };

/** The names of the lines, which the card's glossary is keyed by, so a line without its term fails to compile. */
export type SnippetName =
  | 'ANTHROPIC_BASE_URL'
  | 'BASE_CA'
  | 'BASE_URL'
  | 'CLAUDE_CODE_GATEWAY_HINT_HEADERS'
  | 'CLAUDE_CODE_OAUTH_TOKEN'
  | 'CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK'
  | 'CLAUDE_CODE_SUBSCRIPTION_TYPE'
  | 'DISABLE_TELEMETRY'
  | 'PHOEBIS_FEATURE_CA_SHA256'
  | 'PLAN'
  | 'PRIVATE_HOSTS'
  | 'PROXY_URL'
  | 'ROUTER_KEY'
  | 'SETUP'
  | '_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL';
type SnippetLine = { name: SnippetName; value: string };

const snippetOf = (lines: SnippetLine[]) => ({ lines, text: lines.map((l) => `${l.name}=${l.value}`).join('\n') });

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
export function snippetFor(setup: Setup, listeners: { proxy: string; proxyTls: boolean } | null, features: FeatureSettings | null): { lines: SnippetLine[]; text: string } | null {
  const port = listeners === null ? '' : (/:(\d+)$/.exec(listeners.proxy)?.[1] ?? '');
  const scheme = listeners === null || listeners.proxyTls ? 'https' : 'http';
  const baseUrl = `${scheme}://${PLACEHOLDER_HOST}${port === '' ? '' : `:${port}`}`;
  if (setup === 'base') {
    return snippetOf([
      { name: 'ANTHROPIC_BASE_URL', value: baseUrl },
      { name: 'CLAUDE_CODE_OAUTH_TOKEN', value: '<key>' },
      { name: 'CLAUDE_CODE_SUBSCRIPTION_TYPE', value: 'max' },
      { name: '_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL', value: '1' },
      { name: 'CLAUDE_CODE_GATEWAY_HINT_HEADERS', value: '1' },
      { name: 'DISABLE_TELEMETRY', value: '1' },
      { name: 'CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK', value: '1' },
    ]);
  }
  if (features === null) return null;
  return snippetOf([
    { name: 'BASE_URL', value: `'${baseUrl}'` },
    { name: 'ROUTER_KEY', value: "'<key>'" },
    { name: 'PHOEBIS_FEATURE_CA_SHA256', value: `'${features.caFingerprint}'` },
    { name: 'SETUP', value: `'${setup}'` },
    { name: 'PROXY_URL', value: "'http://127.0.0.1:8318'" },
    { name: 'PRIVATE_HOSTS', value: "'no'" },
    { name: 'PLAN', value: "'max'" },
    { name: 'BASE_CA', value: "''" },
  ]);
}
