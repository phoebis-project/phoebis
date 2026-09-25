// Response shapes for the router's admin API. The mock client in src/mock
// produces these; a real client would fetch them.

export const ACCOUNT_KIND_VALUES = ['api_key', 'oauth'] as const;
export type AccountKind = (typeof ACCOUNT_KIND_VALUES)[number];
export type AccountStatus = 'cooldown' | 'funding_required' | 'paused' | 'reauth_required' | 'ready' | 'unpaired';
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
  /** The newest refresh attempt is unresolved or lost its answer, and the store opens no other attempt until it resolves. */
  quarantined: boolean;
  refreshTokenExpiresAt: string | null;
}

export interface Subscription {
  billingType: string;
  organizationType: string;
  /** `max` or `pro` from the profile's account flags; null when the document reports neither. */
  plan: 'max' | 'pro' | null;
  rateLimitTier: string;
  /** Epoch milliseconds on the wire, decoded to the ISO string every other timestamp carries; null when the
   *  profile does not report it. */
  since: string | null;
  trialEndsAt: string | null;
}

export interface PoolMembership {
  poolId: string;
  priority: number;
}

export interface Account {
  cooldowns: Cooldown[];
  credential: Credential;
  email: string | null;
  extraUsage: ExtraUsage | null;
  /** Null when the provider reports no Fable window for this plan. */
  fableWindow: RateLimitWindow | null;
  id: string;
  kind: AccountKind;
  label: string;
  /** The router's own reading of whether requests bill as overage; `since` is null when a usage sample, not a response, reported it. */
  overage: { inUse: boolean; observedAt: string; reset: string | null; since: string | null } | null;
  plan: string | null;
  pools: PoolMembership[];
  requests: { last24h: number; last7d: number; lifetime: number };
  routingEnabled: boolean;
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
  createdAt: string;
  displayName: string;
  id: string;
  maxInputTokens: number | null;
  maxTokens: number | null;
}

export interface AccountCatalog {
  error: string | null;
  fetchedAt: string;
  models: CatalogModel[];
}

/** One entry of a model rule list: a pattern over the folded canonical id, negated when `not` is set. */
export interface ModelRule {
  not: boolean;
  pattern: string;
}

/** How a key's own list meets its pool's: `extend` appends the pool's entries after the key's own, `replace` leaves them out. */
export type ModelRulesMode = 'extend' | 'replace';

/** A live model-scoped cooldown on one of a pool's members, attached to the rule entry whose pattern covers it. */
export interface RuleCooldown {
  accountId: string;
  reason: string;
  scope: string;
  setAt: string;
  until: string;
  window: string;
}

/** One entry of a key's processed rule list, with the level it came from and the live cooldowns it covers. */
export interface ProcessedRule extends ModelRule {
  cooldowns: RuleCooldown[];
  level: 'key' | 'pool';
}

/** `GET /admin/keys/{id}/models`: the processed list a key is judged by, in the order it applies. */
export interface KeyModels {
  keyId: string;
  longContextTwins: boolean;
  modelRulesMode: ModelRulesMode;
  poolId: string;
  rules: ProcessedRule[];
}

/** One `model_rejected` or `model_permitted` ledger event, as `GET /admin/models/state` lists it. */
export interface ModelEvent {
  accountId: string;
  at: string;
  keyId: string | null;
  model: string;
  /** The accounts a permit's request failed over past; empty on a rejection. */
  rejectedBy: string[];
  requestId: string | null;
  sessionId: string | null;
  /** The upstream's own id for the attempt that was rejected or that permitted, `null` when it sent none. */
  upstreamRequestId: string | null;
  wireModel: string;
}

/** `GET /admin/models/state`: the live cooldowns grouped by model, and the newest rejections and permits. */
export interface ModelsState {
  cooldowns: { model: string; rows: RuleCooldown[] }[];
  permits: ModelEvent[];
  rejections: ModelEvent[];
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

export type PoolStrategy = 'pace' | 'priority' | 'round_robin';
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
  /** Subagent recipes are probed too; off warms the main conversation only. */
  warmSubagents: boolean;
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
  /** Null inherits the bootstrap option `compatibility.cache_diagnostics`; true or false overrides it for this pool. */
  cacheDiagnostics: boolean | null;
  /** Always answered; `enabled` off is how a pool that rewrites no breakpoint reads. */
  cacheTtl: PoolCacheTtl;
  cost24h: number;
  id: string;
  keepalive: PoolKeepalive;
  members: { accountId: string; eligible: MemberEligibility; priority: number }[];
  /** Null disables utilization-driven migration. */
  migrateAboveUtilization: number | null;
  modelRules: ModelRule[];
  name: string;
  overagePolicy: OveragePolicy;
  /** Order the selection set by account kind before priority; the operator's answer alone. */
  preferSubscription: boolean;
  requests24h: number;
  /** Null lets selection pick among every eligible member; otherwise it prefers those under this 5-hour utilization. */
  selectBelowUtilization: number | null;
  /** `active` and `disabled` are set by hand; the rest are observed. */
  status: 'active' | 'degraded' | 'disabled' | 'exhausted';
  strategy: PoolStrategy;
  traffic24h: TrafficBucket[];
}

