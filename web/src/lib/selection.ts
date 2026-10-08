import type { Account, MemberEligibility, Pool, PoolMember, PoolStrategy, Session } from '@/api/types';
import { fmtPeriodEnd, fmtRelative } from './format';

export const STRATEGY_LABEL: Record<PoolStrategy, string> = { pace: 'Pace', priority: 'Priority, then random', round_robin: 'Round robin' };
/** The kind tiers, in the order selection walks them. */
export const GROUP_LABEL = ['plan', 'overage', 'API keys'] as const;

/** Whether selection walks the kind tiers for this pool: the operator's answer alone. */
export const kindOrdered = (pool: Pool) => pool.preferSubscription;
export const RULE_NOTE = 'A request for a Fable model is placed on the Fable window instead, which this card does not draw.';

/** Why the router skips a member: its own verdict, as the pool view reports it. */
export type Ineligible = Exclude<MemberEligibility, 'eligible'>;
export const INELIGIBLE_LABEL: Record<Ineligible, string> = { budget: 'at budget', cooldown: 'in cooldown', overage: 'in overage', unavailable: 'not available' };

/**
 * Why an eligible member takes a fresh session only once no other member can: under Pace its remaining allowance
 * is at or below the pool's floor, or, under every strategy, its subscription end is inside the pool's wind-down.
 */
export type LastResort = 'floor' | 'winding_down';
export const LAST_RESORT_LABEL: Record<LastResort, string> = { floor: 'at the Pace floor', winding_down: 'winding down' };

/** `deferred`: eligible but in overage under prefer_plan, so ranked after the plan-funded members of its tier. */
export interface Placement {
  account: Account;
  deferred: boolean;
  ineligible: Ineligible | null;
  lastResort: LastResort | null;
  pins: number;
  rank: number | null;
}
export interface Tier {
  group: 0 | 1 | 2;
  next: string[];
  placements: Placement[];
  priority: number;
}

const HOUR_MS = 3_600_000;
const WEEK_HOURS = 168;

/** The end an engine rule reads: the saved end while the account is cancelled, else a trial end still ahead. */
function endOf(account: Account, now: number): number | null {
  const term = account.subscription;
  if (!term) return null;
  if (account.cancelled) return term.endsAt === null ? null : Date.parse(term.endsAt);
  const trial = term.trialEndsAt === null ? Number.NaN : Date.parse(term.trialEndsAt);
  return trial > now ? trial : null;
}

/**
 * Remaining allowance and the reset that refills it: a subscription account reads the shared 7-day window, an
 * API-key account the worst window its row reports, with no reset. A window whose reset has passed, or none at
 * all, reads as untouched. The router ranks an API-key account by its per-model buckets, which the account row
 * does not carry, so for one this order is a preview; the request drawer's trace shows the order the router took.
 */
function remainingOf(account: Account, now: number): { remaining: number; reset: number | null } {
  const live = account.windows.filter((w) => w.resetsAt === null || Date.parse(w.resetsAt) > now);
  if (account.kind === 'api_key') return { remaining: Math.max(1 - Math.max(0, ...live.map((w) => w.utilization)), 0), reset: null };
  const w = live.find((x) => x.id === '7d');
  if (!w) return { remaining: 1, reset: null };
  return { remaining: Math.max(1 - w.utilization, 0), reset: w.resetsAt === null ? null : Date.parse(w.resetsAt) };
}

/**
 * Remaining allowance over the hours to the earlier of the window's reset and the account's end, so an account that
 * runs out sooner is spent first; an unsampled account counts as untouched, the full week's worth of it. An
 * API-key account scores by its remaining allowance alone, since its buckets refill within a minute.
 */
function paceScore(account: Account, now: number): number {
  const { remaining, reset } = remainingOf(account, now);
  if (account.kind === 'api_key') return remaining;
  const end = endOf(account, now);
  const deadline = reset === null ? end : end === null ? reset : Math.min(reset, end);
  const hours = deadline === null ? WEEK_HOURS : Math.max((deadline - now) / HOUR_MS, 1 / 60);
  return remaining / hours;
}

/** The sort key the strategy ranks an eligible account by; lower first. `priority` scores nothing. */
function keyOf(account: Account, strategy: PoolStrategy, now: number): number {
  if (strategy === 'pace') return -paceScore(account, now);
  return 0;
}

/**
 * Where a member falls in the router's fall-through for a fresh session, lowest first: above the floor and not
 * winding down, then the floor group, then winding down, then both. The router drops each group from the whole
 * selection set before it picks a tier, so a group trails every better member of every tier.
 */
function resortOf(pool: Pool, account: Account, now: number): { lastResort: LastResort | null; order: number } {
  const end = endOf(account, now);
  const windingDown = end !== null && now >= end - pool.windDownMinutes * 60_000;
  const atFloor = pool.strategy === 'pace' && remainingOf(account, now).remaining <= pool.paceFloor;
  return { lastResort: windingDown ? 'winding_down' : atFloor ? 'floor' : null, order: (windingDown ? 2 : 0) + (atFloor ? 1 : 0) };
}

