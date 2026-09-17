import type {
  Account,
  AccountRollup,
  AlertEvent,
  AlertRule,
  ApiKey,
  CapturedBodies,
  CatalogModel,
  CreatedApiKey,
  HourlyRollup,
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
  RequestQuery,
  ServedModel,
  Session,
  Settings,
} from './types';

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
  completePairing(input: PairingInput): Promise<PairingPreview>;
  confirmPairing(state: string): Promise<void>;
  createKey(input: CreateKeyInput): Promise<CreatedApiKey>;
  createPool(input: PoolInput): Promise<Pool>;
  getCapturedBodies(requestId: string): Promise<CapturedBodies | null>;
  getKeepaliveOverview(): Promise<KeepaliveOverview>;
  getOverview(): Promise<Overview>;
  getQuotaHistory(accountId: string): Promise<QuotaHistory[]>;
  getRequest(id: string): Promise<RequestLog | null>;
  getSettings(): Promise<Settings>;
  listAccountRollups(from: string, to: string): Promise<AccountRollup[]>;
  listAccounts(): Promise<Account[]>;
  listAlertEvents(): Promise<AlertEvent[]>;
  listAlertRules(): Promise<AlertRule[]>;
  listCatalog(): Promise<CatalogModel[]>;
  listKeepaliveRecipes(): Promise<KeepaliveRecipe[]>;
  listKeys(): Promise<ApiKey[]>;
  listPools(): Promise<Pool[]>;
  listQuotaSnapshots(): Promise<QuotaSnapshot[]>;
  listRequests(query?: RequestQuery): Promise<RequestLog[]>;
  listRollups(from: string, to: string): Promise<HourlyRollup[]>;
  listSessions(): Promise<Session[]>;
  /** The rows a catalog at this URL yields, already converted to USD per million and reduced to models the router serves, with the counts behind that filter. */
  previewPriceImport(url: string): Promise<PriceImportPreview>;
  /** Idle time after which the router forgets a session; the sessions list honours it. */
  setSessionExpiry(seconds: number): Promise<Settings>;
  /** Pass null to pair a new account, or an id to re-pair an existing one. */
  startPairing(accountId: string | null): Promise<PairingSession>;
  /** Delivers a snapshot first, then deltas; returns the unsubscribe function. */
  subscribeLive(handler: (event: LiveEvent) => void): () => void;
  updateKey(id: string, input: KeyInput): Promise<ApiKey>;
  updatePool(id: string, input: PoolInput): Promise<Pool>;
  updateServedModels(accountId: string, models: ServedModel[]): Promise<Account>;
}
