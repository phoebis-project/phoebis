import type {
  Account,
  AccountCatalog,
  AccountInput,
  AccountRollup,
  AffinitySource,
  AlertDeliveryTest,
  AlertEvent,
  AlertKind,
  AlertRule,
  AlertRuleInput,
  AnalyticsGroup,
  AnalyticsPoint,
  AnalyticsQuery,
  ApiKey,
  BudgetPeriod,
  BurnGroup,
  BurnPoint,
  CapturedBodies,
  CatalogModel,
  CreatedApiKey,
  Delivery,
  KeepaliveOverview,
  KeepaliveRecipe,
  KeyModels,
  LifecycleDelivery,
  LiveEvent,
  MemberBudget,
  ModelRule,
  ModelRulesMode,
  ModelsState,
  OveragePolicy,
  Overview,
  PairingInput,
  PairingPreview,
  PairingSession,
  PlacementRow,
  Pool,
  PoolCacheTtl,
  PoolKeepalive,
  PoolStrategy,
  PriceImportPreview,
  QuotaHistory,
  QuotaSnapshot,
  RequestDetail,
  RequestPage,
  RequestQuery,
  RewritePoint,
  Session,
  SessionInfo,
  Settings,
  TtlRules,
  WindowFit,
} from './types';
import { FAILOVER_PRESETS, SHEDDING_PRESETS } from '@/lib/presets';

export type { AccountInput, AlertRuleInput, AnalyticsQuery } from './types';

/** The account route's cancellation pair; the route refuses an end without the flag and the flag without an end. */
export interface AccountPatch {
  cancelled: boolean;
  /** An ISO instant, or null to clear it. */
  subscriptionEndsAt: string | null;
}

export interface KeyInput {
  affinitySource: AffinitySource;
  bodyCaptureHours: number | null;
  enabled: boolean;
  /** An ISO instant, or null for no expiry; undefined leaves the stored expiry alone. */
  expiresAt?: string | null;
  /** Null clears the cap; undefined leaves the stored one alone. */
  inFlightCap?: number | null;
  keepaliveOptOut: boolean;
  longContextTwins: boolean;
  modelRules: ModelRule[];
  modelRulesMode: ModelRulesMode;
  name: string;
  notes: string | null;
  poolId: string;
}

export interface CreateKeyInput extends KeyInput {
  expiresInDays: number | null;
}

/** Probe tuning a new pool starts with; the pool dialog shows these as its initial values. */
export const KEEPALIVE_TUNING_DEFAULTS: Omit<PoolKeepalive, 'costCapUsdPerDay' | 'enabled'> = {
  capWindowSharePerDay: null,
  coldProbeLimit: 2,
  jitterSeconds1h: 30,
  jitterSeconds5m: 5,
  leadSeconds1h: 360,
  leadSeconds5m: 60,
  maxIdleSeconds: 3_600,
  maxIdleTicks: 0,
  netNegativeGraceProbes: 2,
  retryMaxSeconds: 300,
  retrySeconds: 30,
  stopWhenNetNegative: true,
  warmSubagents: true,
};

/** Cache TTL policy a new pool starts with: off, so a pool's breakpoints pass through untouched until set. */
export const PLAN_TTL_DEFAULTS: TtlRules = { keepalive: '1h', main: '1h', models: [], subagent: '1h' };
export const METERED_TTL_DEFAULTS: TtlRules = { keepalive: '5m', main: '5m', models: [], subagent: '5m' };
export const CACHE_TTL_DEFAULTS: PoolCacheTtl = { enabled: false, injectWhenAbsent: false, metered: METERED_TTL_DEFAULTS, plan: PLAN_TTL_DEFAULTS };

export type PoolEngineFields = Pick<
  Pool,
  'burnWindowSeconds' | 'inFlightCap' | 'migrateWhenSpent' | 'overageMoveGraceMinutes' | 'paceFloor' | 'retryInPlaceSeconds' | 'shedShare' | 'sourceDwellSeconds' | 'warmSessions' | 'windDownMinutes'