/** Which kind tier an account sits in; a member the accounts list does not carry has no kind, so it leads. */
const groupOf = (account: Account | undefined): 0 | 1 | 2 => (account?.kind === 'api_key' ? 2 : account?.extraUsage?.inUse ? 1 : 0);

/** The order the router walks for a fresh session of this pool, from the data the routes already serve. */
export function selectionOrder(pool: Pool, accounts: Account[], sessions: Session[], now: number = Date.now()): Tier[] {
  const byId = new Map(accounts.map((a) => [a.id, a]));
  const pinsOf = (id: string) => sessions.filter((s) => s.pinned && s.accountId === id).length;
  const grouped = kindOrdered(pool);
  const groupFor = (accountId: string) => (grouped ? groupOf(byId.get(accountId)) : 0);
  const tiers = [...new Map(pool.members.map((m) => [`${groupFor(m.accountId)}:${m.priority}`, { group: groupFor(m.accountId), priority: m.priority }])).values()].sort(
    (a, b) => a.group - b.group || a.priority - b.priority,
  );
  const resort = new Map<string, number>();
  const built = tiers.map(({ group, priority }) => {
    const placements: Placement[] = pool.members
      .filter((m) => m.priority === priority && groupFor(m.accountId) === group)
      .flatMap((m) => {
        const account = byId.get(m.accountId);
        if (!account) return [];
        const ineligible = m.eligible === 'eligible' ? null : m.eligible;
        const { lastResort, order } = resortOf(pool, account, now);
        if (ineligible === null) resort.set(account.id, order);
        return [
          {
            account,
            // Under the kind order an account in overage already has a tier of its own, so deferring it inside
            // one would rank it behind its own siblings twice.
            deferred: !grouped && pool.overagePolicy === 'prefer_plan' && Boolean(account.extraUsage?.inUse),
            ineligible,
            lastResort: ineligible === null ? lastResort : null,
            pins: pinsOf(m.accountId),
            rank: null,
          },
        ];
      });
    return { group, placements, priority };
  });
  // The group the router settles in is the best one any eligible member of the pool reaches.
  const settled = Math.min(...resort.values());
  const orderOf = (p: Placement) => resort.get(p.account.id) ?? 0;
  const strategy = pool.strategy;
  let nextFound = false;
  return built.map(({ group, placements, priority }) => {
    // The last-resort group leads the sort, since the router narrows it away before it picks a tier. Inside a
    // group the router's tier is (priority, deferred): plan-funded members rank ahead of deferred ones, then the
    // strategy orders inside each half; equal scores share a rank because the router draws among them at random.
    const eligible = placements
      .filter((p) => p.ineligible === null)
      .sort(
        (a, b) =>
          orderOf(a) - orderOf(b) ||
          Number(a.deferred) - Number(b.deferred) ||
          keyOf(a.account, strategy, now) - keyOf(b.account, strategy, now) ||
          a.account.label.localeCompare(b.account.label),
      );
    let rank = 0;
    let previous: { deferred: boolean; order: number; score: number } | null = null;
    for (const p of eligible) {
      const score = keyOf(p.account, strategy, now);
      if (!previous || previous.order !== orderOf(p) || previous.deferred !== p.deferred || previous.score !== score) rank += 1;
      // Round robin and priority both score every member 0, so the rank only moves across the last-resort group
      // boundary and the deferred one: each member of a subtier is equally the next, and the router rotates the
      // plan-funded subtier on its own while it lasts, so an overage sibling ranks behind it rather than beside it.
      p.rank = rank;
      previous = { deferred: p.deferred, order: orderOf(p), score };
    }
    const ordered = [...eligible, ...placements.filter((p) => p.ineligible !== null).sort((a, b) => a.account.label.localeCompare(b.account.label))];
    const lead = eligible.find((p) => orderOf(p) === settled)?.rank;
    const next = !nextFound && lead !== undefined ? eligible.filter((p) => p.rank === lead).map((p) => p.account.id) : [];
    if (next.length) nextFound = true;
    return { group, next, placements: ordered, priority };
  });
}

export type StatusPart = { text: string } | { accountId: string; label: string };
export interface PoolStatusSentence {
  /** The parts after "<Word>: "; an account part draws as a link to /accounts/<id>. */
  parts: StatusPart[];
  status: Pool['status'];
  /** Active, Degraded, Disabled or Exhausted. */
  word: string;
}
const STATUS_WORD: Record<Pool['status'], string> = { active: 'Active', degraded: 'Degraded', disabled: 'Disabled', exhausted: 'Exhausted' };

