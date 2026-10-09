// Response shapes for the router's admin API. The mock client in src/mock
// produces these; a real client would fetch them.

export const ACCOUNT_KIND_VALUES = ['api_key', 'oauth'] as const;
export type AccountKind = (typeof ACCOUNT_KIND_VALUES)[number];
export type AccountStatus = 'cooldown' | 'funding_required' | 'paused' | 'reauth_required' | 'ready' | 'unpaired';
/** The three windows the pages know: the two shared ones and the model-scoped week. */
export type KnownWindowId = '5h' | '7d' | '7d_fable';
/** A known window, or the key of a window the pages do not know, which is shown under its name. */
export type WindowId = KnownWindowId | (string & {});

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
  /** What the cooldown covers, as the router spells it: `account`, `fast` (the fast lane's rate limit), `fast_credits` (its usage-credits refusal), or `model:<id>`. Clearing one names this. */
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

/** One fast-lane bucket's share used, as an answer's `anthropic-fast-*` headers reported it. */
export interface FastLaneReading {
  observedAt: string;
  resetsAt: string | null;
  utilization: number;
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
  /** The fast lane's input and output buckets as the router last read them; null until an answer to a fast request carried them. */
  fastLane: { input: FastLaneReading | null; output: FastLaneReading | null } | null;
  id: string;
  kind: AccountKind;
  label: string;
  /** The router's own reading of whether requests bill as overage; `since` is null when a usage sample, not a response, reported it. */
  overage: { inUse: boolean; observedAt: string; reset: string | null; since: string | null } | null;
  plan: string | null;
  /** An account belongs to one pool at most, so this is its one membership or null. */
  pool: PoolMembership | null;
  requests: { last24h: number; last7d: number; lifetime: number };
  /** The row's edit token, which an edit sends back as `expected_revision`; an account deleted and created again
   * under its id never answers one its predecessor did. */
  revision: number;
  routingEnabled: boolean;
  status: AccountStatus;
  subscription: Subscription | null;
  /** The poll's standing and its last snapshot's error and instant, as the usage route answers them; null for an
   * API-key account, which the poll never reads. */
  usage: QuotaSnapshot | null;
  /** The 5-hour and 7-day windows; the Fable window is `fableWindow`. */
  windows: RateLimitWindow[];
}

/** What the operator pasted: a whole callback URL, which carries its own state, or the bare code with the state the pairing started under. */
export type PairingInput = { code: string; state: string } | { url: string };

/** What a pairing starts on: an existing OAuth row, or a row the confirm creates under `id` with `label`. */
export type PairingStart = { accountId: string } | { id: string; label: string };

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
  /** The effort levels the listing says the model supports, in the API's order; empty when it names none. */
  effortLevels: string[];
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

/**
 * `GET /admin/models/state`: the live cooldowns grouped by model. The route also answers the newest rejections and
 * permits, which the console reads in the routing log instead.
 */
