/** What the operator replaces with the URL that reaches the router's inference listener. */
export const PLACEHOLDER_URL = 'https://router.example.com';

/**
 * The seven lines that point Claude Code at the router.
 *
 * `ANTHROPIC_BASE_URL` stays a placeholder: the router knows the address its proxy listener binds, never the URL a
 * client reaches it at, which a TLS proxy, a port mapping or a host name decides. A bind address copied into the
 * block could send the router key and the request content to a backend port or over plain HTTP, so the page shows
 * it beside the block as a hint.
 */
export function snippetFor(): string {
  return [`ANTHROPIC_BASE_URL=${PLACEHOLDER_URL}`, 'CLAUDE_CODE_OAUTH_TOKEN=<key>', 'CLAUDE_CODE_SUBSCRIPTION_TYPE=max', '_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1', 'CLAUDE_CODE_GATEWAY_HINT_HEADERS=1', 'DISABLE_TELEMETRY=1', 'CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1'].join('\n');
}