export type KeyStatus = 'active' | 'disabled' | 'expired' | 'revoked';

export interface ApiKey {
  /** Hours of captured bodies for this key; null inherits the pool, 0 turns capture off here. */
  bodyCaptureHours: number | null;
  createdAt: string;
  /** The stored switch, which `status` folds expiry and revocation over; an edit sends it back as it stands. */
  enabled: boolean;
  expiresAt: string | null;
  id: string;
  /** Sessions on this key never receive cache-warming probes. */
  keepaliveOptOut: boolean;
  lastUsedAt: string | null;
  lifetime: { cost: number; requests: number };
  /** Whether `GET /v1/models` under this key lists an `id[1m]` twin beside every long-context model. */
  longContextTwins: boolean;
  modelRules: ModelRule[];
  modelRulesMode: ModelRulesMode;
  name: string;
  notes: string | null;
  poolId: string;
  prefix: string;
  /** When the secret the last rotation retired last authenticated; null until it does or once rotation clears it. */
  retiredSecretLastUsedAt: string | null;
  /** Live sessions whose newest request still presented the previous secret; 0 once the grace ends. */
  sessionsOnRetiredSecret: number;
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
  /** Null when the headers carried the window's reset and no utilization. */
  utilization: number | null;
}

/**
 * Why the prompt-cache prefix did not hit, as the cache diagnostics beta named it. The router stores whatever
 * name the beta sends, so a name added after the console's eight (`CACHE_MISS_REASONS`) is kept as sent.
 */
export type CacheMissReason = string;

/** What the answer said about itself; `servedModel` only when it differs from the resolved model. */
export interface AttemptResponse {
  clearedInputTokens: number | null;
  inferenceGeo: string | null;
  refusalCategory: string | null;
  refusalRecommendedModel: string | null;
  servedModel: string | null;
  speed: string | null;
  stopReason: string | null;
  stopSequence: string | null;
}

export interface Attempt {
  accountId: string;
  /** Null when the attempt drew no response body to read diagnostics from. */
  cacheMissReason: CacheMissReason | null;
  cooldownSet: { reason: string; until: string } | null;
  durationMs: number | null;
  index: number;
  outcome: AttemptOutcome;
  response: AttemptResponse;
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
  /** The share of `output` spent thinking; at most `output`. */
  thinking: number;
  /** Server-tool calls the answer made, not tokens. */
  webFetch: number;
  webSearch: number;
}

/** What the request asked for, read off its body; `null` where the body carried no such member. */
export interface RequestShape {
  contextEdits: string[];
  effort: string | null;
  maxTokens: number | null;
  messageCount: number;
  requestedServiceTier: string | null;
  requestedSpeed: string | null;
  systemBlocks: number;
  temperature: number | null;
  thinkingBudget: number | null;
  thinkingType: string | null;
  toolCount: number;
}

export interface RequestLog {
  accountId: string | null;
  agentId: string | null;
  /** `x-claude-code-agent-type` as the client sent it; `null` when it sent none. */
  agentType: string | null;
  at: string;
  attempts: Attempt[];
  /** The model the router resolved the request to, which a pin and its events are keyed by. */
  canonicalModel: string;
  capturedBodies: boolean;
  /** The class the router derived at admission: `claude-code/<entrypoint>` or `other`; `null` on a row from before the
   * router recorded it. */
  client: string | null;
  /** The kind a client-drift refusal answered with (`hash` or `version_suffix`); `null` for every other request. */
  clientDrift: string | null;
  compaction: string | null;
  contextCompacted: string | null;
  costUsd: number;
  durationMs: number | null;
  errorCode: string | null;
  id: string;
  /** The `created_at` of the key the request was read under, 0 for none; a key id created again is another holder. */
  keyGeneration: number;
  keyId: string | null;
  kind: RequestKind;
  model: string;
  /** How the request ended, once every attempt is in. */
  outcome: 'canceled' | 'exhausted' | 'failed' | 'rejected' | 'succeeded';
  poolId: string | null;
  /** The sum of the client's `x-claude-code-prev-tool-durations`, in milliseconds. */
  prevToolMs: number | null;
  requestTruncated: boolean;
  responseTruncated: boolean;
  sessionId: string | null;
  shape: RequestShape;
  status: number | null;
  transport: Transport;
  ttfbMs: number | null;
  /** First content token; later than `ttfbMs` on streams that open with metadata. */
  ttftMs: number | null;
  /** `cc_turn_origin` off the attribution line; first-party only. */
  turnOrigin: string | null;
  usage: Usage;
  /** The client that sent the request, as it named itself; `null` when it named nothing. */
  userAgent: string | null;
}

