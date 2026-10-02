/** What the operator replaces with the URL that reaches the router's inference listener. */
export const PLACEHOLDER_URL = 'https://router.example.com';

/**
 * The environment block that points Claude Code at the router.
 *
 * `ANTHROPIC_BASE_URL` stays a placeholder: the only origin the router knows of itself is the admin one, which
 * serves the console and the admin routes rather than the inference endpoints, and nothing tells it how its
 * proxy listener is reached from outside. When that listener's address is known, the block names it, so the
 * operator can tell which of the two listeners the variable has to reach.
 */
export function snippetFor(proxyListen: string | null): string {
  const hint = proxyListen === null ? [] : [`# inference is served on ${proxyListen}; use the URL that reaches that listener`];
  return [...hint, `ANTHROPIC_BASE_URL=${PLACEHOLDER_URL}`, 'CLAUDE_CODE_OAUTH_TOKEN=<key>', '_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1'].join('\n');
}