/** "a", "a or b", "a, b or c". */
const listOf = (names: StatusPart[]): StatusPart[] => names.flatMap((name, i) => (i === 0 ? [name] : [{ text: i === names.length - 1 ? ' or ' : ', ' }, name]));

/**
 * When a skipped member returns, as the card already writes it: once every blocker the route reports has ended. The
 * verdict names the first blocker alone (unavailable, cooldown, budget, overage), while `budgetUntil` is set whenever
 * a budget holds the member, and under a pool's `never` overage policy the account's overage holds it until its window
 * resets. So a member in cooldown can sit at its budget and in overage too, and it returns at the latest of its
 * budget's period end (daily, monthly, or plain "budget" when no spend report names the period), its latest
 * account-scope cooldown end and its overage window's reset. A blocker without an end (unavailable, overage, a cooldown
 * without an account-scope row, an overage window without a reset) gives no return, and so does a budget verdict
 * without an end.
 */
function returnOf(pool: Pool, member: PoolMember, name: StatusPart, account: Account | undefined, now: number): { at: number; parts: StatusPart[] } | null {
  const ends: { at: number; parts: StatusPart[] }[] = [];
  if (member.budgetUntil) {
    const period = member.spends.find((s) => s.periodEnd === member.budgetUntil)?.period;
    const which = period === 'day' ? 'daily budget' : period === 'month' ? 'monthly budget' : 'budget';
    ends.push({ at: Date.parse(member.budgetUntil), parts: [name, { text: `'s ${which} resets at ${fmtPeriodEnd(member.budgetUntil)}` }] });
  }
  if (member.eligible === 'cooldown') {
    const until = (account?.cooldowns ?? [])
      .filter((c) => c.scope === 'account')
      .map((c) => c.until)
      .sort((a, b) => Date.parse(a) - Date.parse(b))
      .pop();
    if (!until) return null;
    ends.push({ at: Date.parse(until), parts: [name, { text: `'s cooldown ends ${fmtRelative(until, now)}` }] });
  } else if (member.eligible !== 'budget') {
    return null;
  }
  if (ends.length > 0 && pool.overagePolicy === 'never' && account?.overage?.inUse) {
    const { reset } = account.overage;
    if (reset === null) return null;
    ends.push({ at: Date.parse(reset), parts: [name, { text: `'s overage window resets ${fmtRelative(reset, now)}` }] });
  }
  return ends.sort((a, b) => b.at - a.at)[0] ?? null;
}

/**
 * One sentence on what the status badge means for this pool, from the verdicts and the order the card already reads.
 * `accounts` is null until the account list is read: without it the order places no member and no return or name can
 * be read, so the sentence says what the pool's own verdicts carry and names no account.
 */
export function poolStatusSentence(pool: Pool, accounts: Account[] | null, sessions: Session[], now: number = Date.now()): PoolStatusSentence {
  const { status } = pool;
  const word = STATUS_WORD[status];
  const byId = new Map((accounts ?? []).map((a) => [a.id, a]));
  const nameOf = (id: string): StatusPart => {
    const account = byId.get(id);
    return account ? { accountId: account.id, label: account.label } : { text: id };
  };
  const say = (parts: StatusPart[]): PoolStatusSentence => ({ parts, status, word });
  if (status === 'disabled') return say([{ text: 'the pool accepts no requests; its keys and members stay.' }]);
  if (status === 'exhausted') {
    if (accounts === null) return say([{ text: 'no member can take a request.' }]);
    const first = pool.members
      .map((m) => returnOf(pool, m, nameOf(m.accountId), byId.get(m.accountId), now))
      .filter((r): r is { at: number; parts: StatusPart[] } => r !== null)
      .sort((a, b) => a.at - b.at)[0];
    if (!first) return say([{ text: 'no member can take a request, and none reports when it returns.' }]);
    return say([{ text: 'no member can take a request until ' }, ...first.parts, { text: '.' }]);
  }
  const members = pool.members.length;
  const eligible = pool.members.filter((m) => m.eligible === 'eligible');
  if (members === 0) return say([{ text: 'the pool has no members yet.' }]);
  if (eligible.length === 0) return say([{ text: 'no member takes a fresh session right now.' }]);
  const count = eligible.length === members ? `all ${members} members take fresh sessions` : `${eligible.length} of ${members} members ${eligible.length === 1 ? 'takes' : 'take'} fresh sessions`;
  if (accounts === null) return say([{ text: members === 1 ? "the pool's one member takes every fresh session." : `${count}.` }]);
  if (members === 1) return say([nameOf(eligible[0].accountId), { text: ' takes every fresh session.' }]);
  const next = selectionOrder(pool, accounts, sessions, now).find((tier) => tier.next.length > 0)?.next ?? [];
  if (next.length === 0) return say([{ text: 'no listed member takes fresh sessions.' }]);
  return say([{ text: `${count}; the next one goes to ` }, ...listOf(next.map(nameOf)), { text: '.' }]);
}
