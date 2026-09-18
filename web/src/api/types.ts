// Response shapes for the router's admin API. The mock client in src/mock
// produces these; a real client would fetch them.

export type AccountKind = 'api_key' | 'oauth';
export type AccountStatus = 'cooldown' | 'disabled' | 'funding_required' | 'paused' | 'reauth_required' | 'ready' | 'unpaired';
export type WindowId = '5h' | '7d' | '7d_fable';

export interface RateLimitWindow {
  id: WindowId;
  label: string;
  periodHours: number;
  /** Observed as of `observedAt`; may be stale for accounts that cannot refresh. */
  observedAt: string;
  /** Null while the sampler has seen a utilization but no reset time for this window. */
  resetsAt: string | null;
  utilization: number;
}

export interface ExtraUsage {
  /** Set when the provider stopped funding overage, e.g. a failed card. */
  disabledReason: string | null;
  enabled: boolean;
  inUse: boolean;
  monthlyLimitCredits: number;
  usedCredits: number;
}

export interface ServedModel {
  /** Id clients send; equals `upstreamId` unless the upstream aliases it. */
  canonicalId: string;
  id: string;
  inCatalog: boolean;
  /** Discovered by a catalog sync after the account was added. */
  isNew: boolean;
  longContext: boolean;
  /** Added by hand rather than discovered from the upstream catalog. */
  manual: boolean;
  selected: boolean;
  upstreamId: string;
}

export interface Cooldown {
  reason: string;
  /** What the cooldown covers, as the router spells it: `account`, or `model:<id>`. Clearing one names this. */
  scope: string;
  setAt: string;
  until: string;
  windowId: WindowId | null;
}

export interface Credential {
  accessTokenExpiresAt: string | null;
  lastError: string | null;
  lastRefreshAt: string | null;
  refreshTokenExpiresAt: string | null;
}

export interface Subscription {
  billingType: string;
  organizationType: string;
  rateLimitTier: string;
  /** Null unless the wire carried it; the identity card leaves a null field out rather than guessing one. */
  since: string | null;
  status: 'active' | 'past_due' | 'trialing' | null;
  trialEndsAt: string | null;
}

export interface PoolMembership {
  poolId: string;
  priority: number;
}

export interface Account {
  catalogSyncedAt: string | null;
  cooldowns: Cooldown[];
  credential: Credential;
  email: string | null;
  extraUsage: ExtraUsage | null;
  /** Null when the provider reports no Fable window for this plan. */
  fableWindow: RateLimitWindow | null;
  id: string;
  kind: AccountKind;
  label: string;
  plan: string | null;
  pools: PoolMembership[];
  requests: { last24h: number; last7d: number; lifetime: number };
  routingEnabled: boolean;
  servedModels: ServedModel[];
  status: AccountStatus;
  subscription: Subscription | null;
  /** The 5-hour and 7-day windows; the Fable window is `fableWindow`. */
  windows: RateLimitWindow[];
}

/** What the operator pasted: a whole callback URL, which carries its own state, or the bare code with the state the pairing started under. */
export type PairingInput = { code: string; state: string } | { url: string };

export interface PairingSession {
  authorizeUrl: string;
  expiresAt: string;
  id: string;
  /** Loopback address the router registered for this pairing; nothing listens on it, so the browser only ever shows it. */
  redirectUri: string;
  state: string;
}

export interface PairingPreview {
  email: string;
  organization: string;
  state: string;
}

export interface CatalogModel {
  id: string;
  longContext: boolean;
}

export interface QuotaSample {
  at: string;
  utilization: number;
}

export interface QuotaHistory {
  accountId: string;
  samples: QuotaSample[];
  windowId: WindowId;
}

export type QuotaFetchStatus = 'error' | 'loading' | 'success';

export interface QuotaSnapshot {
  accountId: string;
  error: string | null;
  fetchedAt: string | null;
  status: QuotaFetchStatus;
}

export type PoolStrategy = 'earliest_refresh' | 'least_utilized' | 'priority_random';
export type OveragePolicy = 'allow' | 'never' | 'prefer_plan';

