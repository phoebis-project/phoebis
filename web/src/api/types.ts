// Response shapes for the router's admin API. The mock client in src/mock
// produces these; a real client would fetch them.

export const ACCOUNT_KIND_VALUES = ['api_key', 'oauth'] as const;
export type AccountKind = (typeof ACCOUNT_KIND_VALUES)[number];
export const FEATURE_WORD_VALUES = ['artifacts', 'connectors'] as const;
export type FeatureWord = (typeof FEATURE_WORD_VALUES)[number];
/** Per feature, the id of the account its calls act under, or null for none. */
export type FeatureAccountIds = Record<FeatureWord, string | null>;
export const KEY_FEATURE_MODE_VALUES = ['account', 'inherit', 'none'] as const;
/** A key's value for one feature: an account by its id, the pool's account, or none. Each arm holds `accountId`, as
 * the wire does, so each row has one set of members. */
export type KeyFeatureAccount =
  | { accountId: string; mode: 'account' }
  | { accountId: null; mode: 'inherit' }
  | { accountId: null; mode: 'none' };
export type AccountStatus = 'cooldown' | 'funding_required' | 'paused' | 'reauth_required' | 'ready' | 'unpaired';
/** The three windows the pages know: the two shared ones and the model-scoped week. */
export type KnownWindowId = '5h' | '7d' | '7d_fable';
/** A known window, or the key of a window the pages do not know, which is shown under its name. */
export type WindowId = KnownWindowId | (string & {});

export interface RateLimitWindow {
  /** The share of the window the sessions on the account burn per hour, null where nothing is projected. */
  burn: number | null;
  /**
   * The share per hour the sessions would burn unpaced, the burn over the speed the slow-down realized: equal to `burn`
   * on an account no slow-down holds, null where nothing is projected.
   */
  demand: number | null;
  /** The instant the projection runs to: the reset, the account's end, or five hours out on a weekly window. */
  horizon: string | null;
  id: WindowId;
  label: string;
  /** The router's lead over pace at the answer's instant: the utilization less the share of the window elapsed toward
   *  the earlier of its reset and the account's end. Null while the utilization is unknown. */
  lead: number | null;
  periodHours: number;
  /** Observed as of `observedAt`; may be stale for accounts that cannot refresh. */
  observedAt: string;
  /** The prediction plus the burn to `horizon`. Null for a window with no reading or an account in no pool. */
  projection: number | null;
  /** Null while the sampler has seen a utilization but no reset time for this window. */
  resetsAt: string | null;
  /** The share used, null where the router holds no reading of it. */
  utilization: number | null;
}

export interface ExtraUsage {
  /** Set when the provider stopped funding overage, e.g. a failed card. */
  disabledReason: string | null;
  enabled: boolean;
  inUse: boolean;
  /** The amounts are the newest polled row's, null while no polled row of the last day carries them. */
  monthlyLimitCredits: number | null;
  usedCredits: number | null;
}