/** The bytes each side stored and the bytes it ran to, as the ledger counted them; `seen - stored` is what a cut dropped. */
export interface CapturedBodies {
  request: unknown;
  requestBytesSeen: number;
  requestBytesStored: number;
  requestId: string;
  requestTruncated: boolean;
  response: unknown;
  responseBytesSeen: number;
  responseBytesStored: number;
  responseTruncated: boolean;
}

/** Why the router moved a session's pin: the reason string of a `pin_migrated` event. */
export type PinMigrationReason = 'cooldown' | 'mark' | 'overage' | 'spent' | 'utilization';

/** Why the router released a session's pin without a move: the `reasons` of a `pin_expired` event, in the
 * order the selection found them. */
export type PinReleaseReason = 'attempted' | 'cooldown' | 'deleted' | 'disconnected' | 'expired' | 'funding_required' | 'overage' | 'paused' | 'reauth_required' | 'released' | 'spent' | 'unpaired' | 'unserved';

/** One event of a request's window, as `GET /admin/requests/{id}` lists them beside the summary. */
export interface RequestEvent {
  accountId: string | null;
  at: string;
  detail: Record<string, unknown> | null;
  /** The pin's identity, where the event has one: the route lists a session's events by time alone. */
  keyId: string | null;
  kind: string;
  model: string | null;
  sessionId: string | null;
}

/** `GET /admin/requests/{id}`: the row and the events of its window, from one read. */
export interface RequestDetail {
  events: RequestEvent[];
  request: RequestLog;
}

export interface RequestQuery {
  accountIds?: string[];
  agentTypes?: string[];
  /** A pattern over the client class, the grammar of `userAgent`. */
  client?: string;
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
  turnOrigins?: string[];
  /** A pattern: `*` any run, `?` one character, a backslash quotes either. An empty string is no filter at all. */
  userAgent?: string;
}

export interface RequestPage {
  nextCursor: string | null;
  requests: RequestLog[];
}

export type AnalyticsGroupBy = 'account' | 'all' | 'client' | 'effort' | 'key' | 'kind' | 'model' | 'pool' | 'user_agent';

export interface AnalyticsQuery {
  accountIds?: string[];
  /** Patterns, one per line; a row matches when any matches its client class in full. */
  clients?: string[];
  from: string;
  groupBy: AnalyticsGroupBy;
  includeProbes?: boolean;
  keyIds?: string[];
  kinds?: RequestKind[];
  models?: string[];
  poolIds?: string[];
  to: string;
  /** Patterns, one per line, each naming a group under `user_agent`. */
  userAgentGroups?: string[];
  /** Patterns, one per line; a row matches when any matches its user agent in full. */
  userAgents?: string[];
}