>;

/** The store's shedding and failover defaults, which a new pool starts with. */
export const POOL_ENGINE_DEFAULTS: PoolEngineFields = {
  ...SHEDDING_PRESETS.balanced,
  ...FAILOVER_PRESETS.balanced,
  migrateWhenSpent: true,
  overageMoveGraceMinutes: 30,
  warmSessions: 'when_needed',
  windDownMinutes: 30,
};

export type PoolAffinityFields = Pick<Pool, 'affinityMode' | 'minSharedPrefixTokens' | 'quotaHeaders' | 'sessionPlacement' | 'subagentPlacement'>;

/** The store's pin, placement and quota header defaults, which a new pool starts with. */
export const POOL_AFFINITY_DEFAULTS: PoolAffinityFields = {
  affinityMode: 'always',
  minSharedPrefixTokens: 20_000,
  quotaHeaders: 'pass',
  sessionPlacement: 'by_strategy',
  subagentPlacement: 'by_cohort',
};

export interface PoolInput extends PoolEngineFields, PoolAffinityFields {
  affinityTtlSeconds: number;
  bodyCaptureHours: number;
  cacheDiagnostics: boolean | null;
  cacheTtl: PoolCacheTtl;
  keepalive: PoolKeepalive;
  /** A member list replaces the pool's whole membership, so a member sent without a budget clears its budget. */
  members: { accountId: string; budgetPeriod: BudgetPeriod; budgetUsd: number | null; priority: number }[];
  migrateAboveUtilization: number | null;
  modelRules: ModelRule[];
  name: string;
  overagePolicy: OveragePolicy;
  /** Order the selection set by account kind before priority; the operator's answer alone. */
  preferSubscription: boolean;
  selectBelowUtilization: number | null;
  status: 'active' | 'disabled';
  strategy: PoolStrategy;
}