export interface PoolKeepalive {
  /** Retries allowed for a session whose cache already went cold. */
  coldProbeLimit: number;
  /** Probes stop for the day once this is spent; null means uncapped. */
  costCapUsdPerDay: number | null;
  enabled: boolean;
  jitterSeconds1h: number;
  jitterSeconds5m: number;
  /** Seconds before a 1-hour cache entry expires at which its probe fires. */
  leadSeconds1h: number;
  /** Seconds before a 5-minute cache entry expires at which its probe fires. */
  leadSeconds5m: number;
  /** 0 means a session keeps being probed for as long as its pin lives. */
  maxIdleSeconds: number;
  /** 0 means no limit on consecutive probes without real traffic between them. */
  maxIdleTicks: number;
  /** Probes a recipe still gets once its session's net savings have gone below zero. */
  netNegativeGraceProbes: number;
  retryMaxSeconds: number;
  /** First wait after a failed probe; each further failure doubles it. */
  retrySeconds: number;
  /** Pause a recipe whose probes have cost more than the cache reads they bought. */
  stopWhenNetNegative: boolean;
  /** Subagent recipes are probed too; off warms the main conversation only. Null where the router has no such rule. */
  warmSubagents: boolean | null;
}

export type CacheTtl = '5m' | '1h';

export interface PoolCacheTtl {
  /** Off leaves every breakpoint as the client sent it. */
  enabled: boolean;
  /** What applies while the serving upstream is in overage; null keeps the rules below. */
  inOverage: CacheTtl | null;
  main: CacheTtl;
  /** Per-model overrides; a model absent here takes the pool rule. */
  models: { main: CacheTtl; model: string; subagent: CacheTtl }[];
  subagent: CacheTtl;
}

export interface TrafficBucket {
  at: string;
  costUsd: number;
  failed: number;
  requests: number;
  tokens: number;
}

/** Why routing would refuse a pool member, or `eligible` when it would not. */
export type MemberEligibility = 'cooldown' | 'eligible' | 'overage' | 'unavailable';

export interface Pool {
  activePins: number;
  /** Idle seconds before a session's pin to an account expires; 0 disables affinity. */
  affinityTtlSeconds: number;
  /** Hours of captured request and response bodies for keys with no override; 0 is off. */
  bodyCaptureHours: number;
  /** Null where the router serves no cache-TTL rules for this pool. */
  cacheTtl: PoolCacheTtl | null;
  cost24h: number;
  defaultAllowedModels: string[] | null;
  id: string;
  keepalive: PoolKeepalive;
  members: { accountId: string; eligible: MemberEligibility; priority: number }[];
  /** Null disables utilization-driven migration. */
  migrateAboveUtilization: number | null;
  name: string;
  overagePolicy: OveragePolicy;
  requests24h: number;
  /** `active` and `disabled` are set by hand; the rest are observed. */
  status: 'active' | 'degraded' | 'disabled' | 'exhausted';
  strategy: PoolStrategy;
  traffic24h: TrafficBucket[];
}

export type KeyStatus = 'active' | 'expired' | 'revoked';

export interface ApiKey {
  allowedModels: string[] | null;
  /** Hours of captured bodies for this key; null inherits the pool, 0 turns capture off here. */
  bodyCaptureHours: number | null;
  createdAt: string;
  expiresAt: string | null;
  id: string;
  /** Sessions on this key never receive cache-warming probes. */
  keepaliveOptOut: boolean;
  lastUsedAt: string | null;
  lifetime: { cost: number; requests: number };
  name: string;
  notes: string | null;
  poolId: string;
  prefix: string;
  status: KeyStatus;
  usage7d: { cost: number; requests: number; tokens: number };
}

export interface CreatedApiKey {
  key: ApiKey;
  /** Shown once at creation and never stored in plain text. */
  secret: string;
}

export type RequestKind = 'count_tokens' | 'inference' | 'probe';
export type Transport = 'json' | 'sse';
export type AttemptOutcome = 'auth_failed' | 'canceled' | 'rate_limited' | 'succeeded' | 'transport_error' | 'upstream_error';

export interface AttemptWindow {
  id: WindowId;
  resetsAt: string | null;
  utilization: number;
}

/**
 * Why the prompt-cache prefix did not hit, from the cache diagnostics beta.
 * Ordered as the beta documents it rather than alphabetically: the no-divergence
 * answer first, then the diagnosed divergences, then the two non-answers.
 */
export type CacheMissReason = 'none' | 'model_changed' | 'system_changed' | 'tools_changed' | 'messages_changed' | 'previous_message_not_found' | 'unavailable' | 'not_requested';

export interface Attempt {
  accountId: string;
  /** Estimated input that fell after the divergence point; set only for the `*_changed` reasons. */
  cacheMissedInputTokens: number | null;
  /** Null when the attempt drew no response body to read diagnostics from. */
  cacheMissReason: CacheMissReason | null;
  cooldownSet: { reason: string; until: string } | null;
  durationMs: number | null;
  index: number;
  outcome: AttemptOutcome;
  startedAt: string;
  upstreamStatus: number | null;
  windows: AttemptWindow[];
}