/** A usage-based seat's one window: what it has spent of its monthly limit, in its currency at API rates. */
export interface Spend {
  /**
   * The newest polled row's amounts in minor units, null while no polled row of the last day carries them, with
   * `limitMinor` null where no limit is set.
   */
  amounts: { currency: string; exponent: number; limitMinor: number | null; polledAt: string; usedMinor: number } | null;
  observedAt: string;
  /**
   * `head` from the newest head's reset header while ahead of now, else `estimated` for 00:00 UTC on the first of the
   * next month.
   */
  resetSource: 'estimated' | 'head';
  resetsAt: string | null;
  /** The share of the limit spent, the heads moving it between polls, null where no row carries one. */
  utilization: number | null;
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

/**
 * The resolved plan, one word the router resolves from the stored profile's organization type first and its rate-limit
 * tier second, the usage-based class above both. The router stores whatever word it resolves, so a word added after
 * these eight is kept as sent.
 */
export const PLAN_WORDS = ['enterprise', 'free', 'max', 'max_20x', 'max_5x', 'pro', 'team', 'usage_based'] as const;
export type PlanWord = string;

export interface Subscription {
  billingType: string;
  /** The operator's saved end, standing only while `Account.canceled` is set, and null otherwise. */
  endsAt: string | null;
  organizationType: string;
  /** The plan the router resolved from the profile's organization type and tier, null while no profile document stands. */
  plan: PlanWord | null;
  /** `organization.plan_display_name` of the stored profile (`Ultra` on the `default_claude_max_20x` tier), null when
   *  the document lacks it. */
  planDisplayName: string | null;
  rateLimitTier: string;
  /** Epoch milliseconds on the wire, decoded to the ISO string every other timestamp carries, and `null` when the
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

/** The slow-down entry in force on an account, as the router last read it at a pinned selection. */
export interface AccountSlowDown {
  /** The burn the 5-hour window can still carry to its reset over the demand, from 0 to 1: under 1 the account is slowed. */
  factor: number;
  /** The pool's `slow_down_hold_seconds`, the longest one request waits, in milliseconds. */
  holdMs: number;
  /** The share of one burn average passed since the span opened, 1 once the average has passed, null outside a span. */
  ramp: number | null;
  /** The span's own start: when the factor first read under 1 in it, null outside a span. */
  since: string | null;
  /** The speed the holds realized, which the demand divides the pins' burn by. */
  speed: number;
  /** How long before the reset the sessions' unpaced demand would pass the window's ceiling, 0 when it never does. */
  stretchMs: number;
  /** The 5-hour projection at the window's horizon less the threshold, the figure of the router's stretch test.
   *  Null where that instant lies at or before the read. */
  stretchProjection: number | null;
  /** The pool's `slow_down_stretch_seconds` at the read, in milliseconds. */
  thresholdMs: number;
}

export const RESET_GRANT_STATE_VALUES = ['blocked', 'cooldown', 'ended', 'paused', 'usable', 'used', 'waiting'] as const;
/** `unknown` stands for a state word this console does not know. */
export type ResetGrantState = (typeof RESET_GRANT_STATE_VALUES)[number] | 'unknown';

/** One grant of an account's limit resets, as the account row answers it. */
export interface ResetGrant {
  /** The windows that block the grant: the console's window id where it knows the name, else the name as sent. */
  blocking: WindowId[];
  /** The windows the grant clears, read as `blocking` is. */
  clears: WindowId[];
  endsAt: string;
  id: string;
  /** `fable`, `full`, `session`, `weekly` or the upstream's word as sent. */
  kind: string;
  label: string;
  /** The upstream's reason as sent on a `blocked` grant, null on every other state. */
  reason: string | null;
  resetsLeft: number;
  resetsTotal: number;
  startsAt: string;
  state: ResetGrantState;
  /** The use through this router, null on a grant with a reset left and on one used elsewhere. */
  usedAt: string | null;
}

/** An account's one-time limit resets, null on the row where the router read none. */
export interface LimitResets {
  cooldownUntil: string | null;
  eligible: boolean;
  /** The Fable week's used share of the 7-day window as a fraction of 1, null where the poll read none. */
  fableWeekShare: number | null;
  grants: ResetGrant[];
  ineligibleReason: string | null;
  /** The resets left on the grants that have not ended. */
  left: number;
  readAt: string;
  /** The token a use sends back, so the claim names the account and the state of its block that this member shows. */
  revision: number;
}

/** What the read before the dialog answers: the member as it stands, and the read's error where it failed. */
export interface LimitResetsRead {
  error: string | null;
  limitResets: LimitResets | null;
}

export const RESET_OUTCOME_VALUES = ['refused', 'unsettled', 'used'] as const;
export type ResetOutcome = (typeof RESET_OUTCOME_VALUES)[number];

/** What a use answers for a claim whose answer the router parsed, which names a `result`. */
export interface ResetUse {
  cleared: WindowId[];
  cooldownUntil: string | null;
  limitResets: LimitResets | null;
  /** The router's class. A word this console does not know reads `unsettled`, never a refusal. */
  outcome: ResetOutcome;
  reason: string | null;
  resetsLeft: number | null;
  /** The upstream's word as sent. */
  result: string;
  weeklyResetsAt: string | null;
}

export interface Account {
  /** Set only on an OAuth account; an api_key account always reads false. */
  canceled: boolean;
  /** The eight-character pseudonym the companion's allowance route lists the account under. */
  companionId: string;
  cooldowns: Cooldown[];
  credential: Credential;
  email: string | null;
  extraUsage: ExtraUsage | null;
  /** Null when the provider reports no Fable window for this plan. */
  fableWindow: RateLimitWindow | null;
  /** The fast lane's input and output buckets as the router last read them, and `null` until an answer to a fast
   * request carried them. */
  fastLane: { input: FastLaneReading | null; output: FastLaneReading | null } | null;
  /** The features the stored credential's scopes cover, in alphabetical order: empty for an OAuth account with no
   * stored credential, and null for an api_key account. */
  featureGrants: FeatureWord[] | null;
  id: string;
  kind: AccountKind;
  label: string;
  limitResets: LimitResets | null;
  /** The router's own reading of whether requests bill as overage; `since` is null when a usage sample, not a response, reported it. */
  overage: { inUse: boolean; observedAt: string; reset: string | null; since: string | null } | null;
  plan: string | null;
  /** The instant a profile read found the account's organization holding no plan, null while a plan stands. */
  planEndedAt: string | null;
  /** An account belongs to one pool at most, so this is its one membership or null. */
  pool: PoolMembership | null;
  requests: { last24h: number; last7d: number; lifetime: number };
  /** The row's edit token, which an edit sends back as `expected_revision`; an account deleted and created again
   * under its id never answers one its predecessor did. */
  revision: number;
  routingEnabled: boolean;
  /**
   * Null on an api_key account, an untiered account or one with no 5-hour reading, an account in no pool, a pool with
   * the slow-down off, and an account no pinned selection has read since the last purge or restart.
   */
  slowDown: AccountSlowDown | null;
  /** Null on every account but a usage-based seat. */
  spend: Spend | null;
  status: AccountStatus;
  subscription: Subscription | null;
  /** The poll's standing and its last snapshot's error and instant, as the usage route answers them, and `null` for an
   * API-key account, which the poll never reads. */
  usage: QuotaSnapshot | null;
  /**
   * An OAuth seat billed in dollars at API rates with no plan windows, classed from its stored profile's tier. It is
   * never in overage, whatever its extra usage flags read.
   */
  usageBased: boolean;
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

/** The confirm's answer: the paired account, and whether the confirm started that account's first usage poll, so its
 *  windows land a few seconds after the answer. */
export interface PairingConfirmed {
  accountId: string;
  firstPollInFlight: boolean;
  status: string;
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

export const MODEL_RULES_MODE_VALUES = ['extend', 'replace'] as const;
/** How a key's own list meets its pool's: `extend` appends the pool's entries after the key's own, `replace` leaves them out. */
export type ModelRulesMode = (typeof MODEL_RULES_MODE_VALUES)[number];

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

export interface QuotaSample {
  at: string;
  /** Null for a reading with no figure, such as one that reported the reset alone. */
  utilization: number | null;
}

export interface QuotaHistory {
  accountId: string;
  samples: QuotaSample[];
  windowId: WindowId;
}

export const POLL_OUTCOME_VALUES = ['clean', 'failed', 'rate_limited'] as const;
/** How the usage endpoint answered an account's last attempt. */
export type PollOutcome = (typeof POLL_OUTCOME_VALUES)[number];

/** The poll's standing for an OAuth account, kept by the router, and `null` until the poll first asks. */
export interface QuotaPoll {
  /** The first 429 of the run the account is in, and `null` outside one. */
  backoffSince: string | null;
  lastAttemptAt: string | null;
  /** When the windows of the last clean answer were read, and `null` until one lands. */
  lastCleanAt: string | null;
  /** The message of the last attempt that was not clean, and `null` after a clean one. */
  lastError: string | null;
  lastOutcome: PollOutcome;
  /** When the account may be asked again, and `null` when its next slot may ask it. */
  nextAttemptAt: string | null;
  /** 429s answered in a row; 0 after a clean answer. */
  rateLimitedInARow: number;
}

export interface QuotaSnapshot {
  accountId: string;
  error: string | null;
  fetchedAt: string | null;
  /** The instant a held answer's next attempt may run, ISO. Null outside a hold. */
  heldUntil: string | null;
  poll: QuotaPoll | null;
}

/**
 * The conditions that need the operator, worst first: the order the route ranks by and the dashboard's line draws,
 * so not alphabetical.
 */
export const ATTENTION_KIND_VALUES = ['pool_exhausted', 'client_drift', 'writer_degraded', 'certificate_reload_failed', 'reauth_required', 'funding_required', 'feature_account_unavailable', 'refresh_failed', 'pairing_expiring', 'certificate_expiring', 'limit_reset_ending', 'ledger_record_parked', 'keepalive_bus_gaps', 'poll_backoff', 'model_unpriced', 'meter_drift', 'wall_reached', 'destination_spent', 'shed_no_runway', 'slowed_long', 'session_cap', 'keepalive_cap', 'expired_between_turns'] as const;
/** `unknown` stands for a kind this console does not know, which the route never answers under that name. */
export type AttentionKind = (typeof ATTENTION_KIND_VALUES)[number] | 'unknown';

export const ATTENTION_PAGE_VALUES = ['accounts', 'keys', 'pools'] as const;
export const ATTENTION_SUBJECT_VALUES = ['account', 'client', 'model', 'pool', 'router'] as const;
/** One condition that holds now, as `GET /admin/attention` answers it. */
export interface AttentionItem {
  /** The members the condition's kind carries, passed through as the route sent them. */
  detail: Record<string, unknown>;
  kind: AttentionKind;
  /**
   * Where the operator fixes it, null for a drift refusal that carried no key, a router subject, a deleted pool, the
   * unpriced model item and a kind the console does not know.
   */
  link: { id: string; page: (typeof ATTENTION_PAGE_VALUES)[number] } | null;
  /** The pools the item belongs to, which a pool card filters the same list by. */
  pools: string[];
  /** The wire's kind where `kind` is `unknown`. Null on a known kind. */
  rawKind: string | null;
  /** When the condition started, as an ISO instant. */
  since: string;
  /**
   * An account or a pool by id and label, a client version as both, a model's undated id as both, or one of the
   * router's own components as both: the ledger or the routing state writer, a listener's pair by its key
   * (`proxy.tls`, `admin.tls`), the feature CA as `features.ca`, or `-` for a parked record without a pool. The
   * keepalive engine reads `keepalive`, and `keepalive.retained` for its byte cap, under the label `Keepalive`. A
   * session cap reads its config key (`sessions.max_sessions`, `sessions.max_agents`) under the label `Sessions`.
   */
  subject: { id: string; label: string; type: (typeof ATTENTION_SUBJECT_VALUES)[number] };
}

/** What `POST /admin/ledger/parked/replay` landed, and the parked records the ledger still refuses. */
export interface ReplayOutcome {
  landed: number;
  remaining: number;
}

export const POOL_STRATEGY_VALUES = ['pace', 'priority', 'round_robin'] as const;
export type PoolStrategy = (typeof POOL_STRATEGY_VALUES)[number];
export const OVERAGE_POLICY_VALUES = ['allow', 'never', 'prefer_plan'] as const;
export type OveragePolicy = (typeof OVERAGE_POLICY_VALUES)[number];

/** The lowest and the highest figure a pool's combinations resolve to, equal when every combination agrees. */
export interface Range {
  max: number;
  min: number;
}

export interface PoolKeepalive {
  enabled: boolean;
  /**
   * The idle window of the main conversation in seconds. 0 is the recipe's break-even, and a value above the break-even
   * runs as set.
   */
  maxIdleSeconds: number;
  /** How long a park on a background command or a subagent is probed. Null is the recipe's break-even. */
  maxParkedSeconds: number | null;
  /** A park on a scheduled wakeup is probed while the wake is at most this far away. Null is the recipe's break-even. */
  scheduledHorizonSeconds: number | null;
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
  /** The dollars of the bucket's requests no subscription window paid. */
  billedUsd: number;
  costUsd: number;
  /** The plan-funded requests as the Pro-week figure, null where a request priced nothing and 0 with no plan-funded
   *  request. */
  equivalent: number | null;
  failed: number;
  requests: number;
  tokens: number;
}

export const ELIGIBILITY_VALUES = ['budget', 'budget_unknown', 'cooldown', 'eligible', 'overage', 'spent', 'subscription_ended', 'unavailable', 'unknown'] as const;
/** Why routing would refuse a pool member, or `eligible` when it would not. */
export type MemberEligibility = (typeof ELIGIBILITY_VALUES)[number];
/** A member's fit for a fresh session, from best to worst: a word's rank is its place in the list. */
export const MEMBER_FIT_VALUES = ['fits', 'rewrite_only', 'least_bad', 'ending', 'spent'] as const;
export type MemberFit = (typeof MEMBER_FIT_VALUES)[number];
/** A fresh session's fit on a member under one regime of the cost model in force. */
export interface RegimeFit {
  /** The worst word of the member's 5-hour and 7-day windows under the regime, null for a word outside
   *  `MEMBER_FIT_VALUES`, which has no rank in the order. */
  fit: MemberFit | null;
  /** The regime's name in the cost model, null for the base regime. */
  regime: string | null;
  /** The runway the router ranks the member by among the members of its fit, to the pool's ceiling under `fits` and
   *  `rewrite_only` and to 1.0 under `least_bad`. Null for an unbounded runway and under `ending` and `spent`. */
  runwayMs: number | null;
}

export const AFFINITY_MODE_VALUES = ['always', 'when_cached'] as const;
/** Whether a session's pin is made for every request, or only for one whose body carries a cache marker. */
export type AffinityMode = (typeof AFFINITY_MODE_VALUES)[number];
export const BUDGET_PERIOD_VALUES = ['day', 'month'] as const;
export type BudgetPeriod = (typeof BUDGET_PERIOD_VALUES)[number];
export const SESSION_PLACEMENT_VALUES = ['by_strategy', 'by_warm_prefix'] as const;
/** Where a fresh session with no pin lands: by the strategy, or where its static prefix is warm. */
export type SessionPlacement = (typeof SESSION_PLACEMENT_VALUES)[number];
export const SUBAGENT_PLACEMENT_VALUES = ['by_cohort', 'by_strategy', 'with_main'] as const;
/** Where a subagent with no pin of its own lands. */
export type SubagentPlacement = (typeof SUBAGENT_PLACEMENT_VALUES)[number];

export interface PoolMember {
  accountId: string;
  /** The pins under the pool's keys on this account, one per key, session, agent and model. */
  activePins: number;
  /** Null is no budget over that period; a member may carry one, both, or neither. */
  budgetDailyUsd: number | null;
  budgetMonthlyUsd: number | null;
  /** The router's end for a member at a budget: the latest end among the budgets reached, and `null` while none is. */
  budgetUntil: string | null;
  /**
   * The 7-day window's share the sessions on the member burn per hour, null on an API-key member or one with no 7-day
   * reading.
   */
  burn: number | null;
  eligible: MemberEligibility;
  /**
   * The share of the member's 7-day room Pace holds back for its expected Fable use, the reservation `paceScore`
   * subtracts. Null on an API-key member, a usage-based seat and a member with no Fable reading, and while the router
   * does not answer.
   */
  fableReservation: number | null;
  /** The factor the pool's slow-down computed for the account, null where the account route answers no `slowDown`. */
  factor: number | null;
  kind: AccountKind;
  /**
   * The router's Pace score for the member whatever its kind: the spare rate for an OAuth member, the remaining
   * worst bucket for an API-key member.
   */
  paceScore: number | null;
  priority: number;
  /** One entry for each regime, the base entry first. Null while the router does not answer. */
  regimeFits: RegimeFit[] | null;
  /** The speed the account's holds realized inside a slow-down span, from 0 to 1. Null outside a span and where
   *  `factor` is. */
  speed: number | null;
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

export const RETURN_TO_PLAN_VALUES = ['always', 'never', 'when_cold'] as const;
/** When a session on an API-key member returns to a subscription member: at once when cold and after the dwell when warm, only when cold, or never. */
export type ReturnToPlan = (typeof RETURN_TO_PLAN_VALUES)[number];

export const WARM_SESSIONS_VALUES = ['always', 'never', 'when_needed'] as const;
/** Whether a shedding round may mark a warm session: never, only once the cold ones fall short, or by burn alone. */
export type WarmSessions = (typeof WARM_SESSIONS_VALUES)[number];

/** One static check that holds on a pool's saved settings. A warning blocks nothing. */
export interface PoolWarning {
  code: string;
  /** The pool fields the check reads, as the route names them. */
  fields: string[];
  text: string;
}

/** A sum over both fundings, as a key's lifetime and week, a pool's day and the overview's day carry it. */
export interface FundedSum {
  /** The rows no subscription window paid, in dollars. */
  billedUsd: number;
  cost: number;
  /** The plan rows as the Pro-week figure, whatever tier served them, null where a plan row priced nothing. */
  equivalent: number | null;
}

export const POOL_STATUS_VALUES = ['active', 'degraded', 'disabled', 'exhausted'] as const;
export interface Pool {
  activePins: number;
  affinityMode: AffinityMode;
  /** Idle seconds before a session's pin to an account expires; 0 disables affinity. */
  affinityTtlSeconds: number;
  /** Whether the client's server-side auto mode checks travel; off drops them. */
  autoModeServer: boolean;
  /** Hours of captured request and response bodies for keys with no override; 0 is off. */
  bodyCaptureHours: number;
  /** The time constant of each session's burn average, which the projection and the shedding round read. */
  burnAverageSeconds: number;
  /** The pool's switch for the cache-diagnosis beta and `diagnostics.previous_message_id`; off by default. */
  cacheDiagnostics: boolean;
  /** Always answered; `enabled` off is how a pool that rewrites no breakpoint reads. */
  cacheTtl: PoolCacheTtl;
  /** The day's sum: the billed rows' dollars, every row's cost and the plan rows as the Pro-week figure, 0 with no
   *  plan-funded request. */
  day24h: FundedSum;
  /**
   * Whether a round also fires on a member whose 7-day room stands under its Fable reservation, on a Pace pool with
   * load shedding on. Such a round marks non-Fable sessions alone.
   */
  fableReservationShedding: boolean;
  /** The account each feature of the pool's keys acts under, null for none. A key may override it. */
  featureAccounts: FeatureAccountIds;
  /**
   * The share of the 5-hour window a move or a new session must stay under until the window resets, and the line a
   * shedding round tests the window against at the five-hour bound.
   */
  fiveHourCeiling: number;
  /** The pool's own guardrails, which run before a key's, ordered by `position`. */
  guardrails: GuardrailRef[];
  id: string;
  /** Requests in flight one member may hold before a fresh session skips it. */
  inFlightCap: number;
  keepalive: PoolKeepalive;
  /**
   * The ceilings the router resolves for the pool's recipes over its members' fundings, the main agent's TTLs and the
   * pool's models, in seconds, with the idle window's cost as a share of one re-write, and null for a pool with no
   * member.
   */
  keepaliveResolved: { breakEvenSeconds: Range; horizonSeconds: Range; idleSeconds: Range; idleShare: Range; parkedSeconds: Range } | null;
  keys: PoolKey[];
  /** The shedding round's switch. Off, no round runs. The spent move keeps `migrateWhenSpent`. */
  loadShedding: boolean;
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
  /** Order the selection set by account kind before priority; the operator's answer alone. */
  preferSubscription: boolean;
  requests24h: number;
  /** The longest throttle a request waits out on its own member before it fails over. */
  retryInPlaceSeconds: number;
  returnToPlan: ReturnToPlan;
  /** Under `always`, how long a warm session stays on the API-key member before it returns, 0 at once. */
  returnToPlanDwellSeconds: number;
  /** The row's edit token, which an edit sends back as `expected_revision`. A pool deleted and created again under
   * its id never answers one its predecessor did. */
  revision: number;
  sessionPlacement: SessionPlacement;
  /** The most of a member's burn one shedding round removes, as a share of it. */
  shedCap: number;
  /** Seconds a member that shed waits before its next round, and a session shed off it stays away. */
  shedDwellSeconds: number;
  /** Holds each request of a held class before it is forwarded, so a member whose window would pass the ceiling reaches its reset under it. */
  slowDown: boolean;
  /** The longest one request waits for the slow-down. */
  slowDownHoldSeconds: number;
  /** The time the slow-down may add to a session before a shedding round moves sessions off the member. */
  slowDownStretchSeconds: number;
  /** `active` and `disabled` are set by hand; the rest are observed. */
  status: (typeof POOL_STATUS_VALUES)[number];
  strategy: PoolStrategy;
  subagentPlacement: SubagentPlacement;
  /** The pool's switch for the thinking-binding beta, which the relay adds where thinking is on and the client sent
   *  none. Off by default. */
  thinkingDiagnostics: boolean;
  traffic24h: TrafficBucket[];
  /** The stamp of the row's last write, which `revision` carries. */
  updatedAt: string;
  warmSessions: WarmSessions;
  /** The static checks that hold on the saved settings, sorted by code. None blocks a save. */
  warnings: PoolWarning[];
  /** The same line for the 7-day window and each model week, read over the next five hours. */
  weeklyCeiling: number;
  /** Minutes before an account's subscription end from which it takes a fresh session only as the last resort. */
  windDownMinutes: number;
}

export type KeyStatus = 'active' | 'disabled' | 'expired' | 'revoked';

export const COMPANION_PERMISSION_VALUES = ['allowance', 'keepalive'] as const;
/** A companion route a key may use: the allowance tool's read, or the router warming its sessions' caches. */
export type CompanionPermission = (typeof COMPANION_PERMISSION_VALUES)[number];

/** `equivalent` is 0 with no plan-funded request. */
export interface KeyFigures extends FundedSum { requests: number }

export interface ApiKey {
  /** Null inherits the pool's `autoModeServer`; true or false overrides it for this key. */
  autoModeServer: boolean | null;
  /** Hours of captured bodies for this key. `null` inherits the pool's hours, and 0 turns capture off here. */
  bodyCaptureHours: number | null;
  /** The companion routes the key permits, in the order `allowance`, `keepalive`: both for a new key. */
  companionPermissions: CompanionPermission[];
  createdAt: string;
  /** The stored switch, which `status` folds expiry and revocation over; an edit sends it back as it stands. */
  enabled: boolean;
  expiresAt: string | null;
  /** The key's own value for each feature. */
  featureAccounts: Record<FeatureWord, KeyFeatureAccount>;
  /** The key's own guardrails, which run after its pool's, ordered by `position`. */
  guardrails: GuardrailRef[];
  id: string;
  lastUsedAt: string | null;
  lifetime: KeyFigures;
  /** Whether `GET /v1/models` under this key lists an `id[1m]` twin beside every long-context model. */
  longContextTwins: boolean;
  modelRules: ModelRule[];
  modelRulesMode: ModelRulesMode;
  name: string;
  notes: string | null;
  /** The pool's guardrails as the key inherits them, read-only here, ordered by `position`. */
  poolGuardrails: GuardrailRef[];
  poolId: string;
  prefix: string;
  /** When the secret the last rotation retired last authenticated, and `null` until it does or once rotation clears
   * it. */
  retiredSecretLastUsedAt: string | null;
  /** The row's edit token, which an edit sends back as `expected_revision`. A key deleted and created again under
   * its id never answers one its predecessor did. */
  revision: number;
  /** Live sessions whose newest request still presented the previous secret; 0 once the grace ends. */
  sessionsOnRetiredSecret: number;
  status: KeyStatus;
  /** The stamp of the row's last write, which `revision` carries. */
  updatedAt: string;
  /** The week's figures, read as `lifetime`'s. */
  usage7d: KeyFigures & { tokens: number };
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
export type AutoMode = (typeof AUTO_MODE_VALUES)[number];
export const AUTO_MODE_VALUES = ['body_shape', 'server', 'stripped'] as const;
/**
 * Who sent a token that asks the upstream for a report: `diagnostics.previous_message_id` on the body the upstream
 * compared, for the cache diagnostics, or the `thinking-binding-controls-2026-08-01` beta, for the dropped thinking
 * blocks.
 */
export type DiagnosticsSource = (typeof DIAGNOSTICS_SOURCE_VALUES)[number];
export const DIAGNOSTICS_SOURCE_VALUES = ['client', 'router'] as const;
export const REQUEST_KIND_VALUES = ['count_tokens', 'inference', 'probe'] as const;
export type RequestKind = (typeof REQUEST_KIND_VALUES)[number];
export type Transport = 'json' | 'sse';
export const ATTEMPT_OUTCOME_VALUES = ['auth_failed', 'canceled', 'rate_limited', 'succeeded', 'transport_error', 'upstream_error'] as const;
export type AttemptOutcome = (typeof ATTEMPT_OUTCOME_VALUES)[number];
export const REQUEST_OUTCOME_VALUES = ['canceled', 'exhausted', 'failed', 'incomplete', 'rejected', 'succeeded'] as const;
/** How a request ended, in the ledger's own words. */
export type RequestOutcome = (typeof REQUEST_OUTCOME_VALUES)[number];

export interface AttemptWindow {
  id: WindowId;
  resetsAt: string | null;
  /** Null when the headers carried the window's reset and no utilization. */
  utilization: number | null;
}

/**
 * Why the prompt-cache prefix did not hit, as the cache diagnostics beta named it. The router stores whatever
 * name the beta sends, so a name added after the console's ten (`CACHE_MISS_REASONS`) is kept as sent.
 */
export type CacheMissReason = string;

/**
 * The thinking blocks an answer dropped from the request's history, or let through, per `<type>.<reason>` key in the
 * upstream's two words (`other` for a word past 64 bytes): the blocks under the key and the lowest index `i` of their
 * `messages.{i}` paths. The keys are data, so a key the console does not know is kept as sent.
 */
export type InputTransformations = Record<string, { blocks: number; firstMessage: number }>;

/** What the answer said about itself; `servedModel` only when it differs from the resolved model. */
export interface AttemptResponse {
  clearedInputTokens: number | null;
  inferenceGeo: string | null;
  /**
   * An empty map for an answer that listed nothing, and null for an answer without the member and for an attempt the
   * router recorded before it kept it.
   */
  inputTransformations: InputTransformations | null;
  refusalCategory: string | null;
  refusalRecommendedModel: string | null;
  servedModel: string | null;
  speed: string | null;
  stopReason: string | null;
  stopSequence: string | null;
}

/** The stop reasons the console names, as the API spells them. The router stores whatever word the answer carried. */
export const STOP_REASONS = ['end_turn', 'max_tokens', 'model_context_window_exceeded', 'pause_turn', 'refusal', 'stop_sequence', 'tool_use'] as const;
/** The refusal categories the console names. The router stores whatever word the answer carried. */
export const REFUSAL_CATEGORIES = ['cyber'] as const;

/** A candidate the selection passed over, with the obstacle in its way as the router named it. */
export interface SkippedCandidate {
  accountId: string;
  obstacle: string;
}

export const PLACEMENT_RUNG_VALUES = ['content', 'static', 'with_main'] as const;
/**
 * Where a fresh placement's account came from: `content` the deepest warm message boundary, `static` the deepest
 * warm static boundary, `with_main` the parent agent's pin or another agent's pin of the session.
 */
export interface Placement {
  /** The matched boundary's depth; 0 for `with_main`. */
  depth: number;
  /** The prefix the entry promised, in tokens, which a hit reads up to; 0 for `with_main`. */
  prefixTokens: number;
  rung: (typeof PLACEMENT_RUNG_VALUES)[number];
}

/** The holds a request waited through before this attempt was placed. */
export interface CohortHold {
  /** The cohort's account the last hold was placed by, and `null` for a hold on the strategy's choice. */
  cohort: string | null;
  /** The holds' waits summed, in milliseconds, whether a slot freed or the bound ran out. */
  heldMs: number;
  waitedOn: string[];
  /** Each wait with its reason (`cap`, `reset`, `slow_down`, `rewrite`), oldest first, and none on a row from before the member. */
  waits: { heldMs: number; reason: string }[];
}

/** Why a selection landed where it did, as the router stored it on the attempt. */
export interface SelectionTrace {
  /** The cache-read tokens the response reported, folded in at completion, and `null` until then or without a
   * response. */
  cacheRead: number | null;
  cohortHold: CohortHold | null;
  /** The move reason when a failover, a rule or the state of the pin's account placed the attempt, null otherwise. */
  moved: string | null;
  /** Null for a renewal or a choice the strategy made. */
  placement: Placement | null;
  renewed: boolean;
  skipped: SkippedCandidate[];
  strategy: string;
  /** Group, priority, and the overage bit the choice was made in, and `null` for a renewed pin. */
  tier: [number, number, number] | null;
}

export interface Attempt {
  accountId: string;
  accountKind: AccountKind;
  /**
   * Null for an attempt whose answer held no usage report and for one recorded before the router filed its own words,
   * `not_requested` and `not_returned`.
   */
  cacheMissReason: CacheMissReason | null;
  cooldownSet: { reason: string; until: string } | null;
  durationMs: number | null;
  /**
   * Whether the pin's cache entry was past its lifetime when the selection read it, a probe's refresh counted. Null
   * where the selection handed no size on and on an attempt stored before the router wrote the fact.
   */
  entryExpired: boolean | null;
  index: number;
  /** The digest of the credit token a refused attempt issued, null elsewhere. */
  issuedDigest: string | null;
  outcome: AttemptOutcome;
  /** The head's overage flag as the attempt stored it: null on an API-key attempt and on a head that reported none. */
  overageInUse: boolean | null;
  /**
   * The size (input, read and both writes) of the pin's last usage report as the selection handed it on. Null where
   * the attempt lands off the account of that report (a move, a later leg of a failover), on a turn the router read
   * as compacted, as a changed lane or as a static restart, on a pin with no usage report yet, and on an attempt
   * stored before the column.
   */
  previousInputTokens: number | null;
  response: AttemptResponse;
  /** Null for an attempt the router stored no trace for. */
  selection: SelectionTrace | null;
  /** The answer's `service_tier`, null on an answer that carried none. */
  serviceTier: string | null;
  startedAt: string;
  /** The account tier the attempt's account was on when it landed, null for an API-key account and an unknown tier. */
  tier: string | null;
  /** The `id` of the message the answer carried, the join to a transcript's assistant entry, and `null` without one. */
  upstreamMessageId: string | null;
  upstreamRequestId: string | null;
  upstreamStatus: number | null;
  /** The serving account's class at the attempt, false on an attempt from before the router stored it. */
  usageBased: boolean;
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

/** What the request asked for, read off its body, and `null` where the body carried no such member. */
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

/** The instants a request passed through, in the order it passed them. A phase the ledger holds no instant for
 *  reads null: a row from before the two columns on `writtenAt` and `streamEndAt`, a request with no attempt on
 *  `selectedAt`, `writtenAt` and `headAt`, a failed stream on `firstContentAt` and `streamEndAt`, and a JSON
 *  answer on `streamEndAt`, since nothing streamed. */
export interface RequestPhases {
  finishedAt: string | null;
  firstContentAt: string | null;
  headAt: string | null;
  receivedAt: string;
  selectedAt: string | null;
  streamEndAt: string | null;
  writtenAt: string | null;
}

/** A plan-funded request's shares of its account's windows, keyed by the ledger's window ids, every window the model
 *  applies to a key and null on one the cost model could not price, a shared window the account's window set lacks
 *  left out. Null for a request billed in dollars, and an empty object for one whose account the route's tiers lack or
 *  hold without a tier. */
export type CostShares = Record<string, number | null>;

export const CLASSIFIER_VALUES = ['client', 'client_off'] as const;

export interface RequestLog {
  accountId: string | null;
  agentId: string | null;
  /** `x-claude-code-agent-type` as the client sent it, and `null` when it sent none. */
  agentType: string | null;
  at: string;
  attempts: Attempt[];
  autoMode: AutoMode | null;
  /** Whether the body carried a cache breakpoint or the pool's TTL policy added one, the fact the decision record holds
   *  as `flags.marker`. Null on a row stored before the router kept it. */
  cacheMarker: boolean | null;
  /** The model the router resolved the request to, which a pin and its events are keyed by. */
  canonicalModel: string;
  capturedBodies: boolean;
  /** `client` for the client's own classifier call on the account and incarnation its judged turn ended on,
   * `client_off` where it ran off them or in a session with no turn to judge, and `null` for every other request. */
  classifier: (typeof CLASSIFIER_VALUES)[number] | null;
  /** The class the router derived at admission: `claude-code/<entrypoint>` or `other`, and `null` on a row from before
   * the router recorded it. */
  client: string | null;
  /** The item a `client_contract` refusal named, and `null` for every other request. */
  clientContract: string | null;
  /** The kind a client-drift refusal answered with (`hash` or `version_suffix`), and `null` for every other request. */
  clientDrift: string | null;
  compaction: string | null;
  contextCompacted: string | null;
  /**
   * The serving attempt's usage as the share of the Pro 7-day window, whatever account tier served it: the Pro-week
   * figure. Null for a billed request and for a plan-funded request on a model without a weight.
   */
  costEquivalent: number | null;
  costShares: CostShares | null;
  costUsd: number;
  /** Who put `diagnostics.previous_message_id` on the serving leg's body, `null` for a body that went without it. */
  diagnosticsSource: DiagnosticsSource | null;
  durationMs: number | null;
  errorCode: string | null;
  /** The digest of the fallback credit token the request presented, null where it presented none. */
  fallbackDigest: string | null;
  /** A request whose session branched from another: the depth where the two part, and the session it branched from. */
  forkDepth: number | null;
  forkedFrom: string | null;
  /** The guardrail that refused the request, null where none did. */
  guardrailId: string | null;
  /** Every guardrail that matched, in the order they ran, the one that blocked marked. */
  guardrails: GuardrailMatch[];
  /** The holds the request waited through, with the wait, and `null` for a request never held. */
  held: CohortHold | null;
  id: string;
  /** The `created_at` of the key the request was read under, 0 for none; a key id created again is another holder. */
  keyGeneration: number;
  keyId: string | null;
  kind: RequestKind;
  model: string;
  /** How the request ended, once every attempt is in. */
  outcome: RequestOutcome;
  phases: RequestPhases;
  poolId: string | null;
  /** The sum of the client's `x-claude-code-prev-tool-durations`, in milliseconds. */
  prevToolMs: number | null;
  /** The router refused the request before any account was asked, so the figures leave it out and the logs list it. */
  refused: boolean;
  /** `x-claude-code-request-class` as the client sent it, and `null` when it sent none. */
  requestClass: string | null;
  responseTruncated: boolean;
  /** The client's own session, the one its companion registers under. */
  sessionId: string | null;
  shape: RequestShape;
  /** `client_classifier` where the shares read zero by rule, and `null` for a figure priced from the counts. */
  shareReason: 'client_classifier' | null;
  status: number | null;
  /**
   * The serving attempt's two counts as the route stored them. A null is a count the serving attempt never stored: a
   * token count's, a request's whose serving attempt answered no 200, and one that no attempt served. `usage` reads 0
   * there, so every sum over it keeps a number.
   */
  storedTokens: { input: number | null; output: number | null };
  /**
   * Who put `thinking-binding-controls-2026-08-01` on the request: `client` where the caller's `anthropic-beta` list
   * carried it, `router` where the relay added it under the pool's `thinking_diagnostics` switch, and `null` for a
   * request that went without it and on a row stored before the router kept the sender.
   */
  thinkingDiagnosticsSource: DiagnosticsSource | null;
  /**
   * The serving attempt's account tier, null for an API-key account, a request no attempt served and an unknown tier.
   */
  tier: string | null;
  transport: Transport;
  ttfbMs: number | null;
  /** First content token; later than `ttfbMs` on streams that open with metadata. */
  ttftMs: number | null;
  /** `cc_turn_origin` off the attribution line; first-party only. */
  turnOrigin: string | null;
  usage: Usage;
  /** The client that sent the request, as it named itself, and `null` when it named nothing. */
  userAgent: string | null;
}

/** The bytes each side stored and the bytes it ran to, as the ledger counted them; `seen - stored` is what a cut dropped. */
export interface CapturedBodies {
  request: unknown;
  requestBytesSeen: number;
  requestId: string;
  response: unknown;
  responseBytesSeen: number;
  responseBytesStored: number;
  responseTruncated: boolean;
}

/** The move reasons a rule or a failover writes on a `pin_migrated` event and a trace's `moved`. A move an account's
 *  state forced carries a cause word in their place, one of `PIN_RELEASE_REASON_VALUES`, beside the event's
 *  `reasons`. */
export const PIN_MIGRATION_REASON_VALUES = ['cooldown', 'drain', 'fast_lane', 'mark', 'overage', 'return_to_plan', 'shed', 'spent'] as const;
/** Why the router moved a session's pin: the reason string of a `pin_migrated` event. */
export type PinMigrationReason = (typeof PIN_MIGRATION_REASON_VALUES)[number];

/**
 * The reasons a `pin_expired` event carries, which are also the `reasons` of a move an account's state forced and the
 * word for its cause. The drawer words any other name as sent.
 */
export const PIN_RELEASE_REASON_VALUES = [
  'attempted', 'budget', 'budget_unknown', 'client_error', 'cooldown', 'deleted', 'disconnected', 'expired', 'fast_credits', 'fast_rate_limit', 'funding_required', 'overage', 'paused', 'reauth_required', 'released', 'spent', 'subscription_ended', 'unpaired', 'unserved',
] as const;
/** Why the router released a session's pin: the `reasons` of a `pin_expired` event, or of the `pin_migrated` event of a
 * move an account's state forced, in the order the selection found them. */
export type PinReleaseReason = (typeof PIN_RELEASE_REASON_VALUES)[number];

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

/** The newest stream interruption of the request, from `stream_cut`. Null where none was recorded. */
export interface StreamCut {
  at: string;
  attempt: number;
  bytesRelayed: number;
  /** `account_canceled`, `stream_cut`, `stream_stalled` or `stream_undecodable`, the request's error code word. */
  code: string;
  text: string;
  upstreamRequestId: string | null;
}

/** `GET /admin/requests/{id}`: the row and the events of its window, from one read. */
export interface RequestDetail {
  events: RequestEvent[];
  request: RequestLog;
  streamCut: StreamCut | null;
}

/** The route's own list of the request list's keys, its default first. */
export const REQUEST_ORDER_VALUES = ['received_at', 'attempts', 'duration_ms', 'ttfb_ms', 'input_tokens', 'output_tokens'] as const;
export type RequestOrder = (typeof REQUEST_ORDER_VALUES)[number];
export type SortDir = 'asc' | 'desc';

export interface RequestQuery {
  accountIds?: string[];
  /** The requests whose agent id equals the value, `main` or a subagent's id. */
  agentId?: string;
  agentTypes?: string[];
  /** The serving attempt's cache miss reason, any name the beta sent. */
  cacheMissReasons?: string[];
  /** `client`, `client_off`, `-`: the classifier standing. */
  classifiers?: string[];
  /** A pattern over the client class, the grammar of `userAgent`. */
  client?: string;
  cursor?: string;
  /** The key's direction, `desc` the route's default. */
  dir?: SortDir;
  /** The effort as the rollups fold it: a level, `none` or `other`. */
  efforts?: string[];
  /** `true` keeps the rows that presented a fallback digest, `false` the rest. */
  fallback?: boolean;
  /** The range holds its own end, so `landedThrough` is left uncapped at `to` and a late row is read once. */
  fixedEnd?: boolean;
  from?: string;
  /** Guardrail ids, `-` for a request no guardrail matched. */
  guardrails?: string[];
  /** Keep the requests that waited for a slot. */
  held?: boolean;
  includeProbes?: boolean;
  /** Keys of the thinking blocks the serving attempt's answer dropped, any key the upstream sent. */
  inputTransformations?: string[];
  keyIds?: string[];
  kind?: RequestKind | null;
  /** Unix milliseconds: the rows the ledger wrote after this instant, the `landedThrough` of a poll's last answer. */
  landedAfter?: number;
  limit?: number;
  models?: string[];
  /** The list's key, `received_at` the route's default. */
  order?: RequestOrder;
  outcomes?: RequestOutcome[];
  poolIds?: string[];
  /** The serving attempt's refusal category, `-` for none. */
  refusalCategories?: string[];
  /** The one request whose id equals the value. */
  request?: string;
  /** The class as the rollups fold it: the class, `-` or `other`. */
  requestClasses?: string[];
  search?: string;
  /** The requests whose session id equals the value. */
  session?: string;
  /** `fast`, `standard`: the serving attempt's speed, else `fast` for a request that asked for it. */
  speeds?: string[];
  statuses?: number[];
  /** The serving attempt's stop reason, `-` for a request none served or whose attempt carries none. */
  stopReasons?: string[];
  to?: string;
  turnOrigins?: string[];
  /** A pattern: `*` any run, `?` one character, a backslash quotes either. An empty string is no filter at all. */
  userAgent?: string;
}

export interface RequestPage {
  /**
   * The newest write instant among the rows the query's filters admit at the read, `to` included, the `landedAfter` it
   * passed where none above it has landed, and 0 where neither is set, never past a `to` the query names unless the
   * query passes `fixedEnd`, since a row received after `to` lands after it. Under `fixedEnd` the stamp may pass `to`,
   * so a row received before the end and landed after it is read once.
   */
  landedThrough: number;
  nextCursor: string | null;
  requests: RequestLog[];
}

export const FEATURE_OUTCOME_VALUES = ['canceled', 'failed', 'forwarded'] as const;
/** How a feature call ended, in the ledger's own words. */
export type FeatureOutcome = (typeof FEATURE_OUTCOME_VALUES)[number];

/** One call the router sent to a feature host under an account. */
export interface FeatureCall {
  accountId: string;
  durationMs: number;
  /** The router's code where it answered a forward that could not complete. */
  errorCode: string | null;
  feature: FeatureWord;
  id: string;
  keyId: string;
  method: string;
  outcome: FeatureOutcome;
  /** The path without its query. */
  path: string;
  receivedAt: string;
  /** Null where no upstream head arrived. */
  status: number | null;
}

export interface FeatureCallPage {
  featureCalls: FeatureCall[];
  /** Set only while a row lies beyond the page. */
  nextCursor: string | null;
}

/** Each list narrows to the rows whose value it holds, and both ends of the range hold their own instant. */
export interface FeatureCallQuery {
  accounts?: string[];
  cursor?: string;
  features?: FeatureWord[];
  /** Unix milliseconds. */
  from?: number;
  keys?: string[];
  outcomes?: FeatureOutcome[];
  /** Unix milliseconds. */
  to?: number;
}

export const DECISION_KIND_VALUES = ['exhausted', 'held', 'moved', 'placed', 'refused', 'renewed'] as const;
/** A decision's kind as the routing log names it. A probe's decision keeps its kind and the row's `probe` marks it. */
export type DecisionKind = (typeof DECISION_KIND_VALUES)[number];

export const PIN_REASON_VALUES = ['classifier', 'not_looked_up', 'pinning_off'] as const;
/** Why a record pins nothing. `classifier` is a classifier call's, which holds no pin and follows the turn it
 *  judges. `not_looked_up` is a refusal's, made before the pin lookup whatever session it named. */
export type PinReason = (typeof PIN_REASON_VALUES)[number];

export const FACT_UNIT_VALUES = ['count', 'ms', 'percent', 'percent_per_hour', 'text', 'time', 'tokens', 'usd'] as const;
/** How the console draws a fact's value. */
export type FactUnit = (typeof FACT_UNIT_VALUES)[number];

export const FACT_ENTITY_VALUES = ['account', 'key', 'model', 'pool'] as const;
export interface FactEntity {
  id: string;
  kind: (typeof FACT_ENTITY_VALUES)[number];
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

export const BY_KIND_VALUES = ['exhaustion', 'hold', 'none', 'refusal', 'rule', 'rung', 'strategy'] as const;
/** What decided the row, the By chip: a rule, a rung, the strategy, a hold, an exhaustion, a refusal or none. */
export interface DecisionBy {
  kind: (typeof BY_KIND_VALUES)[number];
  tier: [number, number, number] | null;
  value: string;
}

export interface Score {
  accountId: string;
  score: number;
  unit: string;
}

export interface Exhaustion { deadline: string | null; kind: string; obstacle: string | null; reason: string | null; status: number | null; window: string | null }
/** The refusal's code (a key refusal's `revoked` and its siblings, a client's `drift` or `contract`) beside its
 *  sentence. */
export interface Refusal { code: string; kind: string; reason: string }

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
  exhaustion: Exhaustion | null;
  /** One pair, or two at the most. */
  fact: SummaryFact[];
  fast: boolean;
  fromAccountId: string | null;
  hold: RecordHold | null;
  keyId: string | null;
  kind: DecisionKind;
  model: string | null;
  poolId: string | null;
  probe: boolean;
  refusal: Refusal | null;
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
  /**
   * The rows whose agent id equals the value: a decision's by its request, an event's own, so an event that names
   * no agent is none.
   */
  agentId?: string;
  cursor?: string;
  /** The time's direction, `desc` the route's default. */
  dir?: SortDir;
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
  /** The relay's estimate of the body's prompt tokens, a fresh session's rewrite, null on a refusal. */
  bodyTokens: number | null;
  canonicalModel: string;
  chain: { boundaries: number; staticBoundaries: number; staticHash: boolean };
  clientClass: string;
  flags: { contextCompacted: boolean; count: boolean; fast: boolean; marker: boolean; portable: boolean; probe: boolean };
  /**
   * The holds this request waited through before the decision, each with the wire attempt that waited. `ranOut` is
   * true on the hold whose bound the relay answered at.
   */
  holds: { accountId: string; attempt: number; cohort: string | null; heldMs: number; ranOut: boolean }[];
  keyGeneration: number;
  keyId: string;
  lane: { effort: string; speed: string; thinking: string };
  parentAgentId: string | null;
  poolId: string;
  registryRevision: number;
  requestClass: string | null;
  requestId: string;
  seq: number;
  /** Null on a refusal of a request that sent no session header. */
  sessionId: string | null;
}

/** The no-return stamp a shed move or a failover's landing leaves on a pin. */
export interface ShedStamp {
  at: string;
  from: string;
  reason: string;
}

/** Step 1's reading of the pin, its row's fields null when none was found. */
export interface RecordPin {
  accountId: string | null;
  createdAt: string | null;
  /** The pin's cycle the slow-down sizes its hold from, null without one or on a record from before the member. */
  cycleMs: number | null;
  epoch: number | null;
  /** `live`, `expired_idle`, `left_pool` or `none`. */
  found: string;
  idleMs: number | null;
  incarnation: number | null;
  lane: string | null;
  lastRefreshAt: string | null;
  lastUsedAt: string | null;
  marked: boolean;
  /** When a shedding round marked the pin, null while it carries no mark. */
  markedAt: string | null;
  /** The account the round marked the pin on. */
  markedOn: string | null;
  /** The plan window whose projection fired the round. */
  markedWindow: string | null;
  refreshAgeMs: number | null;
  released: string[];
  /**
   * The source a shed move or a failover's landing left the pin with. `reason` is `round` for a shedding round's move
   * and the failover's word (`cooldown`, `fast_lane`, `mark`) for a landing, and `round` on a record from before the
   * member.
   */
  shed: ShedStamp | null;
  /** The slow-down deadline standing on the pin, null without one or on a record from before the member. */
  slowDownUntil: string | null;
  warmth: string | null;
  warmthCause: string | null;
}

/**
 * The twenty-seven values that entered the decision, each named as the pool row names the field it was read from. A
 * version 1 record read the cap, the average and the dwell under their old names and carried neither the switch nor a
 * ceiling. The slow-down's three and the return-to-plan's two read null on a record from before them.
 */
export interface RecordSettings {
  affinityMode: string;
  affinityTtlSeconds: number;
  burnAverageSeconds: number;
  /** Whether the pool sheds on its members' Fable reservation. Null on a record stored before the router wrote it. */
  fableReservationShedding: boolean | null;
  /** Null on a version 1 record, which carried no ceiling. */
  fiveHourCeiling: number | null;
  inFlightCap: number;
  /** A version 1 record's three removed settings, null on a record from version 2 on. */
  legacy: { paceFloor: number; selectBelowUtilization: number | null; shedAheadOfPace: number | null } | null;
  /** Null on a version 1 record, which carried no switch. */
  loadShedding: boolean | null;
  migrateWhenSpent: boolean;
  minSharedPrefixTokens: number;
  overageMoveGraceMinutes: number;
  overagePolicy: string;
  poolUpdatedAt: string;
  preferSubscription: boolean;
  retryInPlaceSeconds: number;
  returnToPlan: string | null;
  returnToPlanDwellSeconds: number | null;
  sessionPlacement: string;
  shedCap: number;
  shedDwellSeconds: number;
  slowDown: boolean | null;
  slowDownHoldSeconds: number | null;
  slowDownStretchSeconds: number | null;
  strategy: string;
  subagentPlacement: string;
  warmSessions: string;
  /** Null on a version 1 record, which carried no ceiling. */
  weeklyCeiling: number | null;
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
  /** The plan window whose runway is the candidate's, null where the gate read none. */
  binding: string | null;
  /** The share of each plan window the sessions on the account burn per hour, empty on a version 1 record. */
  burn: Record<string, number>;
  cooldowns: RecordCooldown[];
  credits: RecordCredits | null;
  enabled: boolean;
  /** The account's slow-down factor from its entry in force, null with none or on a record from before the member. */
  factor: number | null;
  /**
   * `fits`, `rewrite_only`, `least_bad`, `ending` or `spent` where the gate or the no-fit partition read the candidate.
   * `spent` stands on a placement's member that has no allowance left.
   */
  fit: string | null;
  inFlight: number;
  inFlightCap: number;
  inOverage: boolean;
  incarnation: number;
  kind: AccountKind;
  obstacle: { name: string; reason: string | null; scope: string | null; until: string | null; window: string | null } | null;
  overage: { observedAt: string; reset: string | null; since: string; source: string } | null;
  /**
   * `score` is the spare rate: the remaining share less `fableReservation` per hour, less `burn`, the burn on `window`
   * of the pins the reservation does not stand for. `burn` is null on a version 1 record, which read none.
   */
  pace: {
    burn: number | null;
    deadline: string | null;
    end: string | null;
    /**
     * The share of the member's 7-day room the score holds back for its expected Fable use, null where none applies.
     */
    fableReservation: number | null;
    hours: number | null;
    remaining: number;
    reset: string | null;
    score: number;
    window: string | null;
  } | null;
  partition: string | null;
  planTier: string | null;
  /**
   * Per rung the deepest visited entry and, apart from it, the one the rung accepted on this member. The card's chips
   * read the match.
   */
  prefix: { content: PrefixEntry | null; contentMatch: PrefixEntry | null; static: PrefixEntry | null; staticMatch: PrefixEntry | null; withMain: boolean };
  priority: number;
  rank: number | null;
  /** Until the window that binds reaches the ceiling with the pin's rewrite and burn, null for an unbounded runway. */
  runwayMs: number | null;
  score: number | null;
  state: string;
  term: { canceled: boolean; ended: boolean; endsAt: string | null; since: string | null; trialEndsAt: string | null; windingDown: boolean } | null;
  tier: [number, number, number];
  verdict: string;
  /**
   * `projection` and `horizon` are null where nothing is projected, and `reserved` sums the live reservations, null on
   * a version 1 record, which booked none. `demand` is the 5-hour window's unpaced burn per hour on an OAuth candidate,
   * null elsewhere and on a record from before the member.
   */
  windows: Record<string, { ageMs: number; ahead: number | null; demand: number | null; horizon: string | null; observedAt: string; projection: number | null; reserved: number | null; reset: string | null; source: string; utilization: number }>;
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
  /** The completing attempt's two cache write counts added, null until the attempt has usage. */
  cacheWriteTokens: number | null;
  completedAt: string | null;
  freed: boolean;
  heldMs: number | null;
  hit: boolean | null;
  inputTokens: number | null;
  /**
   * The completing attempt's dropped thinking blocks: an empty map for an answer that listed nothing, and null until
   * the answer ends, for an answer without the member and for an attempt the router recorded before it kept it.
   */
  inputTransformations: InputTransformations | null;
  promisedTokens: number | null;
  status: number | null;
}

/**
 * A hold: `cap` waits for a slot, `reset` for the spent window's reset at `until`, `slow_down` for the pin's deadline at
 * `until`, `rewrite` for the head of the request that carried a move, `waitsFor`, bounded at `until`. The slow-down's
 * figures are set under `slow_down` alone and read null on a record from before them.
 */
export interface RecordHold {
  accountId: string;
  boundMs: number;
  cohort: string | null;
  /** The pin's cycle the hold was sized from. */
  cycleMs: number | null;
  /** The factor the hold was sized from: the account's factor pulled toward 1 by the ramp, the shed line's `ramp`. */
  factor: number | null;
  reason: string;
  /** The speed the account's holds realize together. */
  speed: number | null;
  stretchMs: number | null;
  thresholdMs: number | null;
  until: string | null;
  /** The request whose head the hold waited for, under `rewrite`. */
  waitsFor: string | null;
}

/**
 * Why a failover moved its session: a cooldown's `scope`, `reason` and `window`, or a mark's account `state` and its
 * `cause`. The members the other shape carries read null.
 */
export interface MoveCause {
  cause: string | null;
  reason: string | null;
  scope: string | null;
  state: string | null;
  window: string | null;
}

export interface RecordOutcome {
  accountId: string | null;
  /** Set on a failover's landing, null for a rule's move and on a record from before the member. */
  cause: MoveCause | null;
  cohortHold: CohortHold | null;
  completion: RecordCompletion | null;
  exhaustion: Exhaustion | null;
  fromAccountId: string | null;
  hold: RecordHold | null;
  inOverage: boolean;
  kind: DecisionKind;
  /**
   * The shed mark this decision cleared, null when it cleared none. `projection` is the reading that healed it,
   * filled on `healed` alone and null on `released` and `moved`.
   */
  markCleared: { markedAt: string; markedOn: string; markedWindow: string; projection: number | null; reason: string } | null;
  /** In the routing task's emit order: two for a replaced expired pin, `pin_expired` then `pin_created`. */
  pinEvents: { kind: string; reason: string | null }[];
  pinWritten: { action: string; epoch: number; hashesStamped: boolean; lane: string; refreshStamped: boolean } | null;
  refusal: Refusal | null;
  /** What the decision booked on the account until the usage replaces it: the rewrite and the burn per hour by plan window. */
  reservation: { accountId: string; burn: Record<string, number>; expiresAt: string; rewrite: Record<string, number> } | null;
  rule: string | null;
  rung: Placement | null;
  /** The pins the shedding round this decision ran marked, or null when no round ran. */
  shedMarks: { agentId: string; keyId: string; model: string; sessionId: string }[] | null;
  strategy: string;
  tier: [number, number, number] | null;
  upstreamModel: string | null;
}

/**
 * A record as the record route answers it, every block in camelCase: version 2 or later, or version 1 for a decision
 * made before the projection, whose settings read the old names and whose walk carries the cool reading. `raw` is the
 * route's answer as it arrived, kept for the card's Copy as JSON, and `settings` null for a refusal under a key or pool
 * the registry no longer held.
 */
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

export type AnalyticsGroupBy = 'account' | 'all' | 'classifier' | 'client' | 'effort' | 'key' | 'kind' | 'model' | 'pool' | 'request_class' | 'speed' | 'user_agent';

/** The rows a read keeps by who paid for them: `billed` the API key, fast, overage and usage-based rows, `plan` the
 *  plan-funded rows. */
export type FundingFilter = 'billed' | 'plan';

export interface AnalyticsQuery {
  accountIds?: string[];
  /** Patterns, one per line; a row matches when any matches its client class in full. */
  clients?: string[];
  from: string;
  /** Absent keeps every row, and the two values partition them. */
  funding?: FundingFilter;
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
  /** Hits over placements, and `null` for a rung with no placement. */
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
  /** The next UTC day or month boundary. Like `spentUsd`, it reads `null` until the ledger reports. */
  periodEnd: string | null;
  poolId: string;
  spentUsd: number | null;
}

/** One group of one bucket or of the whole range, as the analytics routes sum it. */
export interface AnalyticsGroup {
  attempts: number;
  /** The dollars of the billed rows alone by token kind, the cache halves null on a fold as `cost`'s are. */
  billed: AnalyticsGroup['cost'];
  billedUsd: number;
  /** The plan rows' cache reads and writes in the Pro-week figure, null as `costEquivalent` is and on a fold, whose
   *  rows carry one total. */
  cacheEquivalent: { read: number; write: number } | null;
  cacheHits: number;
  cacheMisses: number;
  /**
   * Per reason, the requests under it whose serving attempt read `REASON_HIT_PERCENT` or more of its prompt from the
   * cache. A reason with none is no key, and an hour folded before the route counted them reads empty.
   */
  cacheMissReasonHits: Record<CacheMissReason, number>;
  /** The ten known reasons always, beside any other name the route answered. */
  cacheMissReasons: Record<CacheMissReason, number>;
  /** The requests a client left before their answer ended, counted in `requests` and in neither `failed` nor `errors`,
   *  since a client that hangs up is no fault of the route. */
  clientCanceled: number;
  /** The cache read and write costs are null on a group folded from request rows, which carry one total. */
  cost: Omit<CostSplit, 'cacheRead' | 'cacheWrite'> & { cacheRead: number | null; cacheWrite: number | null };
  /**
   * The plan rows as the Pro-week figure, whatever tier served them, under every grouping: null where a plan-funded
   * row with tokens priced nothing, a model without a weight, and 0 for a group with no plan-funded row.
   */
  costEquivalent: number | null;
  /**
   * Keyed by the ledger's window ids. Null for an api_key group, under a grouping other than account unless the
   * query names exactly one account, and on a group folded from request rows with no plan-funded row or with one
   * whose account the route could not price. A window is absent where the group's models do not apply to it, and a
   * shared window where the account's window set lacks it. The route answers a window a row could not be priced on as
   * absent too, while a fold of the request rows carries such a window as a key with null, so a reader treats the two
   * alike.
   */
  costShares: Record<string, number | null> | null;
  costUsd: number;
  /** The plan rows' Pro-week figure by token kind, summing to `costEquivalent`, null where a row priced nothing and on
   *  a fold, which carries one total. */
  equivalentSplit: CostSplit | null;
  errors: Record<string, number>;
  exhausted: number;
  expiredBetweenTurns: number;
  failed: number;
  failovers: number;
  /**
   * Set on a group folded from request rows, the session view's source: the rows carry no cost split and no
   * kept-warm mark, so those figures are null and the probe figures are not measured.
   */
  fromRequests?: true;
  histograms: LatencyHistograms;
  incomplete: number;
  /**
   * The requests per key of the blocks their serving attempt's answer dropped, a request counted once under each key it
   * listed whatever its blocks. Empty for an hour the router folded before it counted the blocks.
   */
  inputTransformations: Record<string, number>;
  /** The moved requests whose serving attempt's answer dropped a block under one of the two account reasons. */
  inputTransformationsMoved: number;
  /** The requests whose serving attempt's answer listed no dropped block. */
  inputTransformationsNone: number;
  /** The answered requests whose answer carried the dropped-blocks member, the base of the card's shares. 0 for an hour
   *  whose requests the ledger no longer held when the router first kept the count. */
  inputTransformationsReported: number;
  /**
   * The kept-warm requests and the tokens they read warm, booked per TTL, each with its plan-funded part: the credits
   * whose request a plan account served.
   */
  keptWarm: { planRequests: number; planTokens1h: number; planTokens5m: number; requests: number; tokens1h: number; tokens5m: number } | null;
  key: string;
  /**
   * An earlier holder's creation in Unix milliseconds under the key grouping, 0 for one with no creation time. Null
   * for the live key, for every other dimension and for a group folded from request rows.
   */
  keyCreatedAt: number | null;
  /** The key's id for a key group the route answered, so the label is built in the viewer's zone, and `null`
   * otherwise. */
  keyId: string | null;
  label: string;
  percentiles: Record<keyof LatencyHistograms, { p50: number | null; p90: number | null; p95: number | null; p99: number | null }>;
  /**
   * The plan-funded requests and output tokens, which the routes and a fold of request rows answer under every
   * grouping. The output count is the one plan token count a group carries, so `planRowsRequired` reads it.
   */
  planOutputTokens: number | null;
  planRequests: number | null;
  probeCostUsd: number;
  /** The plan-funded probes and their wasted part in the Pro-week figure, 0 without one, null where a plan probe row
   *  priced nothing and on a fold, which measures no probe. */
  probeEquivalent: { cost: number; wasted: number } | null;
  /** `planOk` is the part of `ok` a plan account served. */
  probes: { failed: number; held: number; ok: number; planOk: number; skipped: number };
  rateLimited: number;
  /** The refused requests of the group by error code, each counted here and in `refused`, never in `requests`. */
  refusals: Record<string, number>;
  refused: number;
  rejected: number;
  requests: number;
  /** The kept-warm saving in the Pro-week figure, 0 without one, null where a kept-warm count priced nothing and on a
   *  fold, which carries no kept-warm fact. */
  savingEquivalent: number | null;
  /** The kept-warm requests' saving in dollars, gross of the probes: the writes they avoided less the reads they
   *  paid. */
  savingUsd: number;
  succeeded: number;
  /** The routes answer every key but `plainOutput`, which a view derives where it splits the output into its
   * thinking share and the rest. */
  tokens: { cacheRead: number; cacheWrite1h: number; cacheWrite5m: number; output: number; plainOutput?: number; thinking: number; uncachedInput: number; webFetch: number; webSearch: number };
  /**
   * The uncached requests that could never read a cache, a part of `uncached`: the token counts, and the requests whose
   * body marked no cache breakpoint. The second part reads 0 for the hours the router folded before it kept the marker.
   */
  uncacheable: number;
  uncached: number;
  wastedProbeReasons: Record<WastedProbeReason, number>;
  wastedProbeUsd: number;
  wastedProbes: number;
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

/** Counts per bucket, one for each of the 84 log-spaced buckets src/lib/histogram.ts defines. */
export interface LatencyHistograms {
  durationMs: number[];
  outputTps: number[];
  ttfbMs: number[];
  ttftMs: number[];
}

/** Why probes a recipe had already paid for went unanswered: the stop that left them, or a request after them that read no cache. */
export type WastedProbeReason = 'cold_return' | 'evicted' | 'expired' | 'horizon' | 'idle_limit' | 'parked_limit' | 'released' | 'replaced' | 'session_ended';

/** An hour's last quota sample of one window: `lead` null for a sample that reported the reset alone, `sampledAt`
 *  the instant it was taken, which the lead and the Pace score read. */
export interface WindowSample {
  /** The instant the sample's lead reads, the earlier of the window's reset and the account's end. */
  deadline: string | null;
  lead: number | null;
  resetsAt: string | null;
  sampledAt: string;
  utilization: number | null;
}

export interface AccountRollup {
  accountId: string;
  cooldownSeconds: number;
  hour: string;
  overageSeconds: number;
  /** Null for a window the hour holds no utilization sample of. */
  peak: { '5h': number | null; '7d': number | null; '7d_fable': number | null };
  pins: { created: number; migratedIn: number; migratedOut: number };
  /** Keyed by the ledger's window ids, never folded: an hour's last sample per window, absent where the hour holds none. */
  windows: Partial<Record<string, WindowSample>>;
}

/**
 * The dimensions the burn route groups on: the breakdown's, and `session`, which reads the twenty sessions with the
 * most tokens and which no other route groups on.
 */
export type BurnGroup = Exclude<AnalyticsGroupBy, 'all'> | 'session';

/** One group's burn over one bucket, as `GET /admin/analytics/burn` answers it. */
export interface BurnPoint {
  at: string;
  /** Cache-read tokens. */
  cachedTokens: number;
  /** The bucket's plan rows as each plan tier's share of its own 7-day window, under every grouping, keyed by the tiers
   *  of the cost model's capacity rows, the same keys on every point. Each row prices under the cost revision in force
   *  when it arrived and its own regime before the sum. A billed row adds a zero on every tier, so a bucket with tokens
   *  and no plan row reads 0. Null on every tier for an idle bucket and for one holding a row the cost model cannot
   *  weigh, and on one tier where a row's revision does not size that tier's window. */
  equivalentByTier: Record<string, number | null>;
  /** The same over the bucket's plan rows whose model draws the Fable week, as each tier's share of its Fable week. A
   *  bucket with traffic and no such row reads 0 on every tier. Null as `equivalentByTier` reads it, on the Fable week.
   *  It carries no flag of its own. */
  fableEquivalentByTier: Record<string, number | null>;
  /** The bucket's plan rows as each plan tier's share of its own 5-hour window, each row under its own regime and the
   *  band of its hour, under every grouping and bucket size. A billed row adds a zero on every tier. Null as
   *  `equivalentByTier` reads it, on the 5-hour window. It carries no flag of its own. */
  fiveHourEquivalentByTier: Record<string, number | null>;
  /** A subscription account's spend in the bucket as a fraction of its own 5-hour window, the band weighed in, and
   *  `null` for an API-key account, one whose tier is unknown, one whose window set lacks the window, a bucket with no
   *  row the cost model prices (an idle hour among them) or one holding a row the cost model cannot price, and wherever
   *  the group holds more than one account's rows (every grouping but the account one, unless the Accounts filter names
   *  exactly one account). */
  fiveHourShare: number | null;
  group: string;
  /** The same spend as a fraction of the account's model week, null where `fiveHourShare` is for any reason but the
   *  window set and where no row of the bucket is on a model with a week of its own. */
  modelWeekShare: number | null;
  /** The list dollars of the bucket's plan-funded rows at the API price revision in force at each row, 0 for a bucket
   *  with no plan row. `usd` keeps the billed rows alone. A plan row the revision lists no rate for adds nothing, so a
   *  zero does not tell a bucket with no plan row from one whose plan rows have no list price. */
  planUsd: number;
  /** The same spend as a fraction of the account's 7-day window, unbanded, null where `fiveHourShare` is for any
   *  reason but the window set and where the account's window set lacks the 7-day window. */
  sevenDayShare: number | null;
  /** Every token of the bucket, all five types: zero for a bucket with no traffic the ledger can attribute. */
  tokens: number;
  /** Uncached input tokens. */
  uncachedTokens: number;
  /** Whether the bucket holds a plan row with tokens whose model has no weight on the 7-day window, which nulls every
   *  week entry. So a null week entry with the flag unset is an idle bucket or a tier a row's revision does not size.
   *  The shares carry no flag of their own. */
  unpriced: boolean;
  /** What the account spends in: a subscription's window share, even while no tier prices it, or an API key's
   *  dollars. Null where the shares are, and for an account the registry no longer holds. */
  unit: BurnUnit | null;
  /** The estimated cost of every row of the bucket a plan did not pay. */
  usd: number;
}

/** The cache-write tokens the first request after a move wrote, per bucket. */
export interface RewritePoint {
  at: string;
  rewriteTokens: number;
}

/** The slow-down's held time per bucket: the bucket's slow-down waits summed and the requests that waited. */
export interface HeldPoint {
  at: string;
  heldMs: number;
  requests: number;
}

/** One model and category's refused answers in range, as `GET /admin/analytics/refusal_categories` answers them, `-`
 *  for no category. */
export interface RefusalCategoryRow {
  category: string;
  model: string;
  requests: number;
}

export interface RefusalCategoriesAnswer {
  /** The requests in range whose row carries a fallback digest. */
  fallbacks: number;
  from: string;
  rows: RefusalCategoryRow[];
  to: string;
}

/** The rewrites route's answer: the moves by reason, and by reason and destination fit, over the range. */
export interface RewritesAnswer {
  /** One point per bucket as `points` has, empty on an answer from before the member. */
  held: HeldPoint[];
  moves: Record<string, number>;
  /** Each reason's moves by the fit of the destination they landed on (`fits`, `rewrite_only`, `least_bad`, `unknown`). */
  movesByReasonAndFit: Record<string, Record<string, number>>;
  points: RewritePoint[];
}

/** One completed window of an account against the meter, as `GET /admin/analytics/reconciliations` answers it. */
export interface Reconciliation {
  accountId: string;
  /** When the ledger reconciled the window, as an ISO instant. */
  computedAt: string;
  firstReadingAt: string;
  lastReadingAt: string;
  /** The last reading's utilization less the first's, as a fraction of the window. */
  observedRise: number;
  /** The share of the window the router priced its own attempts between the two readings at. */
  predictedShare: number;
  /** `observedRise` over `predictedShare`: 1.1 means the meter charged 1.1 times the table. */
  ratio: number;
  /** The attempts priced. */
  requests: number;
  /** `five_hour`, `seven_day` or `seven_day_fable`, as the route spells it. */
  window: string;
  /** The reset the window's readings named, as an ISO instant. */
  windowEnd: string;
}

/** The windows that ended in the trailing 30 days, newest first, and the request retention that bounds them. */
export interface ReconciliationsAnswer {
  reconciliations: Reconciliation[];
  /** The days the ledger keeps a request row, 0 for no limit: a window whose first reading is older finds no request
   *  to price, so it is never reconciled. */
  requestRetentionDays: number;
}

/** Where a weekly reset's share comes from: a used limit reset that cleared the window inside its week, a usage sample,
 *  the clock walk (its reading as the event carries it, up to one poll interval old), a plan change that cleared the
 *  window inside its week, or the account's term ending inside the week. The router stores whatever word it writes. */
export type AllowanceResetSource = 'claim' | 'clock' | 'plan_changed' | 'sample' | 'term_ended' | string;

export interface AllowanceReset {
  /** The drop's instant on a `claim` or a `plan_changed` row, the poll that wrote the drop's event. */
  observedAt: string;
  /** The pair of the reading the reset replaced, null on a reset whose event carries none and on a term's end. */
  plan: PlanWord | null;
  /** The unchanged reset of the week the drop fell in on a `claim` or a `plan_changed` row, a day still ahead for a
   *  drop in an open week. */
  resetAt: string;
  /** One less the utilization before the reset, inside the unit interval, before the drop on a `claim` or a
   *  `plan_changed` row. */
  shareLeft: number;
  source: AllowanceResetSource;
  /** The tier of that reading, null where `plan` is. */
  tier: string | null;
}

export interface AllowanceWindow {
  /** The window's claim rows, each a used limit reset that cleared it, by the drop's instant, newest first. */
  claims: AllowanceReset[];
  /** The window's rows that are no claim, newest first by the instant each draws at: the drop's for a plan change, the
   *  reset's for each other. */
  resets: AllowanceReset[];
  /** The route's sum over the rows of both lists. */
  totalShareLeft: number;
  /** `seven_day` or a model week's key, as the route spells it. */
  window: string;
}

export interface AllowanceAccount {
  accountId: string;
  plan: PlanWord | null;
  tier: string | null;
  windows: AllowanceWindow[];
}

/** The weekly resets in range per subscription account, as `GET /admin/analytics/unused_allowance` answers them. */
export interface UnusedAllowanceAnswer {
  accounts: AllowanceAccount[];
  from: string;
  to: string;
}

export const PARK_KIND_VALUES = ['background', 'both', 'scheduled'] as const;
/** What a parked agent waits on: a background command or a subagent, a scheduled wakeup, or both. */
export type ParkKind = (typeof PARK_KIND_VALUES)[number];

/** Whether the companion mod registered the session and is still reporting. */
export type CompanionState = 'none' | 'reporting' | 'silent';

export const BURN_UNIT_VALUES = ['usd', 'window_share'] as const;
/** Which figure a pin's burn reads: the share of the account's own 5-hour window per hour on plan quota, dollars per hour off it. */
export type BurnUnit = (typeof BURN_UNIT_VALUES)[number];

export const LIFECYCLE_VALUES = ['busy', 'idle', 'parked', 'registered', 'released', 'unknown'] as const;
/** The lifecycle word of a row, a line and a recipe. `registered` reads on a record the mod registered that has sent
 *  no real request yet. */
export type Lifecycle = (typeof LIFECYCLE_VALUES)[number];

/** A pin's burn members: a row's for its display pin, a line's for its own. */
export interface BurnFigures {
  /** The pin's typed burn per hour in `burnUnit`, priced when read, on plan quota the share of the account's own window
   *  `burnWindow` names. It reads 0 for a pin never renewed with usage, and null on a row with no pin at all, for a pin
   *  whose account left the registry, on a subscription account whose tier is unknown, or on a model it has no weight
   *  for, or a tier it has no capacity for on that window. */
  burn: number | null;
  /** The instant the pin's served burn is decayed to. Null for a pin never renewed with usage. */
  burnAt: string | null;
  /** The Pro-week burn per hour, the share of the Pro 7-day window a Pro account would spend on it, null where
   *  `burnUnit` is not `window_share` or the model has no weight. */
  burnEquivalent: number | null;
  /** Null on a row with no pin, or for a pin whose account left the registry: no kind is left to pick a unit from. */
  burnUnit: BurnUnit | null;
  /** The pin's plan copy per hour in dollars at the API price list in force, priced when read. Null where `burnUnit`
   *  is not `window_share`, on a row with no pin, where the dollar table lists no row for the pin's model and where a
   *  token type the pin burns has no rate in it. It reads a figure where `burn` reads null for a tier the cost model
   *  does not size or a model it does not weigh, since the dollar table reads neither. A billed pin keeps its dollars
   *  in `burn`. */
  burnUsd: number | null;
  /** The window `burn` is read on: `five_hour` where the account's shared window set has it, `seven_day` otherwise,
   *  null where `burnUnit` is not `window_share`. */
  burnWindow: string | null;
}

/** A pin's move figures: a row's for its display pin, a line's for its own. */
export interface MoveFigures {
  /** The cost a move of the pin pays as a warm pin, as a shedding round's pick compares it. Null where the cost model
   *  sizes none, and for a pin that no round can move: one that holds state on its account and one with no
   *  destination. */
  moveCost: number | null;
  /** The share of the dropped thinking as output on the window moveCost reads, which a cold pin's move pays. Null
   *  where moveDropSize is. */
  moveDropCost: number | null;
  /** The dropped thinking at the output's weight, in tokens, which orders a cold pin whose move drops thinking. Null
   *  where the move drops nothing, a pin with no destination among them, and where the cost model weighs none. */
  moveDropSize: number | null;
  /** The size the round's order divides the pin's burn by, in tokens. Null on a pin with no prompt size. */
  moveSize: number | null;
}

export interface AgentContext extends BurnFigures, MoveFigures {
  /** Whether the pin holds state on its account, such as a file, a container or a skill, so that the router keeps it
   *  there. */
  accountBound: boolean;
  /** The pin's account. */
  accountId: string;
  agentId: string;
  /** The cache TTL in force at the pin's last refresh. */
  cacheTtlMs: number | null;
  /** The effective effort of the pin's last turn, the word as the client sent it. */
  effort: string | null;
  /** Whether the pin's agent ended and its next request has not arrived. A shedding round leaves such a pin out. */
  ended: boolean;
  /** Whether the pin's lane is the fast lane, as the row's `fast`. A round on a plan window leaves such a pin out. */
  fast: boolean;
  lastInputTokens: number | null;
  lastUsedAt: string | null;
  /** The agent's lifecycle word from the keepalive engine's record, null where no mod reports it. */
  lifecycle: Lifecycle | null;
  model: string;
  /** The instant the pin was written on the account it stands on. */
  pinnedAt: string | null;
  /** The route's reading of the selector's rule: whether the pin's last refresh is inside its cache TTL. */
  warm: boolean;
  /** The first instant `warm` answers false for the pin, null only where an answer carries none. */
  warmUntil: string | null;
}

/** A pin under a pool's keys as a shedding round reads it, a pin the Sessions list hides among them. */
export interface PoolPin extends MoveFigures, Pick<BurnFigures, 'burn' | 'burnAt' | 'burnUnit' | 'burnWindow'>, Pick<AgentContext, 'accountId' | 'agentId' | 'ended' | 'fast' | 'model' | 'warm' | 'warmUntil'> {
  accountBound: boolean;
  keyId: string;
  /** The standing mark on this pin, as a session row's `mark`. */
  mark: Session['mark'];
  sessionId: string;
}

/** A set of a session's requests: their count, their money and their shares per account. */
export interface SessionSpendPart {
  /** The dollars billed by funding (`api_key`, `fast`, `overage`, `usage_based`), a zero entry left out. */
  billedUsd: Record<string, number>;
  /** The dollars each serving account billed, an account with none left out. */
  billedUsdByAccount: Record<string, number>;
  /** The Pro-week figures summed, null where a request priced nothing and 0 with no plan-funded request. */
  equivalent: number | null;
  requests: number;
  /** Per account, its own windows' shares under the tier the serving attempt recorded. Every plan-funded request
   *  writes its serving account's key, a free one with no share. */
  shares: Record<string, Record<string, number>>;
  /** The list dollars, the sum of what the request log shows as the cost. */
  usd: number;
}

/** A session's spend over its client requests, and the same figures over its probes, apart. */
export interface SessionSpend extends SessionSpendPart {
  probes: SessionSpendPart;
}

/** A session's client requests of the 24 whole hours before the current hour and the current hour so far, by class,
 *  and their spend. */
export interface SessionDay extends Pick<SessionSpendPart, 'billedUsd' | 'equivalent'> {
  /** By request class as the ledger holds it, `other` for a request with none. */
  requests: Record<string, number>;
}

/** A session's requests of one agent, from the ledger: its sums over every model it ran, with the model, the
 *  account, the type and the instant of its newest client request. An agent the ledger holds probes of alone reads
 *  its newest probe's model, with a null account, type and instant. */
export interface AgentRequests {
  /** The newest client request's serving account. */
  accountId: string | null;
  agentId: string;
  /** The newest request's agent type. */
  agentType: string | null;
  /** The agent's first client request over the session's life, refused ones included, null where the ledger holds no
   *  request of the agent that is no probe. */
  firstRequestAt: string | null;
  lastRequestAt: string | null;
  model: string;
  spend: SessionSpend;
}

/** What a session's companion reports of it, each member null where it reports none. */
export interface SessionFacts {
  branch: string | null;
  cwd: string | null;
  project: string | null;
  /** The origin remote as `host/path`. */
  repository: string | null;
  repositoryRoot: string | null;
  title: string | null;
}

/** What a session's companion reports of one agent. */
export interface SessionAgentFacts {
  agentId: string;
  agentType: string | null;
  description: string | null;
  name: string | null;
}

/**
 * On a row with no pin, `burnAt`, `moveCost` and `moveSize` read null, and a reader takes a null `moveCost` as 0.
 * `moveCost` reads null too on a pin that holds state on its account and on a pin with no destination, since no round
 * moves either. The row's `moveSize` is the rewrite at the pin's destination plus the dropped thinking at the output's
 * weight.
 */
export interface Session extends BurnFigures, MoveFigures {
  /** Whether the pin holds state on its account, such as a file, a container or a skill, so that the router keeps it
   *  there. Null on a row with no pin. */
  accountBound: boolean | null;
  accountId: string | null;
  /** One entry per conversation pin that draws a line, main's among them, and empty where none does. */
  agentContexts: AgentContext[];
  /** One entry per agent whose companion reported a fact, in agent id order. Empty where none did. */
  agentFacts: SessionAgentFacts[];
  /** The pin's own agent: `main`, or a subagent's id, and `null` for a session with no pin. */
  agentId: string | null;
  /** The session's requests per agent, main's among them, each with its type, its newest request's account and
   *  instant and its spend. */
  agentRequests: AgentRequests[];
  /** The cache TTL in force at the pin's last refresh, and `null` for a session with no pin. */
  cacheTtlMs: number | null;
  companion: CompanionState;
  /** The companion's last report while `companion` reads `silent`, and the session's end on a row no companion
   *  reports whose mod ended it. Null otherwise. */
  companionSince: string | null;
  /** The pin's cycle, null until two requests of the pin after its placement have measured a gap under the pool's
   *  burn average, and on a row with no pin. */
  cycleMs: number | null;
  /** The session's client requests of the 24 whole hours before the current hour and the current hour so far, by
   *  class, and their spend. */
  day: SessionDay;
  /** The effective effort of the pin's last turn, the word as the client sent it, and `null` with none or no pin. */
  effort: string | null;
  /** Whether the display pin's agent ended and its next request has not arrived, null on a row with no pin. */
  ended: boolean | null;
  /** The factor of the slow-down entry in force on the pin's account, null on a row with no pin and where no entry is
   *  in force. */
  factor: number | null;
  facts: SessionFacts;
  /** Whether the display pin's lane is the fast lane, null on a row with no pin. */
  fast: boolean | null;
  /** The session's first request under its key that is no probe, refused ones included, null where the ledger holds
   *  none. */
  firstRequestAt: string | null;
  /** The depth where a forked session parts from its base, and `null` with `forkedFrom`. */
  forkDepth: number | null;
  /** The session this one branched from, null for one that branched from none. */
  forkedFrom: string | null;
  /** A generation no live key holds: the ledger's memory of an earlier holder of this key id. Read-only, no recipe, no release. */
  historical: boolean;
  keepalive: {
    /**
     * The cause of the agent whose recipe stands for the session, main's where main holds one and else the newest
     * other agent's. `session_opted_out` for a session whose own option holds the cache warming off, and null where no
     * agent holds a recipe.
     */
    cause: RecipeCause | null;
    /** The soonest probe that the agents of the session have scheduled, null while none of them holds one. */
    nextProbeAt: string | null;
  };
  /** The `created_at` of the key that owns the session. */
  keyGeneration: number;
  keyId: string;
  /** The session's newest request that is neither a probe nor a token count, refused ones included, null while the
   *  ledger holds none, as before a first turn. */
  lastInferenceAt: string | null;
  /** The input, cache-read and cache-write tokens of the pin's last renewal, and `null` for a session with no pin. */
  lastInputTokens: number | null;
  /** The pin's last main, subagent or compaction turn, or a keepalive probe's renewal, and `null` for a session with
   *  no pin. */
  lastRefreshAt: string | null;
  /** Last ledgered client request, and `null` for a session that has not been seen in the ledger. */
  lastRequestAt: string | null;
  /** A session known from the ledger or from a registration alone carries no activity of its own, so the last ledgered
   *  request stands in for it. */
  lastUsedAt: string | null;
  /** Main's lifecycle word from the keepalive engine's record, whether or not a recipe stands, null where no mod
   *  reports the session. */
  lifecycle: Lifecycle | null;
  /** The pin's open shedding mark: when the round set it, the account it stands on and the window whose trigger set
   *  it. Null where the pin holds none and on a row with no pin. */
  mark: { at: string; on: string; window: string } | null;
  model: string | null;
  /** Whether the session is pinned to its account for as long as affinity holds. */
  pinned: boolean;
  /** The instant the pin was written on the account it stands on, null on a row with no pin. */
  pinnedAt: string | null;
  /** How many times the router placed or moved the session's main conversation under its key, 0 where the ledger
   *  holds none. */
  placements: number;
  /** Seconds since main's last presence report, null before the first one. */
  presenceAgeSeconds: number | null;
  /**
   * Null with no projection, false for an unbounded runway, true for a bounded one, whose `runwayMs` is set. Null on an
   * unpinned row.
   */
  runwayBounded: boolean | null;
  /** Until the pin's account's projection reaches the ceiling on the window that binds, null unless `runwayBounded`. */
  runwayMs: number | null;
  sessionId: string;
  /**
   * The pin's no-return stamp, which records a move that happened and is no mark: its instant, where a shedding
   * round's move or a failover's landing took the pin `from`, which keeps the pin off that account for the dwell, and
   * why the move took it off: `round` for a shedding round's move, the failover's word (`cooldown`, `fast_lane`,
   * `mark`) for its landing. Null off a pin never stamped, or with no pin.
   */
  shed: ShedStamp | null;
  /** The speed the account's holds realized inside a slow-down span, from 0 to 1. Null outside a span and where
   *  `factor` is. */
  speed: number | null;
  /** The spend of the session's requests that the retention keeps under its key, null where the ledger holds no
   *  request of it the router did not refuse. */
  spend: SessionSpend | null;
  /** The user agent of the session's newest client request, as the client sent it. */
  userAgent: string | null;
  /** Whether the pinned account still holds the session's prompt cache, from the pin's last refresh and its cache
   *  TTL as the selector reads it. Null without a pin. */
  warm: boolean | null;
}

export const RECIPE_STATE_VALUES = ['ended', 'held', 'not_retained', 'paused', 'retained'] as const;
/** The Sessions row's one word for an agent's recipe, read off the engine's cause and its pause. */
export type RecipeState = (typeof RECIPE_STATE_VALUES)[number];
/** Every cause word the engine serializes, so the decoder never reads one it sends as none. */
export const RECIPE_CAUSE_VALUES = ['affinity_too_short', 'agent_limit', 'agent_released', 'agent_unknown', 'awaiting_cache_evidence', 'awaiting_request', 'awaiting_selection', 'budget', 'bus_gap', 'compacted', 'cooled', 'credits', 'evicted', 'expired', 'expired_at_restart', 'fixed_thinking', 'forced_tool', 'headers_too_large', 'horizon', 'idle_limit', 'ineligible_credential', 'instance_mismatch', 'instance_replaced', 'invalid_cache_control', 'invalid_model', 'invalid_probe_usage', 'invalid_request', 'keepalive_off', 'key_opted_out', 'lease_expired', 'ledger_behind', 'missing_account', 'no_cache_evidence', 'no_pin', 'no_probe_cache_evidence', 'opted_out', 'overage', 'parked_limit', 'partial_cache_coverage', 'paused', 'pin_moved', 'pool_disabled', 'probe_authentication_error', 'probe_pool_exhausted', 'probe_quota_exhausted', 'probe_retry_limit', 'probe_transport_error', 'probing', 'reauth', 'repeated_cold_probe', 'request_canceled', 'request_failed', 'request_limit', 'request_rejected', 'request_too_large', 'reset', 'retryable_probe_error', 'scheduled', 'session_ended', 'session_expired', 'session_limit', 'session_opted_out', 'slow_probe', 'spent', 'storage_limit', 'structured_output', 'unexpected_output', 'unsupported_probe', 'unsupported_shape'] as const;
/** The engine's own reason, every word it serializes. */
export type RecipeCause = (typeof RECIPE_CAUSE_VALUES)[number];

export const GUARD_VERDICT_VALUES = ['auxiliary', 'compaction', 'foreign', 'rearm', 'restart'] as const;
/** What the last request the agent observed did to its recipe: `auxiliary` and `foreign` both left it alone,
 * `rearm` and `restart` both fed it, and `compaction` did neither, since the completed compaction ended the recipe.
 * Never a hold: `cause` names that separately. */
export type GuardVerdict = (typeof GUARD_VERDICT_VALUES)[number];

/** One row per agent that has sent a real request: its recipe's state and cause, its lifecycle and its probes. */
export interface KeepaliveRecipe {
  /**
   * The recipe's holder, the account its probes run on: the account the agent is pinned to, except behind `pin_moved`,
   * where it keeps the account the pin left until the agent's next request arms the recipe.
   */
  accountId: string | null;
  agentId: string;
  agentState: Lifecycle;
  cause: RecipeCause;
  /** Requests credited as kept warm since midnight UTC. */
  creditedToday: number;
  /** When the harness stops the background command a park waits on, null off a park on one. */
  deadline: string | null;
  keyId: string;
  /** Null only before the agent's first observation. */
  lastGuard: GuardVerdict | null;
  lastProbeAt: string | null;
  /** This agent's last real request. */
  lastRequestAt: string | null;
  model: string;
  nextProbeAt: string | null;
  parkKind: ParkKind | null;
  poolId: string;
  /** Probes sent since midnight UTC. */
  probesToday: number;
  sessionId: string;
  state: RecipeState;
  ttlSeconds: number;
  /** When the scheduled wakeup a park waits on fires, null off a park on one. */
  wakeAt: string | null;
}

export interface KeepaliveOverview {
  /** Times the keepalive engine found it had missed bus events; each gap discarded every observation still open. */
  busGaps: number;
  /** Requests whose prefix expired between turns in the last 24 hours. */
  expiredBetweenTurns24h: number;
  /** The last bus gap's instant, null before the first gap. */
  lastBusGapAt: string | null;
  /** The engine's live sessions, agent records and retained bytes, each beside the cap the bootstrap file sets. */
  limits: Record<'agents' | 'retainedBytes' | 'sessions', { count: number; max: number }>;
  /**
   * Per pool, the last 24 hours' probes, credits and wasted probes, and the idle-age counters' ten buckets, the last
   * open-ended. An account outside every pool lists under its own id.
   */
  pools: { credited24h: number; idleReturns: { fromSeconds: number; toSeconds: number | null; reached: number; returned: number }[]; poolId: string; probes24h: number; wasted24h: number; wastedReasons24h: Record<WastedProbeReason, number> }[];
  probes24h: { failed: number; held: number; ok: number; skipped: number };
}

/** Five cells, USD per million tokens: a model's standard, fast-mode or long-context prices. */
export interface PriceCells { cacheReadPerM: number; cacheWrite1hPerM: number; cacheWrite5mPerM: number; inputPerM: number; outputPerM: number }
/** The prices of an attempt whose prompt is longer than `over` tokens. */
export interface LongContextPrice extends PriceCells { over: number }

export interface ModelPrice extends PriceCells {
  /** The fast cells, null on a model that serves no fast mode. */
  fast: PriceCells | null;
  /** The long-context set, null on a model priced one way. */
  longContext: LongContextPrice | null;
  model: string;
}

export interface TypeMultiplier { cacheRead: number; cacheWrite1h: number; cacheWrite5m: number; input: number; output: number }
/** Each tier's capacity on the two windows, null where the cost model sizes none. */
export type CapacityRows = Record<string, { fiveHour: number | null; sevenDay: number | null }>;
export interface WindowFactors { fiveHour: number; sevenDay: number }
/** A pricing regime of the cost model: the entrypoints it covers, its capacity rows and its band factors. */
export interface Regime {
  capacity: CapacityRows;
  entrypoints: string[];
  peakCapacityFactor: WindowFactors;
}
/** The subscription cost model as the ledger loaded it, the file's keys in camel case and its maps in the file's order. */
export interface SubscriptionCosts {
  capacity: CapacityRows;
  effective: string;
  /** Each undated model id's model tier, a name of `tierOrder`. */
  modelTier: Record<string, string>;
  modelWeight: Record<string, number | null>;
  modelWindows: Record<string, { capacityFactor: number; of: string }>;
  overrides: Record<string, { typeMultiplier?: Partial<TypeMultiplier>; weightByWindow?: Record<string, number> }>;
  peak: { capacityFactor: WindowFactors; fromHour: number; toHour: number; weekdays: boolean; zone: string };
  /** The regimes by name, in the file's order. A request prices under the one listing its entrypoint, else the base. */
  regimes: Record<string, Regime>;
  /** The model tiers in display order. */
  tierOrder: string[];
  typeMultiplier: TypeMultiplier;
  /** The anchor every figure divides by, the envelope's one unit handed to every revision: its tier's row on its
   *  window. */
  unit: { tier: string; window: string };
}

/** The two members every model tier order reads, so a caller with the cost model's members alone can order models. */
export type ModelTiers = Pick<SubscriptionCosts, 'modelTier' | 'tierOrder'>;

/** One dated entry of the price table: `effective` as the router sent it, `effectiveDate` the instant it parses to. */
export interface PriceRevision {
  effective: string;
  effectiveDate: string | null;
  models: ModelPrice[];
}

/** One dated entry of the subscription cost model, read as strictly as the entry in force. */
export interface CostRevision {
  content: SubscriptionCosts;
  effective: string;
  effectiveDate: string | null;
}

/** The feature listener's CA, as the client card prints it. */
export interface FeatureSettings {
  /** The SHA-256 of the bytes the CA download serves, as 64 lowercase hex characters, which each launcher compares. */
  caFingerprint: string;
  /** The CA's certificate as PEM text. */
  caPem: string;
}

export interface Settings {
  /** `build` is the commit the router was built from, or `unknown` where it names none. */
  about: { build: string; profile: string; version: string };
  access: { adminConfigured: boolean };
  /**
   * The price table used to cost the ledger, its rows in the file's order. `effective` is the date as the router sent
   * it, `effectiveDate` the instant it parses to, or null when it is not a date. `revisions` lists every dated entry
   * of the file, ascending by date, the entry in force among them.
   */
  apiPrices: { effective: string; effectiveDate: string | null; models: ModelPrice[]; revisions: PriceRevision[]; source: string };
  /** Null on a router without the feature listener. */
  features: FeatureSettings | null;
  /** `proxyTls`: whether the proxy listener terminates TLS itself, which sets the client snippet's scheme. */
  listeners: { metrics: string; proxy: string; proxyTls: boolean };
  /** The two day counts are file settings of the `stats` section, 0 for forever. */
  retention: { bodyCaptureMaxHours: number; retentionDays: number; rollupRetentionDays: number };
  /**
   * How long a session stays listed after its last activity, and how long a session no mod reports stays listed
   * after its last use. The lesser of the timeout and the session expiry applies.
   */
  sessions: { expireAfterSeconds: number; nonCompanionIdleSeconds: number };
  /** Each path is null when no override file is configured. */
  storage: { apiPricesPath: string | null; ledgerPath: string; path: string; subscriptionCostsPath: string | null };
  /**
   * The envelope's `content` with its `source` beside it. An answer without the envelope reads as an empty model.
   * `problem` is the sentence the cost views show in place of a model the answer carries and the console cannot read,
   * which then reads as the empty model too, and `null` otherwise.
   * `revisions` lists every dated entry of the file, ascending by date, and is empty where the model reads as empty.
   */
  subscriptionCosts: SubscriptionCosts & { problem: string | null; revisions: CostRevision[]; source: string };
  /** `limitResetWarningDays`: how many days before an unused limit reset ends the warning shows, 0 for never, a file
   *  setting the console shows and never edits. */
  usage: { limitResetWarningDays: number };
}

export interface SessionInfo {
  /** Whether the session is the 30-day kind a checked login asked for. No view draws it. */
  persistent: boolean;
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
  /** The day's sum: the billed rows' dollars, every row's cost and the plan rows as the Pro-week figure. */
  day24h: FundedSum;
  errorRate24h: number | null;
  /** `billed` and `equivalent` split each hour as `day24h` splits the day. */
  hourly: { at: string; billed: number; cost: number; equivalent: number | null; errors: number; refused: number; requests: number; tokens: number }[];
  /** The requests the router refused before any account was asked, not in `requests24h`. */
  refused24h: number;
  requests24h: number;
  tokens24h: number;
  tokensCached24h: number;
  topErrorCode: string | null;
  topRefusalCode: string | null;
}

export const GUARDRAIL_MODE_VALUES = ['enforce', 'observe'] as const;
/** Observe records a match on the request, enforce refuses the request with a 403. */
export type GuardrailMode = (typeof GUARDRAIL_MODE_VALUES)[number];
export const CONDITION_SUBJECT_VALUES = ['agent_type', 'body', 'client', 'header', 'kind', 'message', 'model', 'request_class', 'user_agent'] as const;
export type ConditionSubject = (typeof CONDITION_SUBJECT_VALUES)[number];
export const CONDITION_OPERATOR_VALUES = ['absent', 'equals', 'gt', 'lt', 'matches', 'present'] as const;
export type ConditionOperator = (typeof CONDITION_OPERATOR_VALUES)[number];
export const MESSAGE_SELECTOR_VALUES = ['any', 'any_assistant', 'any_user', 'first_user', 'last', 'last_assistant', 'last_user'] as const;
export type MessageSelector = (typeof MESSAGE_SELECTOR_VALUES)[number];

/**
 * One condition as the route spells it, every member a string. `name` is the `header` subject's, `path` the `body`
 * subject's, `selector` the `message` subject's, and `value` the operand of `equals`, `gt`, `lt` and `matches`.
 */
export interface Condition {
  name?: string;
  operator: ConditionOperator;
  path?: string;
  selector?: MessageSelector;
  subject: ConditionSubject;
  value?: string;
}

/** A pool or key a guardrail is attached to, and the guardrail's place in that subject's list. */
export interface GuardrailAttachment {
  id: string;
  /** The pool's or the key's label at the read. */
  label: string;
  /** Orders the subject's list. It keeps its value when another guardrail is detached, so it is no index. */
  position: number;
}

/** Where a guardrail came from, as the route reads it against the presets this build ships. */
export interface GuardrailPresetState {
  /** The highest version this build ships of the preset. */
  currentVersion: number;
  /** The conditions differ from the stamped version's. */
  customized: boolean;
  id: string;
  version: number;
}

export interface Guardrail {
  action: 'block';
  /** In the written order, which every refusal and the dry run name by index. */
  conditions: Condition[];
  createdAt: string;
  description: string;
  enabled: boolean;
  id: string;
  keys: GuardrailAttachment[];
  mode: GuardrailMode;
  name: string;
  pools: GuardrailAttachment[];
  preset: GuardrailPresetState | null;
  /** The row's edit token, which an edit sends back as `expected_revision`. */
  revision: number;
  updatedAt: string;
}

/** A guardrail on a pool's or a key's list. */
export interface GuardrailRef {
  enabled: boolean;
  id: string;
  mode: GuardrailMode;
  name: string;
  /** Orders the list. It keeps its value when another guardrail is detached, so it is no index. */
  position: number;
}

/** A preset the binary ships, at its current version. */
export interface GuardrailPreset {
  conditions: Condition[];
  description: string;
  id: string;
  /** The client versions the preset was checked against, as one line to show. */
  measuredOn: string;
  name: string;
  suggestedMode: GuardrailMode;
  version: number;
}

/** A guardrail that matched a request, in evaluation order. */
export interface GuardrailMatch {
  blocked: boolean;
  id: string;
  position: number;
}

export const TEST_VERDICT_VALUES = ['indeterminate', 'matched', 'unmatched'] as const;
export type TestVerdict = (typeof TEST_VERDICT_VALUES)[number];

/** The dry run's answer over the captured bodies of a pool or a key. */
export interface GuardrailTestResult {
  /**
   * Newest first. `condition` is the written index of the failing condition on `unmatched`, of the header condition
   * the row cannot answer on `indeterminate`, and null on `matched`.
   */
  requests: { condition: number | null; id: string; receivedAt: string; verdict: TestVerdict }[];
  sample: { keyId: string | null; limit: number; poolId: string | null; windowHours: number };
  totals: { indeterminate: number; matched: number; sampled: number; unmatched: number };
}

/** One guardrail's matches over a range: the observed ones, the blocks, and the blocked requests' bytes. */
export interface GuardrailCount {
  blocked: number;
  blockedBytes: number;
  id: string;
  /** Null for a guardrail deleted since it matched. */
  name: string | null;
  observed: number;
}

export interface GuardrailCounts {
  from: string;
  /** One row per guardrail that matched a request in the range, ordered by id. */
  guardrails: GuardrailCount[];
  to: string;
}
