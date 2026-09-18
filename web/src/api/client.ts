import type {
  Account,
  AccountInput,
  AccountRollup,
  AlertDeliveryTest,
  AlertEvent,
  AlertRule,
  AlertRuleInput,
  AnalyticsGroup,
  AnalyticsPoint,
  AnalyticsQuery,
  ApiKey,
  CapturedBodies,
  CatalogModel,
  CreatedApiKey,
  Delivery,
  KeepaliveOverview,
  KeepaliveRecipe,
  LiveEvent,
  OveragePolicy,
  Overview,
  PairingInput,
  PairingPreview,
  PairingSession,
  Pool,
  PoolCacheTtl,
  PoolKeepalive,
  PoolStrategy,
  PriceImportPreview,
  QuotaHistory,
  QuotaSnapshot,
  RequestLog,
  RequestPage,
  RequestQuery,
  ServedModel,
  Session,
  SessionInfo,
  Settings,
  SyncResult,
} from './types';

export type { AccountInput, AlertRuleInput, AnalyticsQuery } from './types';

export interface KeyInput {
  allowedModels: string[] | null;
  bodyCaptureHours: number | null;
  keepaliveOptOut: boolean;
  name: string;
  notes: string | null;
  poolId: string;
}

export interface CreateKeyInput extends KeyInput {
  expiresInDays: number | null;
}

/** Probe tuning a new pool starts with; the pool dialog shows these as its initial values. */
export const KEEPALIVE_TUNING_DEFAULTS: Omit<PoolKeepalive, 'costCapUsdPerDay' | 'enabled'> = {
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
export const CACHE_TTL_DEFAULTS: PoolCacheTtl = { enabled: false, inOverage: null, main: '1h', models: [], subagent: '5m' };

export interface PoolInput {
  affinityTtlSeconds: number;
  bodyCaptureHours: number;
  cacheTtl: PoolCacheTtl;
  defaultAllowedModels: string[] | null;
  keepalive: PoolKeepalive;
  members: { accountId: string; priority: number }[];
  migrateAboveUtilization: number | null;
  name: string;
  overagePolicy: OveragePolicy;
  status: 'active' | 'disabled';
  strategy: PoolStrategy;
}

export interface Api {
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
  deleteAlertRule(id: string): Promise<void>;
  deleteKey(id: string): Promise<void>;
  deletePool(id: string): Promise<void>;
  disconnectAccount(accountId: string): Promise<void>;
  getAnalyticsBreakdown(query: AnalyticsQuery): Promise<AnalyticsGroup[]>;
  getAnalyticsSeries(query: AnalyticsQuery): Promise<AnalyticsPoint[]>;
  getCapturedBodies(requestId: string): Promise<CapturedBodies | null>;
  getKeepaliveOverview(): Promise<KeepaliveOverview>;
  getOverview(): Promise<Overview>;
  getQuotaHistory(accountId: string): Promise<QuotaHistory[]>;
  getRequest(id: string): Promise<RequestLog | null>;
  /** Null when no session cookie is valid, which is what puts the login screen up. */
  getSession(): Promise<SessionInfo | null>;
  getSettings(): Promise<Settings>;
  listAccountRollups(from: string, to: string): Promise<AccountRollup[]>;
  listAccounts(): Promise<Account[]>;
  listAlertDeliveries(eventId: string): Promise<Delivery[]>;
  listAlertEvents(state?: 'open' | 'resolved'): Promise<AlertEvent[]>;
  listAlertRules(): Promise<AlertRule[]>;
  listCatalog(): Promise<CatalogModel[]>;
  listKeepaliveRecipes(): Promise<KeepaliveRecipe[]>;
  listKeys(): Promise<ApiKey[]>;
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
  syncCatalog(accountId: string): Promise<SyncResult>;
  testAlertRule(id: string): Promise<AlertDeliveryTest>;
  updateAlertRule(id: string, input: AlertRuleInput): Promise<AlertRule>;
  updateKey(id: string, input: KeyInput): Promise<ApiKey>;
  updatePool(id: string, input: PoolInput): Promise<Pool>;
  updateServedModels(accountId: string, models: ServedModel[]): Promise<Account>;
  verifyAccount(accountId: string): Promise<{ ok: boolean; status: number }>;
}
