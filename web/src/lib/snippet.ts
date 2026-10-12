/** What the operator replaces with the host name that reaches the router's inference listener. */
const PLACEHOLDER_HOST = 'router.example.com';

/**
 * The seven lines that point Claude Code at the router.
 *
 * `ANTHROPIC_BASE_URL` takes its scheme from whether the proxy listener terminates TLS and its port from the
 * listener's bind, the two parts the router knows. The host stays a placeholder, since the router knows no host a
 * client reaches it at. Without a settings answer the URL reads the whole placeholder, https with no port, and a bind
 * that names no port reads none.
 */
export function snippetFor(listeners: { proxy: string; proxyTls: boolean } | null): string {
  const port = listeners === null ? '' : (/:(\d+)$/.exec(listeners.proxy)?.[1] ?? '');
  const scheme = listeners === null || listeners.proxyTls ? 'https' : 'http';
  const baseUrl = `${scheme}://${PLACEHOLDER_HOST}${port === '' ? '' : `:${port}`}`;
  return [`ANTHROPIC_BASE_URL=${baseUrl}`, 'CLAUDE_CODE_OAUTH_TOKEN=<key>', 'CLAUDE_CODE_SUBSCRIPTION_TYPE=max', '_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1', 'CLAUDE_CODE_GATEWAY_HINT_HEADERS=1', 'DISABLE_TELEMETRY=1', 'CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1'].join('\n');
}