export interface Usage {
  cacheRead: number;
  cacheWrite1h: number;
  cacheWrite5m: number;
  input: number;
  output: number;
}

export interface RequestLog {
  accountId: string | null;
  agentId: string | null;
  at: string;
  attempts: Attempt[];
  capturedBodies: boolean;
  costUsd: number;
  durationMs: number | null;
  errorCode: string | null;
  id: string;
  keyId: string | null;
  kind: RequestKind;
  model: string;
  /** How the request ended, once every attempt is in. */
  outcome: 'canceled' | 'exhausted' | 'failed' | 'rejected' | 'succeeded';
  poolId: string | null;
  sessionId: string | null;
  status: number | null;
  transport: Transport;
  ttfbMs: number | null;
  /** First content token; later than `ttfbMs` on streams that open with metadata. */
  ttftMs: number | null;
  usage: Usage;
}

export interface CapturedBodies {
  request: unknown;
  requestId: string;
  response: unknown;
}

export interface RequestQuery {
  accountIds?: string[];
  cursor?: string;
  from?: string;
  includeProbes?: boolean;
  keyIds?: string[];
  kind?: RequestKind | null;
  limit?: number;
  models?: string[];
  poolIds?: string[];
  search?: string;
  statuses?: number[];
  to?: string;
}

export interface RequestPage {
  nextCursor: string | null;
  requests: RequestLog[];
}

export type AnalyticsGroupBy = 'account' | 'all' | 'key' | 'kind' | 'model' | 'pool' | 'user_agent';

export interface AnalyticsQuery {
  accountIds?: string[];
  from: string;
  groupBy: AnalyticsGroupBy;
  includeProbes?: boolean;
  keyIds?: string[];
  kinds?: RequestKind[];
  models?: string[];
  poolIds?: string[];
  to: string;
}

/** One group of one bucket or of the whole range, as the analytics routes sum it. */
export interface AnalyticsGroup {
  attempts: number;
  cacheHits: number;
  cacheMisses: number;
  cacheMissReasons: Record<CacheMissReason, number>;
  cost: CostSplit;
  costUsd: number;
  errors: Record<string, number>;
  exhausted: number;
  expiredBetweenTurns: number;
  failed: number;
  failovers: number;
  histograms: LatencyHistograms;
  keptWarm: { requests: number; tokens: number };
  key: string;
  label: string;
  percentiles: Record<keyof LatencyHistograms, { p50: number | null; p90: number | null; p95: number | null; p99: number | null }>;
  probes: { failed: number; migrated: number; ok: number; skipped: number };
  rateLimited: number;
  rejected: number;
  requests: number;
  savings: { grossUsd: number; probeCostUsd: number; wastedProbeReasons: Record<WastedProbeReason, number>; wastedProbeUsd: number; wastedProbes: number };
  succeeded: number;
  tokens: { cacheRead: number; cacheWrite1h: number; cacheWrite5m: number; output: number; uncachedInput: number };
  uncached: number;
}

export interface AnalyticsPoint {
  at: string;
  groups: AnalyticsGroup[];
}

export interface CostSplit {
  cacheRead: number;
  cacheWrite: number;
  input: number;
  output: number;
}

/** Counts per bucket; the 64 log-spaced edges are defined in src/lib/histogram.ts. */
export interface LatencyHistograms {
  durationMs: number[];
  outputTps: number[];
  ttfbMs: number[];
  ttftMs: number[];
}

/** Why a recipe stopped while probes it had already paid for were still in the air. */
export type WastedProbeReason = 'idle_limit' | 'released' | 'replaced' | 'session_ended';

export interface AccountRollup {
  accountId: string;
  cooldownSeconds: number;
  hour: string;
  overageSeconds: number;
  peak: { '5h': number; '7d': number; '7d_fable': number | null };
  pins: { created: number; migratedIn: number; migratedOut: number };
}

export type KeepaliveState = 'busy' | 'idle' | 'parked';

/** Whether a cache-keepalive companion is registered for the session and still reporting. */
export type CompanionState = 'none' | 'reporting' | 'silent';

export interface Session {
  accountId: string | null;
  agents: number;
  companion: CompanionState;
  /** When the companion last reported; set only while `companion` is `silent`. */
  companionSince: string | null;
  createdAt: string | null;
  epoch: number | null;
  /** `state` is null when the session carries no cache-warming recipe at all. */
  keepalive: { nextProbeAt: string | null; state: KeepaliveState | null };
  keyId: string;
  /** Last ledgered client request; null for a session that has not been seen in the ledger. */
  lastRequestAt: string | null;
  lastUsedAt: string | null;
  model: string | null;
  /** Whether the session is pinned to its account for as long as affinity holds. */
  pinned: boolean;
  /** Ledgered requests on this session; 0 means analytics and observe have nothing to show. */
  requests: number;
  sessionId: string;
}