export interface Api {
  /** Every budgeted member's spend in its current period; the route reads no range and no filter. */
  analyticsBudgets(): Promise<MemberBudget[]>;
  /** Sends the range and the key and pool lists; the route's other filters stay unset, so the page's others pass it by. */
  analyticsPlacements(query: Pick<AnalyticsQuery, 'from' | 'keyIds' | 'poolIds' | 'to'>): Promise<PlacementRow[]>;
  cancelPairing(pairingId: string): Promise<void>;
  clearCooldown(accountId: string, scope: string): Promise<void>;
  clearFunding(accountId: string): Promise<void>;
  completePairing(pairingId: string, input: PairingInput): Promise<PairingPreview>;
  confirmPairing(pairingId: string): Promise<void>;
  confirmPriceImport(token: string): Promise<Settings['pricing']>;
  createAccount(input: AccountInput): Promise<Account>;
  createAlertRule(input: AlertRuleInput): Promise<AlertRule>;
  createKey(input: CreateKeyInput): Promise<CreatedApiKey>;
  createPool(input: PoolInput): Promise<Pool>;
  /** Removes the upstream from every pool and deletes it; its request history stays in the ledger. */
  deleteAccount(accountId: string): Promise<void>;
  deleteAlertRule(id: string): Promise<void>;
  deleteKey(id: string): Promise<void>;
  deletePool(id: string): Promise<void>;
  /** Deletes the credential; the upstream keeps its pool memberships and can be paired again. */
  disconnectAccount(accountId: string): Promise<void>;
  /** `refresh` reads the upstream again before answering rather than the cache. */
  getAccountCatalog(accountId: string, refresh?: boolean): Promise<AccountCatalog>;
  getAnalyticsBreakdown(query: AnalyticsQuery): Promise<AnalyticsGroup[]>;
  /** The route reads the range and the grouping alone; no other filter of the query narrows it. */
  getAnalyticsBurn(range: Pick<AnalyticsQuery, 'from' | 'to'>, groupBy: BurnGroup): Promise<{ bucketSeconds: number; points: BurnPoint[] }>;
  /** The live fits, which hold no range. */
  getAnalyticsFits(): Promise<WindowFit[]>;
  getAnalyticsRewrites(range: Pick<AnalyticsQuery, 'from' | 'to'>): Promise<{ moves: Record<string, number>; points: RewritePoint[] }>;
  getAnalyticsSeries(query: AnalyticsQuery): Promise<AnalyticsPoint[]>;
  getCapturedBodies(requestId: string): Promise<CapturedBodies | null>;
  getKeepaliveOverview(): Promise<KeepaliveOverview>;
  getKeyModels(keyId: string): Promise<KeyModels>;
  getModelsState(): Promise<ModelsState>;
  getOverview(): Promise<Overview>;
  getQuotaHistory(accountId: string): Promise<QuotaHistory[]>;
  /** Null for a request the ledger no longer holds. */
  getRequest(id: string): Promise<RequestDetail | null>;
  /** Null when no session cookie is valid, which is what puts the login screen up. */
  getSession(): Promise<SessionInfo | null>;
  getSettings(): Promise<Settings>;
  listAccountRollups(from: string, to: string): Promise<AccountRollup[]>;
  listAccounts(): Promise<Account[]>;
  listAlertDeliveries(eventId: string): Promise<Delivery[]>;
  listAlertEvents(state?: 'open' | 'resolved'): Promise<AlertEvent[]>;
  listAlertRules(): Promise<AlertRule[]>;
  listCatalog(): Promise<CatalogModel[]>;
  /** The union by id of the named accounts' live catalogs; a failed fetch contributes nothing. */
  listCatalogOf(accountIds: string[]): Promise<CatalogModel[]>;
  listKeepaliveRecipes(): Promise<KeepaliveRecipe[]>;
  listKeys(): Promise<ApiKey[]>;
  /** The session events as they were posted; a lifecycle rule opens no event, so this log is all there is to read. */
  listLifecycleDeliveries(kind?: AlertKind): Promise<LifecycleDelivery[]>;
  listPools(): Promise<Pool[]>;
  listQuotaSnapshots(): Promise<QuotaSnapshot[]>;
  listRequests(query?: RequestQuery): Promise<RequestPage>;
  listSessions(): Promise<Session[]>;
  login(secret: string): Promise<SessionInfo>;
  logout(): Promise<void>;
  pauseAccount(accountId: string): Promise<void>;
  /** The rows a catalog at this URL yields, already converted to USD per million and reduced to models the router serves, with the counts behind that filter. */
  previewPriceImport(url: string): Promise<PriceImportPreview>;
  refreshUsage(accountId: string): Promise<void>;
  releaseSession(keyId: string, sessionId: string): Promise<void>;
  renameAccount(accountId: string, label: string): Promise<Account>;
  resolveAlertEvent(id: string): Promise<AlertEvent>;
  resumeAccount(accountId: string): Promise<void>;
  revokeKey(id: string): Promise<void>;
  rotateKey(id: string): Promise<CreatedApiKey>;
  startPairing(accountId: string): Promise<PairingSession>;
  /** Delivers a snapshot first, then deltas; returns the unsubscribe function. */
  subscribeLive(handler: (event: LiveEvent) => void): () => void;
  testAlertRule(id: string): Promise<AlertDeliveryTest>;
  updateAccount(accountId: string, patch: AccountPatch): Promise<Account>;
  updateAlertRule(id: string, input: AlertRuleInput): Promise<AlertRule>;
  updateKey(id: string, input: KeyInput): Promise<ApiKey>;
  updatePool(id: string, input: PoolInput): Promise<Pool>;
  verifyAccount(accountId: string): Promise<{ ok: boolean; status: number }>;
}
