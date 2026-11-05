/** The agent types and turn origins the client documents; a value outside them still lands on the row and in
 * the drawer, and the search text does not cover it. */
export const AGENT_TYPES = ['Explore', 'Plan', 'custom', 'fork', 'general-purpose', 'teammate'];
export const TURN_ORIGINS = ['auto_continuation', 'host_synthetic', 'human', 'peer', 'scheduled', 'sdk', 'system', 'task_notification', 'unknown'];

/**
 * A user agent's first token, its product and version: Claude Code's `claude-cli/<v>` reads `Claude Code <v>` and any
 * other token reads as sent. Null for a session with no user agent or a blank one.
 */
export function clientVersion(userAgent: string | null): string | null {
  const [token] = (userAgent ?? '').trim().split(/\s+/);
  if (token === '') return null;
  return token.startsWith('claude-cli/') ? `Claude Code ${token.slice('claude-cli/'.length)}` : token;
}