export type RecipeState = 'armed' | 'held' | 'probing';
export type RecipeReason =
  | 'affinity_too_short'
  | 'awaiting_cache_evidence'
  | 'cap_reached'
  | 'cold_probes'
  | 'cooled'
  | 'idle_limit'
  | 'net_negative'
  | 'oauth_unverified'
  | 'overage'
  | 'paused'
  | 'probe_error'
  | 'quota_exhausted'
  | 'reauth'
  | 'retrying'
  | 'storage_limit'
  | 'unsupported_shape';

/** One cache-warming recipe per agent: what the next probe replays and why it may be held. */
export interface KeepaliveRecipe {
  accountId: string | null;
  agentId: string;
  keyId: string;
  /** This agent's last real request; null for an agent that has not sent one yet. */
  lastRequestAt: string | null;
  model: string;
  nextProbeAt: string | null;
  poolId: string;
  reason: RecipeReason | null;
  sessionId: string;
  state: RecipeState;
  ttlSeconds: number;
}

export interface KeepaliveOverview {
  agents: Record<KeepaliveState, number>;
  costToday: { capUsd: number | null; poolId: string; usd: number }[];
  /** Requests whose prefix expired between turns in the last 24 hours. */
  expiredBetweenTurns24h: number;
  netSaved7dUsd: number;
  probes24h: { failed: number; migrated: number; ok: number; skipped: number };
  sessions: number;
  sessionsWithCompanion: number;
}

export type AlertKind =
  | 'ask_user_question'
  | 'client_drift'
  | 'funding_required'
  | 'keepalive_cost_cap'
  | 'pool_exhausted'
  | 'reauth_required'
  | 'refresh_failed'
  | 'utilization_above';

/** What an event of one of the session-scoped kinds captured; null for the kinds that need no extra fields. */
export type AlertEventDetail = Record<string, unknown> | null;

/**
 * The two details a page draws a sentence from. A detail is the free-form object the signal built, passed through
 * undecoded, so both carry the wire's own spelling rather than the camelCase every decoded type uses.
 */
export interface AskUserQuestionDetail {
  agent: string | null;
  key: string;
  question: string | null;
  /** True with a null question when a viewer reads the event: the text sits behind the admin secret. */
  question_redacted?: boolean;
  session: string;
}

export interface ClientDriftDetail {
  client_version: string;
  key: string | null;
  /** Free text: the relay names the check that caught the drift, and a later build may name another one. */
  trigger: string;
}

export interface AlertRule {
  createdAt: string;
  enabled: boolean;
  hasSecret: boolean;
  id: string;
  kind: AlertKind;
  scope: { id: string | null; type: 'account' | 'all' | 'any_account' | 'key' | 'pool' };
  threshold: number | null;
  updatedAt: string;
  webhookUrl: string;
  /** The window the router names on the wire, which is not the UI's own spelling of the third one. */
  windowId: '5h' | '7d' | '7d_oi' | null;
}

export interface AlertRuleInput {
  enabled: boolean;
  kind: AlertKind;
  scope: AlertRule['scope'];
  /** Absent leaves the stored secret alone; an empty string clears it. */
  secret?: string;
  threshold: number | null;
  webhookUrl: string;
  windowId: AlertRule['windowId'];
}

export interface AlertDeliveryTest {
  error: string | null;
  status: number | null;
}

export interface Delivery {
  at: string;
  attempt: number;
  error: string | null;
  state: 'opened' | 'resolved';
  status: number | null;
}

export interface AlertEvent {
  deliveries: Delivery[];
  detail: AlertEventDetail;
  id: string;
  kind: AlertKind;
  openedAt: string;
  resolvedAt: string | null;
  ruleId: string;
  state: 'open' | 'resolved';
  subject: string;
}

export interface ModelPrice {
  cacheReadPerM: number;
  cacheWrite1hPerM: number;
  cacheWrite5mPerM: number;
  inputPerM: number;
  model: string;
  outputPerM: number;
}

export interface PriceImportPreview {
  /** One change per field the diff would write, against the table the router serves. */
  changes: number;
  /** Models the file carries, before the ones the router does not serve are dropped. */
  inFile: number;
  /** The rows that survived the filter, which is what the diff is built from. */
  models: ModelPrice[];
  /** How many of the models in the file the router serves. */
  served: number;
  /** Identifies this preview to the apply call, so what is applied is what was shown. */
  token: string;
}

