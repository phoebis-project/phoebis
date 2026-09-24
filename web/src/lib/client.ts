/** The agent types and turn origins the client documents; a value outside them still lands on the row and in
 * the drawer, and the search text does not cover it. */
export const AGENT_TYPES = ['Explore', 'Plan', 'custom', 'fork', 'general-purpose', 'teammate'];
export const TURN_ORIGINS = ['auto_continuation', 'host_synthetic', 'human', 'peer', 'scheduled', 'sdk', 'system', 'task_notification', 'unknown'];

/** The classes the relay derives for the entrypoints it names (`cli`, `sdk-cli`, `claude-vscode`), the SDK language
 * the fixtures carry, and any other caller; the request log sizes its Client column to the longest of them. */
export const CLIENT_CLASSES = ['claude-code/claude-vscode', 'claude-code/cli', 'claude-code/sdk-cli', 'other', 'sdk/python'];
export const CLIENT_CLASS_CHARS = Math.max(...CLIENT_CLASSES.map((c) => c.length));
