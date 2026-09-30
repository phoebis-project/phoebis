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
  /** The account form's prefill: the anchor derived from `since` and the trial, carried forward to the next
   *  occurrence after now; null with no `since` to derive from. */
  derivedEndsAt: string | null;
  /** The operator's saved end, standing only while `Account.cancelled` is set; null otherwise. */
  endsAt: string | null;
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
  /** Set only on an OAuth account; an api_key account always reads false. */
  cancelled: boolean;
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

/** How the usage endpoint answered an upstream's last attempt. */
export type PollOutcome = 'clean' | 'failed' | 'rate_limited';

/** The poll's standing for an OAuth upstream, kept by the router; null until the poll first asks. */
export interface QuotaPoll {
  /** The first 429 of the run the upstream is in; null outside one. */
  backoffSince: string | null;
  lastAttemptAt: string | null;
  /** When the windows of the last clean answer were read; null until one lands. */
  lastCleanAt: string | null;
  lastOutcome: PollOutcome;
  /** When the upstream may be asked again; null when its next slot may ask it. */
  nextAttemptAt: string | null;
  /** 429s answered in a row; 0 after a clean answer. */
  rateLimitedInARow: number;
}

export interface QuotaSnapshot {
  accountId: string;
  error: string | null;
  fetchedAt: string | null;
  poll: QuotaPoll | null;
  status: QuotaFetchStatus;
}

export type PoolStrategy = 'pace' | 'priority' | 'round_robin';
export type OveragePolicy = 'allow' | 'never' | 'prefer_plan';

