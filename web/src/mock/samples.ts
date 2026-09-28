import { decodeAccount, decodeAccountRollups, decodeAlertEvent, decodeAlertRule, decodeAnalyticsGroup, decodeDashboard, decodeDelivery, decodeKeepaliveOverview, decodeKey, decodeLifecycleDelivery, decodeLiveRequest, decodePairingSession, decodePool, decodeQuotaHistory, decodeRecipe, decodeRequest, decodeRequestEvent, decodeSession, decodeSettings, decodeSnapshot } from '@/api/decode';
import { FIXTURES } from './generated/fixtures';

/** The fixture's body, or the named member of it, as the decoder takes it. */
const body = (name: string): Record<string, unknown> => FIXTURES[name].body as Record<string, unknown>;
const first = (name: string, member: string): unknown => (body(name)[member] as unknown[])[0];

export const ACCOUNT_SAMPLE = decodeAccount(body('account'));
export const ACCOUNT_ROLLUP_SAMPLE = decodeAccountRollups(body('analytics_accounts'))[0];
export const ALERT_EVENT_SAMPLE = decodeAlertEvent(first('alert_events', 'events'));
export const ALERT_RULE_SAMPLE = decodeAlertRule(first('alert_rules', 'rules'));
export const ANALYTICS_GROUP_SAMPLE = decodeAnalyticsGroup(first('analytics_breakdown', 'rows'));
export const DELIVERY_SAMPLE = decodeDelivery(first('alert_deliveries', 'deliveries'));
export const KEEPALIVE_OVERVIEW_SAMPLE = decodeKeepaliveOverview(body('keepalive_overview'));
export const KEY_SAMPLE = decodeKey(first('keys', 'keys'));
export const LIFECYCLE_DELIVERY_SAMPLE = decodeLifecycleDelivery(first('alert_lifecycle', 'deliveries'));
export const LIVE_REQUEST_SAMPLE = decodeLiveRequest(((body('live_messages').deltas as { delta: { request: unknown } }[])[0]).delta.request);
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
