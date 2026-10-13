import type {
  Account,
  AccountCatalog,
  AccountInput,
  AccountRollup,
  AnalyticsGroup,
  AnalyticsPoint,
  AnalyticsQuery,
  ApiKey,
  AttentionItem,
  BurnGroup,
  BurnPoint,
  CapturedBodies,
  CatalogModel,
  CreatedApiKey,
  DecisionRecord,
  KeepaliveOverview,
  KeepaliveRecipe,
  KeyModels,
  MemberBudget,
  ModelRule,
  ModelRulesMode,
  ModelsState,
  OveragePolicy,
  Overview,
  PairingInput,
  PairingPreview,
  PairingSession,
  PairingStart,
  PlacementRow,
  Pool,
  PoolCacheTtl,
  PoolKeepalive,
  PoolStrategy,
  QuotaHistory,
  ReplayOutcome,
  RequestDetail,
  RequestPage,
  RequestQuery,
  RewritePoint,
  RoutingLogPage,
  RoutingLogQuery,
  Session,
  SessionInfo,
  Settings,
  TtlRules,
} from './types';
import { FAILOVER_PRESETS, SHEDDING_PRESETS } from '@/lib/presets';

export type { AccountInput, AnalyticsQuery } from './types';

/** The account route's cancellation pair; the route refuses an end without the flag and the flag without an end. */
export interface AccountPatch {
  cancelled: boolean;
  /** An ISO instant, or null to clear it. */
  subscriptionEndsAt: string | null;
}

