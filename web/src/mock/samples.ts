import { decodeAccount, decodeAccountRollups, decodeAnalyticsGroup, decodeAttention, decodeDashboard, decodeKeepaliveOverview, decodeKey, decodePairingSession, decodePool, decodeQuotaHistory, decodeRecipe, decodeRequest, decodeRequestEvent, decodeSession, decodeSettings, decodeSnapshot } from '@/api/decode';
import type { SubscriptionCosts } from '@/api/types';
import { withRegime } from '@/lib/costs';
import { FIXTURES } from './generated/fixtures';

/** The fixture's body, or the named member of it, as the decoder takes it. */
const body = (name: string): Record<string, unknown> => FIXTURES[name].body as Record<string, unknown>;
const first = (name: string, member: string): unknown => (body(name)[member] as unknown[])[0];

export const ACCOUNT_SAMPLE = decodeAccount(body('account'));
export const ACCOUNT_ROLLUP_SAMPLE = decodeAccountRollups(body('analytics_accounts'))[0];
export const ANALYTICS_GROUP_SAMPLE = decodeAnalyticsGroup(first('analytics_breakdown', 'rows'));
export const ATTENTION_ITEM_SAMPLE = decodeAttention(first('attention', 'items'));
export const KEEPALIVE_OVERVIEW_SAMPLE = decodeKeepaliveOverview(body('keepalive_overview'));
export const KEY_SAMPLE = decodeKey(first('keys', 'keys'));
export const OVERVIEW_SAMPLE = decodeDashboard(body('dashboard'));
export const PAIRING_SAMPLE = decodePairingSession(body('pairing_started'));
export const POOL_SAMPLE = decodePool(first('pools', 'pools'));
export const QUOTA_HISTORY_SAMPLE = decodeQuotaHistory(body('account_quota'))[0];
export const QUOTA_SNAPSHOT_SAMPLE = decodeSnapshot('business', body('account_usage'));
export const RECIPE_SAMPLE = decodeRecipe(first('keepalive_recipes', 'recipes'));
export const REQUEST_EVENT_SAMPLE = decodeRequestEvent(first('request', 'events'));
export const REQUEST_SAMPLE = decodeRequest(body('request').request);
export const SESSION_SAMPLE = decodeSession(first('sessions', 'sessions'));
export const SETTINGS_SAMPLE = decodeSettings(body('settings'));
/** The subscription cost model the harness captured, the shipped one, which every mock share is priced through. */
export const TABLES = SETTINGS_SAMPLE.subscriptionCosts;

/** The label prefix the ledger strips to read a client's entrypoint, as `entrypoint_of_client` does. */
const ENTRYPOINT_PREFIX = 'claude-code/';

/** The tables a request prices under: the regime whose client list names the label over the base, as the router
 *  prices it, and the base for a label no regime names. A regime lists entrypoint tokens such as `cli` while a row's
 *  label reads `claude-code/cli`, so the helper takes the token off the label and names the regime whose
 *  `entrypoints` hold it. */
export const tablesFor = (client: string | null, costs: SubscriptionCosts = TABLES): SubscriptionCosts => {
  const token = client?.startsWith(ENTRYPOINT_PREFIX) ? client.slice(ENTRYPOINT_PREFIX.length) : null;
  const regime = token === null ? undefined : Object.keys(costs.regimes).find((name) => costs.regimes[name].entrypoints.includes(token));
  return withRegime(costs, regime ?? null);
};