export interface ModelsState {
  cooldowns: { model: string; rows: RuleCooldown[] }[];
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

export type QuotaFetchStatus = 'error' | 'success';

/** How the usage endpoint answered an account's last attempt. */
export type PollOutcome = 'clean' | 'failed' | 'rate_limited';

/** The poll's standing for an OAuth account, kept by the router; null until the poll first asks. */
export interface QuotaPoll {
  /** The first 429 of the run the account is in; null outside one. */
  backoffSince: string | null;
  lastAttemptAt: string | null;
  /** When the windows of the last clean answer were read; null until one lands. */
  lastCleanAt: string | null;
  /** The message of the last attempt that was not clean; null after a clean one. */
  lastError: string | null;
  lastOutcome: PollOutcome;
  /** When the account may be asked again; null when its next slot may ask it. */
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

/**
 * The conditions that need the operator, worst first: the order the route ranks by and the dashboard's line draws,
 * so not alphabetical.
 */
export const ATTENTION_KIND_VALUES = ['pool_exhausted', 'client_drift', 'writer_degraded', 'certificate_reload_failed', 'reauth_required', 'funding_required', 'refresh_failed', 'certificate_expiring', 'ledger_record_parked', 'poll_backoff'] as const;
export type AttentionKind = (typeof ATTENTION_KIND_VALUES)[number];

/** One condition that holds now, as `GET /admin/attention` answers it. */
export interface AttentionItem {
  /** The members the condition's kind carries, passed through as the route sent them. */
  detail: Record<string, unknown>;
  kind: AttentionKind;
  /** Where the operator fixes it; null for a drift refusal that carried no key, a router subject and a deleted pool. */
  link: { id: string; page: 'accounts' | 'keys' | 'pools' } | null;
  /** The pools the item belongs to, which a pool card filters the same list by. */
  pools: string[];
  /** When the condition started, as an ISO instant. */
  since: string;
  /**
   * An account or a pool by id and label, a client version as both, or one of the router's own components as both:
   * the ledger or the routing state writer, a listener's pair by its key (`proxy.tls`, `admin.tls`), or `-` for a
   * parked record without a pool.
   */
  subject: { id: string; label: string; type: 'account' | 'client' | 'pool' | 'router' };
}

/** What `POST /admin/ledger/parked/replay` landed, and the parked records the ledger still refuses. */
export interface ReplayOutcome {
  landed: number;
  remaining: number;
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
export type BudgetPeriod = 'day' | 'month';
/** Where a fresh session with no pin lands: by the strategy, or where its static prefix is warm. */
export type SessionPlacement = 'by_strategy' | 'by_warm_prefix';
/** Where a subagent with no pin of its own lands. */
export type SubagentPlacement = 'by_cohort' | 'by_strategy' | 'with_main';

export interface PoolMember {
  accountId: string;
  /** Null is no budget over that period; a member may carry one, both, or neither. */
  budgetDailyUsd: number | null;
  budgetMonthlyUsd: number | null;
  /** The router's end for a member at a budget: the latest end among the budgets reached; null while none is. */
  budgetUntil: string | null;
  eligible: MemberEligibility;
  kind: AccountKind;
  priority: number;
  /** One report per budgeted period the ledger has summed; empty until it reports. */
  spends: { period: BudgetPeriod; periodEnd: string | null; spentUsd: number }[];
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
  /** The pool's switch for the cache-diagnosis beta and `diagnostics.previous_message_id`; off by default. */
  cacheDiagnostics: boolean;
  /** Always answered; `enabled` off is how a pool that rewrites no breakpoint reads. */
  cacheTtl: PoolCacheTtl;
  cost24h: number;
  id: string;
  /** Requests in flight one member may hold before a fresh session skips it. */
  inFlightCap: number;
  keepalive: PoolKeepalive;
  keys: PoolKey[];
  members: PoolMember[];
  /** Whether a pin moves off a member whose window for its model is spent while a sibling has allowance left. */
  migrateWhenSpent: boolean;
  /** Under `by_warm_prefix`, the smallest estimated static prefix, in tokens, that places a fresh session. */
  minSharedPrefixTokens: number;
  modelRules: ModelRule[];
  /** Moves that landed on the pool's members in the last hour, by reason. */
  movesPerHour: Record<string, number>;
  name: string;
  /** Minutes a session stays on an account in overage while its window resets within them. */
  overageMoveGraceMinutes: number;
  overagePolicy: OveragePolicy;
  /** Under Pace, a member with no more than this share of its window left takes a fresh session only as the last resort. */
  paceFloor: number;
  /** Order the selection set by account kind before priority; the operator's answer alone. */
  preferSubscription: boolean;
  requests24h: number;
  /** The longest throttle a request waits out on its own member before it fails over. */
  retryInPlaceSeconds: number;
  /** The row's edit token, which an edit sends back as `expected_revision`. A pool deleted and created again under
   * its id never answers one its predecessor did. */
  revision: number;
  /** Null lets selection pick among every eligible member; otherwise it prefers those under this 5-hour utilization. */
  selectBelowUtilization: number | null;
  sessionPlacement: SessionPlacement;
  /** Null disables the shedding round. */
  shedAheadOfPace: number | null;
  /** The share of a hot member's burn one shedding round moves. */
  shedShare: number;
  /** Seconds a member that shed waits before its next round, and a session shed off it stays away. */
  sourceDwellSeconds: number;
  /** `active` and `disabled` are set by hand; the rest are observed. */
  status: 'active' | 'degraded' | 'disabled' | 'exhausted';
  strategy: PoolStrategy;
  subagentPlacement: SubagentPlacement;
  traffic24h: TrafficBucket[];
  /** The stamp of the row's last write, which `revision` carries. */
  updatedAt: string;
  warmSessions: WarmSessions;
  /** Minutes before an account's subscription end from which it takes a fresh session only as the last resort. */
  windDownMinutes: number;
}

export type KeyStatus = 'active' | 'disabled' | 'expired' | 'revoked';

export interface ApiKey {
  /** Null inherits the pool's `autoModeServer`; true or false overrides it for this key. */
  autoModeServer: boolean | null;
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
  /** The row's edit token, which an edit sends back as `expected_revision`. A key deleted and created again under
   * its id never answers one its predecessor did. */
  revision: number;
  /** Live sessions whose newest request still presented the previous secret; 0 once the grace ends. */
  sessionsOnRetiredSecret: number;
  status: KeyStatus;
  /** The stamp of the row's last write, which `revision` carries. */
  updatedAt: string;
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
/** How a request ended, in the ledger's own words. */
export type RequestOutcome = 'canceled' | 'exhausted' | 'failed' | 'incomplete' | 'rejected' | 'succeeded';

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
  /** The holds' waits summed, in milliseconds, whether a slot freed or the bound ran out. */
  heldMs: number;
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
  /** The `id` of the message the answer carried, the join to a transcript's assistant entry; null without one. */
  upstreamMessageId: string | null;
  upstreamRequestId: string | null;
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
  /** The holds the request waited through, with the wait; null for a request never held. */
  held: CohortHold | null;
  id: string;
  /** The `created_at` of the key the request was read under, 0 for none; a key id created again is another holder. */
  keyGeneration: number;
  keyId: string | null;
  kind: RequestKind;
  model: string;
  /** How the request ended, once every attempt is in. */
  outcome: RequestOutcome;
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
export type PinMigrationReason = 'cooldown' | 'drain' | 'fast_lane' | 'mark' | 'overage' | 'return_to_plan' | 'shed' | 'spent';

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
  | 'fast_credits'
  | 'fast_rate_limit'
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
  /** Keep the requests that waited for a slot. */
  held?: boolean;
  includeProbes?: boolean;
  keyIds?: string[];
  kind?: RequestKind | null;
  limit?: number;
  models?: string[];
  outcomes?: RequestOutcome[];
  poolIds?: string[];
  search?: string;
  /** `fast`, `standard`: the serving attempt's speed, else `fast` for a request that asked for it. */
  speeds?: string[];
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

/** A decision's kind as the routing log names it. A probe's decision keeps its kind and the row's `probe` marks it. */
export type DecisionKind = 'exhausted' | 'held' | 'moved' | 'placed' | 'refused' | 'renewed';

/** Why a record pins nothing. `not_looked_up` is a refusal's, made before the pin lookup whatever session it named. */
export type PinReason = 'not_looked_up' | 'pinning_off';

/** How the console draws a fact's value. */
export type FactUnit = 'count' | 'ms' | 'percent' | 'percent_per_hour' | 'text' | 'time' | 'tokens' | 'usd';

export interface FactEntity {
  id: string;
  kind: 'account' | 'key' | 'model' | 'pool';
}

/**
 * One Fact-cell pair as the router sent it: a key from the fact vocabulary, a number or a string, and how to draw
 * it.
 */
export interface SummaryFact {
  entity: FactEntity | null;
  key: string;
  unit: FactUnit | null;
  value: number | string;
}

/** What decided the row, the By chip: a rule, a rung, the strategy, a hold, an exhaustion, a refusal or none. */
export interface DecisionBy {
  kind: 'exhaustion' | 'hold' | 'none' | 'refusal' | 'rule' | 'rung' | 'strategy';
  tier: [number, number, number] | null;
  value: string;
}

export interface Score {
  accountId: string;
  score: number;
  unit: string;
}

/** A decision row of the routing log: the summary the router wrote, with the facts of the attempt that completed it. */
export interface DecisionSummary {
  type: 'decision';
  accountId: string | null;
  agentId: string | null;
  at: string;
  attempt: number;
  /** The request's attempts, null until its row lands. */
  attemptCount: number | null;
  by: DecisionBy;
  /** Null until the completing attempt reports usage, and for a decision that sent nothing upstream. */
  cache: { hit: boolean; inputTokens: number; promisedTokens: number | null; readTokens: number } | null;
  exhaustion: { deadline: string | null; kind: string; obstacle: string | null; reason: string | null; window: string | null } | null;
  /** One pair, or two at the most. */
  fact: SummaryFact[];
  fast: boolean;
  fromAccountId: string | null;
  hold: { accountId: string; boundMs: number; cohort: string | null } | null;
  keyId: string | null;
  kind: DecisionKind;
  model: string | null;
  poolId: string | null;
  probe: boolean;
  refusal: { kind: string; reason: string } | null;
  requestId: string;
  rule: string | null;
  rung: Placement | null;
  runnerUp: Score | null;
  score: Score | null;
  /** The decision's place within its request, counted from 1: a hold and the placement after it share an attempt. */
  seq: number;
  sessionId: string | null;
  skipped: number;
  status: number | null;
  strategy: string;
  tier: [number, number, number] | null;
  warmth: { cause: string; verdict: string } | null;
}

/** An event row of the routing log, every kind of the ledger's events table. */
export interface RoutingEvent {
  type: 'event';
  accountId: string | null;
  actor: string | null;
  agentId: string | null;
  at: string;
  /** The kind's own fields, decoded, which the console reads by name through `routingLog.ts`. */
  detail: Record<string, unknown>;
  id: number;
  keyId: string | null;
  kind: string;
  model: string | null;
  poolId: string | null;
  requestId: string | null;
  sessionId: string | null;
}

export type RoutingLogRow = DecisionSummary | RoutingEvent;

export interface RoutingLogPage {
  /** Answered under a `request` filter alone, the one bounded set. */
  counts: { decisions: number; events: number } | null;
  nextCursor: string | null;
  rows: RoutingLogRow[];
}

/** `from` and `to` are ISO instants, sent as they are: the route reads RFC 3339. */
export interface RoutingLogQuery {
  accountIds?: string[];
  cursor?: string;
  from?: string;
  keyIds?: string[];
  /** Decision kinds, event kinds and the pseudo-kind `probe`. Absent, it is the route's default set. */
  kinds?: string[];
  limit?: number;
  models?: string[];
  poolIds?: string[];
  request?: string;
  /** A session id or its prefix. */
  session?: string;
  to?: string;
}

/** The request's facts as the engine read them. */
export interface RecordRequest {
  agentId: string;
  at: string;
  attempt: number;
  attempted: string[];
  canonicalModel: string;
  chain: { boundaries: number; staticBoundaries: number; staticHash: boolean };
  clientClass: string;
  flags: { contextCompacted: boolean; count: boolean; fast: boolean; marker: boolean; portable: boolean; probe: boolean };
  /** The holds this request waited through before the decision, each with the wire attempt that waited. */
  holds: { accountId: string; attempt: number; cohort: string | null; heldMs: number }[];
  keyGeneration: number;
  keyId: string;
  lane: { effort: string; speed: string; thinking: string };
  parentAgentId: string | null;
  poolId: string;
  registryRevision: number;
  requestClass: string | null;
  requestId: string;
  seq: number;
}

/** Step 1's reading of the pin, its row's fields null when none was found. */
export interface RecordPin {
  accountId: string | null;
  createdAt: string | null;
  epoch: number | null;
  /** `live`, `expired_idle`, `left_pool` or `none`. */
  found: string;
  idleMs: number | null;
  incarnation: number | null;
  lane: string | null;
  lastRefreshAt: string | null;
  lastUsedAt: string | null;
  marked: boolean;
  refreshAgeMs: number | null;
  released: string[];
  shed: { at: string; from: string } | null;
  warmth: string | null;
  warmthCause: string | null;
}

/** The values that entered the decision, each named as the pool row names the field it was read from. */
export interface RecordSettings {
  affinityMode: string;
  affinityTtlSeconds: number;
  burnWindowSeconds: number;
  inFlightCap: number;
  migrateBand: number;
  migrateWhenSpent: boolean;
  minSharedPrefixTokens: number;
  overageMoveGraceMinutes: number;
  overagePolicy: string;
  paceFloor: number;
  poolUpdatedAt: string;
  preferSubscription: boolean;
  retryInPlaceSeconds: number;
  selectBelowUtilization: number | null;
  sessionPlacement: string;
  shedAheadOfPace: number | null;
  shedShare: number;
  sourceDwellSeconds: number;
  strategy: string;
  subagentPlacement: string;
  warmSessions: string;
  windDownMinutes: number;
}

/** A member's spend against one budget. The spend and its period are null while the ledger has reported none. */
export interface RecordBudget {
  budgetUsd: number;
  observedAt: string | null;
  period: string;
  periodEnd: string | null;
  periodStart: string | null;
  spentUsd: number | null;
}

export interface RecordCooldown {
  /** Whether the row kept this request off the member. */
  blocking: boolean;
  reason: string;
  scope: string;
  until: string;
  window: string;
}

export interface RecordCredits {
  blocked: string | null;
  disabledReason: string | null;
  enabled: boolean;
  observedAt: string;
  utilization: number | null;
}

/** One entry the rung walk visited on a member. `refusedBy` names the gate that refused it, null for one accepted. */
export interface PrefixEntry {
  depth: number;
  /** The boundary's chain hash, 64 lowercase hex characters, as the affinity map keys it. */
  hash: string;
  live: boolean;
  prefixTokens: number;
  refusedBy: string | null;
  /** The lifetime left as of `writtenAt`, 0 for a link the cache never confirmed. */
  ttlMs: number;
  writtenAt: string;
}

export interface RecordCandidate {
  accountId: string;
  allowanceKnown: boolean;
  budget: RecordBudget[];
  cooldowns: RecordCooldown[];
  credits: RecordCredits | null;
  enabled: boolean;
  inFlight: number;
  inFlightCap: number;
  inOverage: boolean;
  incarnation: number;
  kind: 'api_key' | 'oauth';
  obstacle: { name: string; reason: string | null; until: string | null; window: string | null } | null;
  overage: { observedAt: string; reset: string | null; since: string; source: string } | null;
  pace: { deadline: string | null; end: string | null; hours: number | null; remaining: number; reset: string | null; score: number } | null;
  partition: string | null;
  planTier: string | null;
  /**
   * Per rung the deepest visited entry and, apart from it, the one the rung accepted on this member. The card's chips
   * read the match.
   */
  prefix: { content: PrefixEntry | null; contentMatch: PrefixEntry | null; static: PrefixEntry | null; staticMatch: PrefixEntry | null; withMain: boolean };
  priority: number;
  rank: number | null;
  score: number | null;
  state: string;
  term: { cancelled: boolean; ended: boolean; endsAt: string | null; since: string | null; trialEndsAt: string | null; windingDown: boolean } | null;
  tier: [number, number, number];
  verdict: string;
  windows: Record<string, { ageMs: number; ahead: number | null; observedAt: string; reset: string | null; source: string; utilization: number }>;
}

/** One step of the rule walk in the engine's order, with the facts it read. */
export interface WalkStep {
  facts: WalkFact[];
  step: string;
  verdict: string;
}

/** A fact's value is the engine's own: a number, a word, a flag or a list. */
export interface WalkFact {
  accountId: string | null;
  key: string;
  unit: string | null;
  value: unknown;
}

/** What the record route adds from the attempt row at read time, null for a decision that sent nothing upstream. */
export interface RecordCompletion {
  cacheMapWritten: { accountId: string; depth: number; prefixTokens: number; tier: string; ttlMs: number }[] | null;
  cacheReadTokens: number | null;
  completedAt: string | null;
  freed: boolean;
  heldMs: number | null;
  hit: boolean | null;
  inputTokens: number | null;
  promisedTokens: number | null;
  status: number | null;
}

export interface RecordOutcome {
  accountId: string | null;
  cohortHold: CohortHold | null;
  completion: RecordCompletion | null;
  exhaustion: { deadline: string | null; kind: string; obstacle: string | null; reason: string | null; window: string | null } | null;
  fromAccountId: string | null;
  hold: { accountId: string; boundMs: number; cohort: string | null } | null;
  inOverage: boolean;
  kind: DecisionKind;
  /** In the routing task's emit order: two for a replaced expired pin, `pin_expired` then `pin_created`. */
  pinEvents: { kind: string; reason: string | null }[];
  pinWritten: { action: string; epoch: number; hashesStamped: boolean; lane: string; refreshStamped: boolean } | null;
  refusal: { kind: string; reason: string } | null;
  rule: string | null;
  rung: Placement | null;
  /** The pins the shedding round this decision ran marked, or null when no round ran. */
  shedMarks: { agentId: string; keyId: string; model: string; sessionId: string }[] | null;
  strategy: string;
  tier: [number, number, number] | null;
  upstreamModel: string | null;
}

/** Record v1 as the record route answers it, every block in camelCase. `raw` is the route's
 * answer as it arrived, kept for the card's Copy as JSON, and `settings` null for a refusal under a key or pool the
 * registry no longer held. */
export interface DecisionRecord {
  candidates: RecordCandidate[];
  outcome: RecordOutcome;
  pin: RecordPin | null;
  pinReason: PinReason | null;
  raw: unknown;
  request: RecordRequest;
  requestId: string;
  seq: number;
  settings: RecordSettings | null;
  version: number;
  walk: WalkStep[];
}

export type AnalyticsGroupBy = 'account' | 'all' | 'client' | 'effort' | 'key' | 'kind' | 'model' | 'pool' | 'request_class' | 'speed' | 'user_agent';

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
  /** The member's account kind: a subscription member's spend is its metered spend alone. */
  kind: AccountKind;
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
  incomplete: number;
  keptWarm: { requests: number; tokens: number } | null;
  key: string;
  /**
   * An earlier holder's creation in Unix milliseconds under the key grouping, 0 for one with no creation time. Null
   * for the live key, for every other dimension and for a group folded from request rows.
   */
  keyCreatedAt: number | null;
  /** The key's id for a key group the route answered, so the label is built in the viewer's zone; null otherwise. */
  keyId: string | null;
  label: string;
  percentiles: Record<keyof LatencyHistograms, { p50: number | null; p90: number | null; p95: number | null; p99: number | null }>;
  probes: { failed: number; held: number; migrated: number; ok: number; skipped: number };
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

/** Why probes a recipe had already paid for went unanswered: the stop that left them, or a request after them that read no cache. */
export type WastedProbeReason = 'cold_return' | 'evicted' | 'expired' | 'idle_limit' | 'released' | 'replaced' | 'session_ended';

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
  /** A subscription account's spend in the bucket as a fraction of its own 5-hour window, the band weighed in; null
   *  for an API-key account, one whose tier is unknown, a bucket with no row the cost model prices (an idle hour among
   *  them) or one holding a row the cost model cannot price, and under every other grouping. */
  fiveHourShare: number | null;
  group: string;
  /** The same spend as a fraction of the account's 7-day window, unbanded. */
  sevenDayShare: number | null;
  /** Every token of the bucket, all five types: zero for a bucket with no traffic the ledger can attribute. */
  tokens: number;
  /** Uncached input tokens. */
  uncachedTokens: number;
  /** What the account spends in: a subscription's window share, even while no tier prices it, or an API key's
   *  dollars; null under every other grouping, and for an account the registry no longer holds. */
  unit: BurnUnit | null;
  /** An API-key account's estimated cost. */
  usd: number;
}

/** The cache-write tokens the first request after a move wrote, per bucket. */
export interface RewritePoint {
  at: string;
  rewriteTokens: number;
}

export type KeepaliveState = 'busy' | 'idle' | 'parked';

/** Whether a cache-keepalive companion is registered for the session and still reporting. */
export type CompanionState = 'none' | 'reporting' | 'silent';

/** Which figure a pin's burn reads: the share of the account's own 5-hour window per hour on plan quota, dollars per hour off it. */
export type BurnUnit = 'usd' | 'window_share';

export interface Session {
  accountId: string | null;
  /** The pin's own agent: `main`, or a subagent's id; null for a session with no pin. */
  agentId: string | null;
  agents: number;
  /** The pin's typed burn per hour in `burnUnit`, priced when read; 0 for a pin never renewed with usage, null with
   *  no pin at all, with one whose account left the registry, on a subscription account whose tier is unknown, or
   *  on a model it has no weight for, or a tier it has no capacity for on that window. */
  burn: number | null;
  /** When `burn` was last folded, the instant the shedding preview decays it forward from; null with no pin, or for a
   *  pin never folded. */
  burnAt: string | null;
  /** Null for a session with no pin, or one whose account left the registry: no kind is left to pick a unit from. */
  burnUnit: BurnUnit | null;
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
  | 'evicted'
  | 'expired'
  | 'idle_limit'
  | 'ledger_behind'
  | 'net_negative'
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
  /** The share of the account's 5-hour window the agent's probes saved minus what they cost. */
  netFiveHourShare: number;
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
  /** Times the keepalive engine found it had missed bus events; each gap discarded every observation still open. */
  busGaps: number;
  costToday: { capUsd: number | null; poolId: string; usd: number }[];
  /** Requests whose prefix expired between turns in the last 24 hours. */
  expiredBetweenTurns24h: number;
  netSaved7dUsd: number;
  probes24h: { failed: number; held: number; migrated: number; ok: number; skipped: number };
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

export interface TypeMultiplier { cacheRead: number; cacheWrite1h: number; cacheWrite5m: number; input: number; output: number }
/** The subscription cost model as the ledger loaded it, the file's keys in camel case and its maps in the file's order. */
export interface SubscriptionCosts {
  capacity: Record<string, { fiveHour: number | null; sevenDay: number | null }>;
  effective: string;
  modelWeight: Record<string, number | null>;
  modelWindows: Record<string, { capacityFactor: number; of: string }>;
  overrides: Record<string, { typeMultiplier?: Partial<TypeMultiplier>; weightByWindow?: Record<string, number> }>;
  peak: { capacityFactor: { fiveHour: number; sevenDay: number }; fromHour: number; toHour: number; weekdays: boolean; zone: string };
  typeMultiplier: TypeMultiplier;
  unit: { definedBy: { band: string; tier: string; window: string }; unitsPerWindow: number };
}

export interface Settings {
  about: { profile: string; version: string };
  access: { adminConfigured: boolean };
  /**
   * The price table used to cost the ledger, its rows in the file's order. `effective` is the date as the router sent
   * it, `effectiveDate` the instant it parses to, or null when it is not a date.
   */
  apiPrices: { effective: string; effectiveDate: string | null; models: ModelPrice[]; source: string };
  keepalive: { maxAgents: number; maxRetainedBytes: number; maxSessions: number };
  listeners: { metrics: string; proxy: string };
  retention: { bodyCaptureMaxHours: number };
  /** How long a session stays listed after its last activity. */
  sessions: { expireAfterSeconds: number };
  /** Each path is null when no override file is configured. */
  storage: { apiPricesPath: string | null; ledgerPath: string; path: string; subscriptionCostsPath: string | null };
  /**
   * The envelope's `content` with its `source` beside it; an answer without the envelope reads as an empty model.
   * `problem` is the sentence the cost views show in place of a model the answer carries and the console cannot read,
   * which then reads as the empty model too; null otherwise.
   */
  subscriptionCosts: SubscriptionCosts & { problem: string | null; source: string };
  usage: { cacheTtlSeconds: number };
}

export interface SessionInfo {
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