export interface KeyInput {
  autoModeServer: boolean | null;
  bodyCaptureHours: number | null;
  enabled: boolean;
  /** An ISO instant, or null for no expiry; undefined leaves the stored expiry alone. */
  expiresAt?: string | null;
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

/** The keepalive ceilings a new pool starts with, which the pool dialog shows as its initial values. */
export const KEEPALIVE_DEFAULTS: Omit<PoolKeepalive, 'enabled'> = { maxIdleSeconds: 3_600, maxParkedSeconds: null, scheduledHorizonSeconds: null };

/** Cache TTL policy a new pool starts with: off, so a pool's breakpoints pass through untouched until set. */
export const PLAN_TTL_DEFAULTS: TtlRules = { main: '1h', models: [], subagent: '1h' };
export const METERED_TTL_DEFAULTS: TtlRules = { main: '5m', models: [], subagent: '5m' };
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

export type PoolAffinityFields = Pick<Pool, 'affinityMode' | 'minSharedPrefixTokens' | 'sessionPlacement' | 'subagentPlacement'>;

/** The store's pin and placement defaults, which a new pool starts with. */
export const POOL_AFFINITY_DEFAULTS: PoolAffinityFields = {
  affinityMode: 'always',
  minSharedPrefixTokens: 20_000,
  sessionPlacement: 'by_strategy',
  subagentPlacement: 'by_strategy',
};

export interface PoolInput extends PoolEngineFields, PoolAffinityFields {
  affinityTtlSeconds: number;
  autoModeServer: boolean;
  bodyCaptureHours: number;
  cacheDiagnostics: boolean;
  cacheTtl: PoolCacheTtl;
  keepalive: PoolKeepalive;
  /** A member list replaces the pool's whole membership, so a member sent without a budget clears its budget. */
  members: { accountId: string; budgetDailyUsd: number | null; budgetMonthlyUsd: number | null; priority: number }[];
  modelRules: ModelRule[];
  name: string;
  overagePolicy: OveragePolicy;
  /** Order the selection set by account kind before priority; the operator's answer alone. */
  preferSubscription: boolean;
  selectBelowUtilization: number | null;
  /** Null disables the shedding round. */
  shedAheadOfPace: number | null;
  status: 'active' | 'disabled';
  strategy: PoolStrategy;
}

export interface Api {
  /** Every budgeted member's spend in its current period; the route reads no range and no filter. */
  analyticsBudgets(): Promise<MemberBudget[]>;
  /**
   * Sends the range and the key and pool lists alone: the page's other filters do not narrow this card, though the
   * route would take them.
   */
  analyticsPlacements(query: Pick<AnalyticsQuery, 'from' | 'keyIds' | 'poolIds' | 'to'>): Promise<PlacementRow[]>;
  cancelPairing(pairingId: string): Promise<void>;
  clearCooldown(accountId: string, scope: string): Promise<void>;
  clearFunding(accountId: string): Promise<void>;
  completePairing(pairingId: string, input: PairingInput): Promise<PairingPreview>;
  confirmPairing(pairingId: string): Promise<void>;
  createAccount(input: AccountInput): Promise<Account>;
  createKey(input: CreateKeyInput): Promise<CreatedApiKey>;
  createPool(input: PoolInput): Promise<Pool>;
  /** Removes the account from every pool and deletes it; its request history stays in the ledger. */
  deleteAccount(accountId: string): Promise<void>;
  deleteKey(id: string): Promise<void>;
  deletePool(id: string): Promise<void>;
  /** Deletes the credential; the account keeps its pool memberships and can be paired again. */
  disconnectAccount(accountId: string): Promise<void>;
  /** `refresh` reads the upstream again before answering rather than the cache. */
  getAccountCatalog(accountId: string, refresh?: boolean): Promise<AccountCatalog>;
  /** `funding` keeps the billed or the plan-funded rows. Under the account grouping a subscription account's row carries its shares. */
  getAnalyticsBreakdown(query: AnalyticsQuery): Promise<AnalyticsGroup[]>;
  /**
   * The route reads the range, the grouping, the Accounts filter and the user agent groups, and no other filter of the
   * page narrows it. Naming exactly one account answers that account's shares and unit under any grouping.
   */
  getAnalyticsBurn(range: Pick<AnalyticsQuery, 'from' | 'to'>, groupBy: BurnGroup, filters?: Pick<AnalyticsQuery, 'accountIds' | 'userAgentGroups'>): Promise<{ bucketSeconds: number; points: BurnPoint[] }>;
  getAnalyticsRewrites(range: Pick<AnalyticsQuery, 'from' | 'to'>): Promise<{ moves: Record<string, number>; points: RewritePoint[] }>;
  /** `funding` keeps the billed or the plan-funded rows. Under the account grouping each point's subscription groups carry their shares. */
  getAnalyticsSeries(query: AnalyticsQuery): Promise<AnalyticsPoint[]>;
  /** The conditions that hold now, worst first and the oldest first within a kind: the route's order is the ranking. */
  getAttention(): Promise<AttentionItem[]>;
  getCapturedBodies(requestId: string): Promise<CapturedBodies | null>;
  /** Null for a decision the ledger no longer holds. */
  getDecision(requestId: string, seq: number): Promise<DecisionRecord | null>;
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
  listCatalog(): Promise<CatalogModel[]>;
  /** The union by id of the named accounts' live catalogs; a failed fetch contributes nothing. */
  listCatalogOf(accountIds: string[]): Promise<CatalogModel[]>;
  listKeepaliveRecipes(): Promise<KeepaliveRecipe[]>;
  listKeys(): Promise<ApiKey[]>;
  listPools(): Promise<Pool[]>;
  listRequests(query?: RequestQuery): Promise<RequestPage>;
  listRoutingLog(query?: RoutingLogQuery): Promise<RoutingLogPage>;
  listSessions(): Promise<Session[]>;
  login(secret: string): Promise<SessionInfo>;
  logout(): Promise<void>;
  pauseAccount(accountId: string): Promise<void>;
  refreshUsage(accountId: string): Promise<void>;
  releaseSession(keyId: string, sessionId: string): Promise<void>;
  /** `revision` is the one the row was read at; a save over a row that moved since answers 409 `stale_edit`. */
  renameAccount(accountId: string, label: string, revision?: number): Promise<Account>;
  /** `POST /admin/ledger/parked/replay`; a `503` under a held writer is thrown as an `ApiError` the caller draws. */
  replayParked(): Promise<ReplayOutcome>;
  resumeAccount(accountId: string): Promise<void>;
  revokeKey(id: string): Promise<void>;
  rotateKey(id: string): Promise<CreatedApiKey>;
  startPairing(start: PairingStart): Promise<PairingSession>;
  /** `revision` is the one the row was read at; a save over a row that moved since answers 409 `stale_edit`. */
  updateAccount(accountId: string, patch: AccountPatch, revision?: number): Promise<Account>;
  /** `revision` is the one the row was read at. A save over a row that moved since answers 409 `stale_edit`. */
  updateKey(id: string, input: KeyInput, revision?: number): Promise<ApiKey>;
  updatePool(id: string, input: PoolInput, revision?: number): Promise<Pool>;
  verifyAccount(accountId: string): Promise<{ ok: boolean; status: number }>;
}