export interface PoolKeepalive {
  /** Probes stop for the day once a subscription account's probes have consumed this share of its 5-hour
   * window; null means uncapped. */
  capWindowSharePerDay: number | null;
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

/** One rule set's shape: the TTL by branch, with per-model overrides on `main` and `subagent`. */
export interface TtlRules {
  keepalive: CacheTtl;
  main: CacheTtl;
  /** Per-model overrides; a model absent here takes `main` and `subagent`. */
  models: { main: CacheTtl; model: string; subagent: CacheTtl }[];
  subagent: CacheTtl;
}

export interface PoolCacheTtl {
  /** Off leaves every breakpoint as the client sent it. */
  enabled: boolean;
  /** Whether the rewrite adds a top-level breakpoint to a body that carries none. */
  injectWhenAbsent: boolean;
  /** The rules while a write costs money: an api_key account, or a subscription account in overage. */
  metered: TtlRules;
  plan: TtlRules;
}

export interface TrafficBucket {
  at: string;
  costUsd: number;
  failed: number;
  requests: number;
  tokens: number;
}

/** Why routing would refuse a pool member, or `eligible` when it would not. */
export type MemberEligibility = 'budget' | 'cooldown' | 'eligible' | 'overage' | 'unavailable';

/** Whether a session's pin is made for every request, or only for one whose body carries a cache marker. */
export type AffinityMode = 'always' | 'when_cached';
/** What a key identifies a conversation by: the climb's first source that answers, or one source alone. */
export type AffinitySource = 'auto' | 'content' | 'session' | 'user_id' | 'whole_key';
export type BudgetPeriod = 'day' | 'month';
/** Whether the upstream's quota headers reach the client on a committed answer. */
export type QuotaHeaders = 'hide' | 'pass';
/** Where a fresh session with no pin lands: by the strategy, or where its static prefix is warm. */
export type SessionPlacement = 'by_strategy' | 'by_warm_prefix';
/** Where a subagent with no pin of its own lands. */
export type SubagentPlacement = 'by_cohort' | 'by_strategy' | 'with_main';

export interface PoolMember {
  accountId: string;
  budgetPeriod: BudgetPeriod;
  /** Null is no budget. */
  budgetUsd: number | null;
  eligible: MemberEligibility;
  kind: AccountKind;
  /** The next UTC day or month boundary, null until the ledger reports. */
  periodEnd: string | null;
  priority: number;
  /** The member's spend in the current period, null until the ledger reports or while it has no budget. */
  spentUsd: number | null;
}

/** A key on the pool, as the pool route lists it beside the pool. */
export interface PoolKey {
  enabled: boolean;
  id: string;
  label: string;
  prefix: string;
}

/** Whether a shedding round may mark a warm session: never, only once the cold ones fall short, or by burn alone. */
export type WarmSessions = 'always' | 'never' | 'when_needed';

export interface Pool {
  activePins: number;
  affinityMode: AffinityMode;
  /** Idle seconds before a session's pin to an account expires; 0 disables affinity. */
  affinityTtlSeconds: number;
  /** Whether the client's server-side auto mode checks travel; off drops them. */
  autoModeServer: boolean;
  /** Hours of captured request and response bodies for keys with no override; 0 is off. */
  bodyCaptureHours: number;
  /** The time constant of each pin's burn average, which a shedding round sizes its target from. */
  burnWindowSeconds: number;
  /** Null inherits the bootstrap option `compatibility.cache_diagnostics`; true or false overrides it for this pool. */
  cacheDiagnostics: boolean | null;
  /** Always answered; `enabled` off is how a pool that rewrites no breakpoint reads. */
  cacheTtl: PoolCacheTtl;
  cost24h: number;
  id: string;
  /** Requests in flight one member may hold before a fresh session skips it. */
  inFlightCap: number;
  keepalive: PoolKeepalive;
  keys: PoolKey[];
  members: PoolMember[];
  /** Null disables the shedding round. */
  migrateAboveUtilization: number | null;
  /** Whether a pin moves off a member whose window for its model is spent while a sibling has allowance left. */
  migrateWhenSpent: boolean;
  /** Under `by_warm_prefix`, the smallest estimated static prefix, in tokens, that places a fresh session. */
  minSharedPrefixTokens: number;
  modelRules: ModelRule[];
  /** Moves that landed on the pool's members in the last hour, by reason. */
  movesPerHour: Record<string, number>;
  name: string;
  /** Minutes a session stays on an upstream in overage while its window resets within them. */
  overageMoveGraceMinutes: number;
  overagePolicy: OveragePolicy;
  /** Under Pace, a member with no more than this share of its window left takes a fresh session only as the last resort. */
  paceFloor: number;
  /** Order the selection set by account kind before priority; the operator's answer alone. */
  preferSubscription: boolean;
  quotaHeaders: QuotaHeaders;
  requests24h: number;
  /** The longest throttle a request waits out on its own member before it fails over. */
  retryInPlaceSeconds: number;
  /** Null lets selection pick among every eligible member; otherwise it prefers those under this 5-hour utilization. */
  selectBelowUtilization: number | null;
  sessionPlacement: SessionPlacement;
  /** The share of a hot member's burn one shedding round moves. */
  shedShare: number;
  /** Seconds a member that shed waits before its next round, and a session shed off it stays away. */
  sourceDwellSeconds: number;
  /** `active` and `disabled` are set by hand; the rest are observed. */
  status: 'active' | 'degraded' | 'disabled' | 'exhausted';
  strategy: PoolStrategy;
  subagentPlacement: SubagentPlacement;
  traffic24h: TrafficBucket[];
  warmSessions: WarmSessions;
  /** Minutes before an upstream's subscription end from which it takes a fresh session only as the last resort. */
  windDownMinutes: number;
}

export type KeyStatus = 'active' | 'disabled' | 'expired' | 'revoked';

export interface ApiKey {
  affinitySource: AffinitySource;
  /** Null inherits the pool's `autoModeServer`; true or false overrides it for this key. */
  autoModeServer: boolean | null;
  /** Hours of captured bodies for this key; null inherits the pool, 0 turns capture off here. */
  bodyCaptureHours: number | null;
  createdAt: string;
  /** The stored switch, which `status` folds expiry and revocation over; an edit sends it back as it stands. */
  enabled: boolean;
  expiresAt: string | null;
  id: string;
  /** Requests in flight the key may hold; null is no cap. */
  inFlightCap: number | null;
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

/**
 * A request's auto-mode standing. Admission reads `server` (a server-side check asked for under an on setting) or
 * `stripped` (the `safeguards` member carried under an off setting, which the dispatch drops) whether or not the
 * request reaches an upstream; `body_shape` is a refusal for its body shape before any upstream saw it.
 */
export type AutoMode = 'body_shape' | 'server' | 'stripped';
export const AUTO_MODE_VALUES = ['body_shape', 'server', 'stripped'] as const satisfies readonly AutoMode[];
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

/** A candidate the selection passed over, with the obstacle in its way as the router named it. */
export interface SkippedCandidate {
  accountId: string;
  obstacle: string;
}

/**
 * Where a fresh placement's account came from: `content` the deepest warm message boundary, `static` the deepest
 * warm static boundary, `with_main` the parent agent's pin or another agent's pin of the session.
 */
export interface Placement {
  /** The matched boundary's depth; 0 for `with_main`. */
  depth: number;
  /** The prefix the entry promised, in tokens, which a hit reads up to; 0 for `with_main`. */
  prefixTokens: number;
  rung: 'content' | 'static' | 'with_main';
}

/** The holds a request waited through before this attempt was placed. */
export interface CohortHold {
  /** The cohort's account the last hold was placed by; null for a hold on the strategy's choice. */
  cohort: string | null;
  waitedOn: string[];
}

/** Why a selection landed where it did, as the router stored it on the attempt. */
export interface SelectionTrace {
  /** The cache-read tokens the response reported, folded in at completion; null until then or without a response. */
  cacheRead: number | null;
  cohortHold: CohortHold | null;
  /** The move reason when a fail-over or a move placed the attempt, null otherwise. */
  moved: string | null;
  /** Null for a renewal or a choice the strategy made. */
  placement: Placement | null;
  renewed: boolean;
  skipped: SkippedCandidate[];
  strategy: string;
  /** Group, priority, and the overage bit the choice was made in; null for a renewed pin. */
  tier: [number, number, number] | null;
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
  /** Null for an attempt the router stored no trace for. */
  selection: SelectionTrace | null;
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
  autoMode: AutoMode | null;
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
  /** A request whose session branched from another: the depth where the two part, and the session it branched from. */
  forkDepth: number | null;
  forkedFrom: string | null;
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
  /** `x-claude-code-request-class` as the client sent it; `null` when it sent none. */
  requestClass: string | null;
  requestTruncated: boolean;
  responseTruncated: boolean;
  /** The client's own session, the one its companion registers under. */
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
export type PinMigrationReason = 'cooldown' | 'drain' | 'mark' | 'overage' | 'return_to_plan' | 'shed' | 'spent';

/** Why the router released a session's pin without a move: the `reasons` of a `pin_expired` event, in the
 * order the selection found them. */
export type PinReleaseReason =
  | 'attempted'
  | 'budget'
  | 'client_error'
  | 'cooldown'
  | 'deleted'
  | 'disconnected'
  | 'expired'
  | 'funding_required'
  | 'overage'
  | 'paused'
  | 'reauth_required'
  | 'released'
  | 'spent'
  | 'subscription_ended'
  | 'unpaired'
  | 'unserved';

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

export type AnalyticsGroupBy = 'account' | 'all' | 'client' | 'effort' | 'key' | 'kind' | 'model' | 'pool' | 'request_class' | 'user_agent';

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

/** The placements of one rung over the range, counted off the attempts that carried usage. */
export interface PlacementRow {
  cacheReadTokens: number;
  /** Hits over placements; null for a rung with no placement. */
  hitShare: number | null;
  hits: number;
  placements: number;
  /** The prefix sizes the placements promised; a `with_main` placement promises none. */
  promisedTokens: number;
  /** `content`, `static` or `with_main`; a rung this build does not know reads as the route sent it. */
  rung: string;
}

/** A budgeted pool member's spend against its budget for the current period. */
export interface MemberBudget {
  accountId: string;
  budgetPeriod: BudgetPeriod;
  budgetUsd: number;
  /** The next UTC day or month boundary; null, with `spentUsd`, until the ledger reports. */
  periodEnd: string | null;
  poolId: string;
  spentUsd: number | null;
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

/** The dimensions the burn route groups on; `session` reads the twenty sessions with the most tokens. */
export type BurnGroup = 'account' | 'key' | 'request_class' | 'session';

/** One group's burn over one bucket, as `GET /admin/analytics/burn` answers it. */
export interface BurnPoint {
  at: string;
  /** Cache-read tokens. */
  cachedTokens: number;
  /** A subscription upstream's spend in the bucket as a fraction of its own 5-hour window, the band weighed in; null
   *  for an API-key upstream or one whose tier is unknown, and under every other grouping. */
  fiveHourShare: number | null;
  /** Whether a row of the bucket was priced through a 5-hour cell or tier factor no measurement has filled. */
  fiveHourUnmeasured: boolean;
  group: string;
  /** The same spend as a fraction of the upstream's 7-day window, unbanded. */
  sevenDayShare: number | null;
  /** The same on the 7-day window, whose cells are flagged apart from the 5-hour ones. */
  sevenDayUnmeasured: boolean;
  /** Uncached input tokens. */
  uncachedTokens: number;
  /** What the upstream spends in: a subscription's window share, even while no tier prices it, or an API key's
   *  dollars; null under every other grouping, and for an upstream the registry no longer holds. */
  unit: BurnUnit | null;
  /** An API-key upstream's estimated cost. */
  usd: number;
}

/** The cache-write tokens the first request after a move wrote, per bucket. */
export interface RewritePoint {
  at: string;
  rewriteTokens: number;
}

/** One completed 5-hour or 7-day window of a subscription upstream, as `GET /admin/analytics/reconciliations`
 *  answers it: the rise its readings showed against the share the router predicted for its own requests in it. */
export interface WindowReconciliation {
  accountId: string;
  computedAt: string;
  /** The instants of the run's first and last readings; the router's requests completed between them. */
  firstReadingAt: string;
  lastReadingAt: string;
  /** The last reading's utilization over the first's, as a fraction of the window. */
  observedRise: number;
  /** The share of the window the router priced its own requests at. */
  predictedShare: number;
  /** `observedRise` over `predictedShare`: 1.1 means the meter charged 1.1 times the table. */
  ratio: number;
  requests: number;
  /** Whether any request was priced through a cell or factor no measurement has filled, or an attempt on a model
   *  the cost table lacks was left out of the prediction. */
  unmeasured: boolean;
  /** `five_hour` or `seven_day`. */
  window: string;
  /** The reset the window's readings named. */
  windowEnd: string;
}

/** The reconciliations of the last 30 days, newest first, and the request retention that bounds them. */
export interface AnalyticsReconciliations {
  reconciliations: WindowReconciliation[];
  /** `stats.retention_days` as configured, zero meaning forever: a window older than the kept requests is not
   *  reconciled. */
  requestRetentionDays: number;
}

export type KeepaliveState = 'busy' | 'idle' | 'parked';

/** Whether a cache-keepalive companion is registered for the session and still reporting. */
export type CompanionState = 'none' | 'reporting' | 'silent';

/** Which figure a pin's burn reads: the share of the upstream's own 5-hour window per hour on plan quota, dollars per hour off it. */
export type BurnUnit = 'usd' | 'window_share';

export interface Session {
  accountId: string | null;
  /** The pin's own agent: `main`, or a subagent's id; null for a session with no pin. */
  agentId: string | null;
  agents: number;
  /** The pin's typed burn per hour in `burnUnit`, priced when read; 0 for a pin never renewed with usage, null with
   *  no pin at all, with one whose account left the registry, or on a subscription upstream whose tier is unknown. */
  burn: number | null;
  /** When `burn` was last folded, the instant the shedding preview decays it forward from; null with `burn`. */
  burnAt: string | null;
  /** Null for a session with no pin, or one whose account left the registry: no kind is left to pick a unit from. */
  burnUnit: BurnUnit | null;
  /** True when `burn` was priced through a cell or a factor the measurement campaign has not filled. */
  burnUnmeasured: boolean;
  /** The cache TTL in force at the pin's last refresh; null for a session with no pin. */
  cacheTtlMs: number | null;
  companion: CompanionState;
  /** When the companion last reported; set only while `companion` is `silent`. */
  companionSince: string | null;
  createdAt: string | null;
  epoch: number | null;
  /** The depth where a forked session parts from its base; null with `forkedFrom`. */
  forkDepth: number | null;
  /** The session this one branched from, null for one that branched from none. */
  forkedFrom: string | null;
  /** A generation no live key holds: the ledger's memory of an earlier holder of this key id. Read-only, no recipe, no release. */
  historical: boolean;
  /** `state` is null when the session carries no cache-warming recipe at all; the other three are null with it. */
  keepalive: {
    lastGuard: GuardVerdict | null;
    netFiveHourShare: number | null;
    /** True once any share in `netFiveHourShare` was priced through a stand-in cell or factor. */
    netFiveHourShareUnmeasured: boolean;
    netUsd: number | null;
    nextProbeAt: string | null;
    state: KeepaliveState | null;
  };
  /** The `created_at` of the key that owns the session. */
  keyGeneration: number;
  keyId: string;
  /** The input, cache-read and cache-write tokens of the pin's last renewal; null for a session with no pin. */
  lastInputTokens: number | null;
  /** The pin's last main, subagent or compaction turn, or a keepalive probe's renewal; null for a session with
   *  no pin. */
  lastRefreshAt: string | null;
  /** Last ledgered client request; null for a session that has not been seen in the ledger. */
  lastRequestAt: string | null;
  lastUsedAt: string | null;
  model: string | null;
  /** Whether the session is pinned to its account for as long as affinity holds. */
  pinned: boolean;
  /** Ledgered requests on this session; 0 means analytics and observe have nothing to show. */
  requests: number;
  sessionId: string;
  /** The account a shed round marked this pin's session to leave, kept off until the session moves; null off a
   *  pin never marked, or with no pin. */
  shedFrom: string | null;
}

export type RecipeState = 'armed' | 'held' | 'probing';
export type RecipeReason =
  | 'affinity_too_short'
  | 'awaiting_cache_evidence'
  | 'budget'
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

/** What the last request the agent observed did to its recipe: `auxiliary` and `foreign` both left it alone,
 * `rearm` and `restart` both fed it. Never a hold; `reason` names that separately. */
export type GuardVerdict = 'auxiliary' | 'foreign' | 'rearm' | 'restart';

/** One cache-warming recipe per agent: what the next probe replays and why it may be held. */
export interface KeepaliveRecipe {
  accountId: string | null;
  agentId: string;
  keyId: string;
  /** Null only before the agent's first observation. */
  lastGuard: GuardVerdict | null;
  /** This agent's last real request; null for an agent that has not sent one yet. */
  lastRequestAt: string | null;
  model: string;
  /** The share of the upstream's 5-hour window the agent's probes saved minus what they cost. */
  netFiveHourShare: number;
  /** True once any share in `netFiveHourShare` was priced through a stand-in cell or factor. */
  netFiveHourShareUnmeasured: boolean;
  netUsd: number;
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
  | 'meter_drift'
  | 'poll_backoff'
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

/** What a meter_drift event carried: the completed window whose ratio left the band. */
export interface MeterDriftDetail {
  account: string;
  observed_rise: number;
  predicted_share: number;
  ratio: number;
  requests: number;
  /** `five_hour` or `seven_day`. */
  window: string;
  window_end: number;
}

/** What a poll_backoff event carried: since when the upstream's usage poll has been answered 429. */
export interface PollBackoffDetail {
  account: string;
  backoff_since: number;
  /** Null once the hold has passed and the upstream's next slot may ask it. */
  next_attempt_at: number | null;
  rate_limited_in_a_row: number;
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

export interface LifecyclePage {
  deliveries: LifecycleDelivery[];
  nextCursor: string | null;
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

/** A model's fast-mode cells, USD per million tokens, which an answer served fast is priced at. */
export interface FastPrice { cacheReadPerM: number; cacheWrite1hPerM: number; cacheWrite5mPerM: number; inputPerM: number; outputPerM: number }

export interface ModelPrice {
  cacheReadPerM: number;
  cacheWrite1hPerM: number;
  cacheWrite5mPerM: number;
  /** The fast cells, null on a model that serves no fast mode. */
  fast: FastPrice | null;
  inputPerM: number;
  model: string;
  outputPerM: number;
}

/** The fraction of a Max 5x window one million tokens of one type consume; `measured` is null on a flagged cell,
 *  whose share is a stand-in derived from earlier meter readings. */
export interface PlanCell { measured: string | null; sharePerMillion: number; unmeasured: boolean }
export interface PlanModelCells { cacheRead: PlanCell; cacheWrite1h: PlanCell; cacheWrite5m: PlanCell; input: PlanCell; output: PlanCell }
/** The peak band on the 5-hour cells: a charge landing inside its hours in `zone` is weighed by `factor`. */
export interface PlanBand { effectiveFrom: string | null; factor: number; fromHour: number; measured: string; provisional: boolean; toHour: number; weekdays: boolean; windows: string[]; zone: string }
/** The share of the Max 5x window per million tokens, per window (`five_hour`, `seven_day`) and model. */
export interface CostTable { bands: PlanBand[]; effective: string; tier: string; version: string; windows: Record<string, Record<string, PlanModelCells>> }
/** A tier's capacity of one window over Max 5x's, dated or flagged like a cell. */
export interface TierFactor { factor: number; measured: string | null; unmeasured: boolean }
export interface TierFactors { measured: string; reference: string; tiers: Record<string, { fiveHour: TierFactor; sevenDay: TierFactor }> }
export interface PlanTables { cost: CostTable; tiers: TierFactors }

export interface Settings {
  about: { profile: string; publicUrl: string | null; version: string };
  access: { adminConfigured: boolean; viewerConfigured: boolean };
  /** What the bootstrap file's compatibility block sets, which the route reads and never writes. */
  compatibility: { cacheDiagnostics: boolean; profile: string };
  keepalive: { leaseSeconds: number; maxAgents: number; maxRequestBytes: number; maxRetainedBytes: number; maxSessions: number; maxWorkers: number; oauthPrewarmVerified: boolean };
  listeners: { admin: string; metrics: string; proxy: string };
  planTables: PlanTables;
  /** The price table shown to the user and used to cost the ledger. */
  pricing: { effectiveDate: string; models: ModelPrice[]; source: string };
  quota: { sampleRetentionDays: number };
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
