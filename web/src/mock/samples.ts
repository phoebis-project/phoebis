import { decodeAccount, decodeAccountRollups, decodeAnalyticsGroup, decodeAttention, decodeDashboard, decodeKeepaliveOverview, decodeKey, decodePairingSession, decodePool, decodeQuotaHistory, decodeRecipe, decodeRequest, decodeRequestEvent, decodeSession, decodeSettings, decodeSnapshot, perFeature } from '@/api/decode';
import type { ApiKey, FeatureAccountIds, FeatureWord, KeyFeatureAccount, Pool, SubscriptionCosts } from '@/api/types';
import { withRegime } from '@/lib/costs';
import { INHERITED_FEATURE } from '@/lib/forms';
import { FIXTURES } from './generated/fixtures';

/** The fixture's body, or the named member of it, as the decoder takes it. */
const body = (name: string): Record<string, unknown> => FIXTURES[name].body as Record<string, unknown>;
const first = (name: string, member: string): unknown => (body(name)[member] as unknown[])[0];

export const ACCOUNT_SAMPLE = decodeAccount(body('account'));
export const ACCOUNT_ROLLUP_SAMPLE = decodeAccountRollups(body('analytics_accounts'))[0];
export const ANALYTICS_GROUP_SAMPLE = decodeAnalyticsGroup(first('analytics_breakdown', 'rows'));
export const ATTENTION_ITEM_SAMPLE = decodeAttention(first('attention', 'items'));
export const INHERITED_FEATURE_ACCOUNTS: Record<FeatureWord, KeyFeatureAccount> = perFeature(() => INHERITED_FEATURE);
export const KEEPALIVE_OVERVIEW_SAMPLE = decodeKeepaliveOverview(body('keepalive_overview'));
/** With no feature account of its own, since the captured selections name accounts the mock lacks. */
export const KEY_SAMPLE: ApiKey = {
  ...decodeKey(first('keys', 'keys')),
  featureAccounts: INHERITED_FEATURE_ACCOUNTS,
};
export const NO_FEATURE_ACCOUNTS: FeatureAccountIds = perFeature(() => null);
export const OVERVIEW_SAMPLE = decodeDashboard(body('dashboard'));
export const PAIRING_SAMPLE = decodePairingSession(body('pairing_started'));
/** With no feature account, since the captured selections name accounts the mock lacks. */
export const POOL_SAMPLE: Pool = { ...decodePool(first('pools', 'pools')), featureAccounts: NO_FEATURE_ACCOUNTS };
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

/** The regime whose client list names the label, and null, the base, for a label no regime names. A regime lists
 *  entrypoint tokens such as `cli` while a row's label reads `claude-code/cli`, so the helper takes the token off the
 *  label and names the regime whose `entrypoints` hold it. */
export const regimeFor = (client: string | null, costs: SubscriptionCosts = TABLES): string | null => {
  const token = client?.startsWith(ENTRYPOINT_PREFIX) ? client.slice(ENTRYPOINT_PREFIX.length) : null;
  if (token === null) return null;
  return Object.keys(costs.regimes).find((name) => costs.regimes[name].entrypoints.includes(token)) ?? null;
};

/** The tables a request prices under: the regime whose client list names the label over the base, as the router
 *  prices it, and the base for a label no regime names. */
export const tablesFor = (client: string | null, costs: SubscriptionCosts = TABLES): SubscriptionCosts => withRegime(costs, regimeFor(client, costs));