/** One group of one bucket or of the whole range, as the analytics routes sum it. */
export interface AnalyticsGroup {
  attempts: number;
  cacheHits: number;
  cacheMisses: number;
  /** The eight known reasons always, beside any other name the route answered. */
  cacheMissReasons: Record<CacheMissReason, number>;
  /** The cache read and write costs are null on a group folded from request rows, which carry one total. */
  cost: Omit<CostSplit, 'cacheRead' | 'cacheWrite'> & { cacheRead: number | null; cacheWrite: number | null };
  costUsd: number;
  errors: Record<string, number>;
  exhausted: number;
  expiredBetweenTurns: number;
  failed: number;
  failovers: number;
  /**
   * Set on a group folded from request rows, the session view's source: the rows carry no cost split, no
   * savings and no kept-warm mark, so those figures are null and the probe counts are not measured.
   */
  fromRequests?: true;
  histograms: LatencyHistograms;
  keptWarm: { requests: number; tokens: number } | null;
  key: string;
  label: string;
  percentiles: Record<keyof LatencyHistograms, { p50: number | null; p90: number | null; p95: number | null; p99: number | null }>;
  probes: { failed: number; migrated: number; ok: number; skipped: number };
  rateLimited: number;
  rejected: number;
  requests: number;
  savings: { grossUsd: number; probeCostUsd: number; wastedProbeReasons: Record<WastedProbeReason, number>; wastedProbeUsd: number; wastedProbes: number } | null;
  succeeded: number;
  /** The routes answer every key but `plainOutput`, which a view derives where it splits the output into its
   * thinking share and the rest. */
  tokens: { cacheRead: number; cacheWrite1h: number; cacheWrite5m: number; output: number; plainOutput?: number; thinking: number; uncachedInput: number; webFetch: number; webSearch: number };
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
  /** Null for a window the hour holds no utilization sample of. */
  peak: { '5h': number | null; '7d': number | null; '7d_fable': number | null };
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
  /** A generation no live key holds: the ledger's memory of an earlier holder of this key id. Read-only, no recipe, no release. */
  historical: boolean;
  /** `state` is null when the session carries no cache-warming recipe at all. */
  keepalive: { nextProbeAt: string | null; state: KeepaliveState | null };
  /** The `created_at` of the key that owns the session. */
  keyGeneration: number;
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
  | 'pool_exhausted'
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
  | 'session_active'
  | 'session_idle'
  | 'session_parked'
  | 'subagent_active'
  | 'utilization_above';

/** What an event captured; null for the kinds that need no extra fields, and for the session kinds, which open none. */
export type AlertEventDetail = Record<string, unknown> | null;

/**
 * The details a page draws a sentence from. A detail is the free-form object the signal built, passed through
 * undecoded, so each carries the wire's own spelling rather than the camelCase every decoded type uses.
 */
export interface AskUserQuestionDetail {
  agent: string | null;
  key: string;
  question: string | null;
  /**
   * Unix milliseconds when the captured body the text was copied from expires, and the text with it; absent when
   * the body was never captured. The route blanks the question for every role from this instant on.
   */
  question_expires_at?: number | null;
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

export interface RefreshFailedDetail {
  account: string;
  failures: number;
  last_error: string | null;
  /** Whether the refusal was the upstream's answer or a lost one, which decides whether the token is really gone. */
  outcome: 'failed' | 'uncertain';
}

/** What a session_active, session_idle, session_parked, or subagent_active delivery carried about its session. */
export interface SessionLifecycleDetail {
  agent?: string;
  event?: LifecycleEventName;
  idle_minutes?: number;
  key: string;
  last_request_at?: number;
  parent_agent?: string | null;
  pool: string | null;
  session: string;
  source?: 'companion' | 'expiry' | 'request';
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
  /** A fraction for utilization_above and whole minutes for session_idle; every other kind takes none. */
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

/** One session event as it was posted: the log the Alerts view lists, since a lifecycle rule opens no event. */
export interface LifecycleDelivery {
  at: string;
  attempt: number;
  detail: SessionLifecycleDetail;
  error: string | null;
  event: LifecycleEventName;
  id: number;
  kind: AlertKind;
  ruleId: string;
  status: number | null;
  subject: string;
}

export type LifecycleEventName = 'session_ended' | 'session_idle' | 'session_parked' | 'session_resumed' | 'session_started' | 'subagent_ended' | 'subagent_started';

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
  /** What the bootstrap file's compatibility block sets, which the route reads and never writes. */
  compatibility: { cacheDiagnostics: boolean; outboundGzip: boolean; profile: string; usageEntrypoint: string };
  keepalive: { leaseSeconds: number; maxAgents: number; maxRequestBytes: number; maxRetainedBytes: number; maxSessions: number; maxWorkers: number; oauthPrewarmVerified: boolean };
  listeners: { admin: string; metrics: string; proxy: string };
  /** The price table shown to the user and used to cost the ledger. */
  pricing: { effectiveDate: string; models: ModelPrice[]; source: string };
  quota: { pollIntervalSeconds: number; sampleRetentionDays: number };
  retention: { bodyCaptureMaxHours: number; ledgerDays: number; quotaSamplesDays: number; rollupDays: number };
  routing: { catalogCacheSeconds: number; catalogRefreshSeconds: number; cooldownFallbackSeconds: number; maxAttempts: number; modelRejectionSeconds: number; startupFetchSeconds: number };
  /** How long a session stays listed after its last activity. */
  sessions: { expireAfterSeconds: number };
  /** `pricingPath` is null when no override file is configured. */
  storage: { ledgerPath: string; path: string; pricingPath: string | null };
  usage: { cacheTtlSeconds: number; refreshConcurrency: number };
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

export interface Overview {
  /** The router's live pins, which the session rows overcount: those also hold ledger-only and historical sessions. */
  activePins: number;
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
  /** Null only on a historical projection, which cannot say how much of the final usage existed at its instant. */
  usage: Usage | null;
  /** The client that sent the request, as it named itself; `null` when it named nothing. */
  userAgent: string | null;
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
  | { at: string; cursor: number; id: string; type: 'finished'; attempts: LiveAttempt[]; costUsd: number; errorCode: string | null; phases: LivePhases; status: number | null; usage: Usage }
  | { cursor: number; request: LiveRequest; type: 'started' }
  | { cursor: number; tick: LiveTick; type: 'tick' };

export type LiveEvent = { kind: 'delta'; delta: LiveDelta } | { kind: 'snapshot'; snapshot: LiveSnapshot };