export interface Settings {
  about: { profile: string; publicUrl: string | null; version: string };
  access: { adminConfigured: boolean; viewerConfigured: boolean };
  keepalive: { leaseSeconds: number; maxAgents: number; maxRequestBytes: number; maxRetainedBytes: number; maxSessions: number; maxWorkers: number; oauthPrewarmVerified: boolean };
  /** The newest catalog sync across the upstreams, or null before the first one. */
  lastSyncAt: string | null;
  listeners: { admin: string; metrics: string; proxy: string };
  /** The price table shown to the user and used to cost the ledger. */
  pricing: { effectiveDate: string; models: ModelPrice[]; source: string };
  retention: { bodyCaptureMaxHours: number; ledgerDays: number; quotaSamplesDays: number };
  /** How long a session stays listed after its last activity. */
  sessions: { expireAfterSeconds: number };
}

export type Role = 'admin' | 'viewer';

export interface SessionInfo {
  role: Role;
  sessionId: string;
}

export interface AccountInput {
  id: string;
  kind: AccountKind;
  label: string;
  secret: string | null;
}

export interface SyncResult {
  added: string[];
  error: string | null;
  missing: string[];
  seeded: boolean;
}

export interface Overview {
  cost24h: number;
  errorRate24h: number | null;
  hourly: { at: string; cost: number; errors: number; requests: number; tokens: number }[];
  requests24h: number;
  tokens24h: number;
  tokensCached24h: number;
  topErrorCode: string | null;
}

/* Live traffic feed */

export type LiveState = 'finished' | 'in_flight';
export type MetricQuality = 'derived' | 'estimated' | 'measured';

/** Timestamps land as each phase is observed; an absent field means "not yet", never zero. */
export interface LivePhases {
  finishedAt?: string;
  firstContentAt?: string;
  headAt?: string;
  receivedAt: string;
  selectedAt?: string;
  streamEndAt?: string;
  writtenAt?: string;
}

export interface LiveAttempt {
  accountId: string;
  cooldownSet: { reason: string; until: string } | null;
  index: number;
  outcome: AttemptOutcome | 'in_flight';
  startedAt: string;
  upstreamStatus: number | null;
  windows: AttemptWindow[];
}

export interface LiveRequest {
  accountId: string | null;
  agentId: string;
  attempts: LiveAttempt[];
  costUsd: number;
  errorCode: string | null;
  id: string;
  keyId: string | null;
  kind: RequestKind;
  model: string;
  outputTps: number | null;
  phases: LivePhases;
  poolId: string | null;
  /** Output text so far; only keys that capture bodies carry it. */
  preview: string | null;
  sessionId: string | null;
  state: LiveState;
  status: number | null;
  transport: Transport;
  usage: Usage;
}

export interface LiveMetric {
  quality: MetricQuality;
  /** One value per second for the last minute, oldest first; null for a second with nothing to measure. */
  series: (number | null)[];
  value: number | null;
}

export interface LiveTick {
  at: string;
  metrics: {
    activeStreams: LiveMetric;
    costPerMinute: LiveMetric;
    errorRate: LiveMetric;
    outputTps: LiveMetric;
    requestsPerSecond: LiveMetric;
    ttfbP50: LiveMetric;
    ttfbP95: LiveMetric;
    ttfbP99: LiveMetric;
  };
}

export interface LiveSnapshot {
  cursor: number;
  inFlight: LiveRequest[];
  /** Finished requests from the last 30 minutes, newest first. */
  recent: LiveRequest[];
  tick: LiveTick;
}

export type LiveDelta =
  | { at: string; cursor: number; id: string; type: 'selected'; accountId: string; attempt: LiveAttempt; failed?: LiveAttempt }
  | { at: string; cursor: number; id: string; type: 'head'; attempt: LiveAttempt; status: number }
  | { at: string; cursor: number; id: string; type: 'progress'; outputTps: number | null; usage: Usage }
  | { cursor: number; id: string; type: 'preview'; chunk: string }
  | { at: string; cursor: number; id: string; type: 'finished'; costUsd: number; errorCode: string | null; phases: LivePhases; status: number | null; usage: Usage }
  | { cursor: number; request: LiveRequest; type: 'started' }
  | { cursor: number; tick: LiveTick; type: 'tick' };

export type LiveEvent = { kind: 'delta'; delta: LiveDelta } | { kind: 'snapshot'; snapshot: LiveSnapshot };
