import type { Account, AccountSlowDown, MemberEligibility, Pool, PoolMember, PoolStrategy } from '@/api/types';
import { fmtAbsolute, fmtDuration, fmtPeriodEnd, fmtRelative, fmtShortTime, instant } from './format';
import { endOf, minuteSpan, slowedOf } from './quota';

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
 * Why an eligible member takes a fresh session only once no other member can: its subscription end is inside the
 * pool's wind-down, under every strategy.
 */
export type LastResort = 'winding_down';
export const LAST_RESORT_LABEL: Record<LastResort, string> = { winding_down: 'winding down' };

/**
 * `deferred`: eligible but in overage under prefer_plan, so ranked after the plan-funded members of its tier. `end`:
 * the end an engine rule reads off the account, which a winding-down badge counts down to.
 */
export interface Placement {
  account: Account;
  deferred: boolean;
  end: number | null;
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

/** A Standing badge's words and the sentence its title carries. */
export interface StandingBadge {
  text: string;
  title: string | undefined;
}

/** A cooldown verdict's badge: the time left on the account's own cooldown where the account names its end. */
export function cooldownBadge(until: string | undefined, now: number): StandingBadge {
  if (until === undefined) return { text: 'cooldown', title: undefined };
  const left = Date.parse(until) - now;
  return left > 0 ? { text: `cooldown ${fmtDuration(left)}`, title: `In a cooldown that ends ${fmtAbsolute(until)}` } : { text: 'cooldown', title: `In a cooldown whose end passed at ${fmtAbsolute(until)}` };
}

/**
 * An overage verdict's badge: the time the overage resets at where the account knows it, and its date too when the
 * reset falls on another day than `now` in the viewer's zone, so a reset days out never reads as today's.
 */
export function overageBadge(reset: string | null, now: number): StandingBadge {
  if (reset === null) return { text: 'overage', title: 'In overage, reset unknown' };
  const sameDay = new Date(reset).toDateString() === new Date(now).toDateString();
  return { text: `overage until ${sameDay ? fmtShortTime(reset) : fmtPeriodEnd(reset)}`, title: `In overage until ${fmtAbsolute(reset)}` };
}

/** A winding-down member's badge: the time left before the plan's end, which the member is held back for. */
export function windingDownBadge(end: number, now: number): StandingBadge {
  const at = fmtAbsolute(new Date(end).toISOString());
  return end > now ? { text: `winding down ${fmtDuration(end - now)}`, title: `Winding down to the plan's end at ${at}` } : { text: 'winding down', title: `Winding down past the plan's end at ${at}` };
}

/** A speed factor as its chip reads it: `0.5x`, `0.35x`, `0.95x`, two decimals with a trailing zero dropped. */
export const speedWord = (factor: number): string => `${Number(factor.toFixed(2))}x`;

/**
 * `slowed since 17:40: the sessions on this account would pass the ceiling 42 min before the reset, over the 30 min
 * threshold`, or `under the 30 min threshold` while no round can fire. With `since` null the words start at `the
 * sessions on this account`. Null where `slowDown` is null or its factor 1.
 */
export function speedHover(slowDown: AccountSlowDown | null, now: number): string | null {
  if (slowDown === null || slowDown.factor >= 1) return null;
  // The round fires on a stretch above the threshold, so one at it reads under.
  const side = slowDown.stretchMs > slowDown.thresholdMs ? 'over' : 'under';
  const words = `the sessions on this account would pass the ceiling ${minuteSpan(slowDown.stretchMs)} before the reset, ${side} the ${minuteSpan(slowDown.thresholdMs)} threshold`;
  return slowDown.since === null ? words : `slowed since ${instant(Date.parse(slowDown.since), now)}: ${words}`;
}

/**
 * A serving member's speed chip: its account's slow-down with the hover where the account is loaded, else the factor
 * the pools route serves on the member with no hover. A loaded account draws none, whatever the member carries, where
 * `slowedOf` reads no slow-down: none in force, a factor of 1, or a 5-hour window reset since the read.
 */
export function memberSpeed(member: Pick<PoolMember, 'speed'>, account: Account | null, now: number): { factor: number | null; hover: string | null } {
  if (account === null) return { factor: member.speed, hover: null };
  const slowDown = slowedOf(account, now);
  return slowDown === null ? { factor: null, hover: null } : { factor: slowDown.factor, hover: speedHover(slowDown, now) };
}

/**
 * The router's Pace score as the pools route serves it, whatever the member's kind: the spare rate for a subscription
 * member, the remaining worst bucket for an API-key member. A member the router has not read carries none and ranks
 * as untouched, ahead of every scored member, as an unsampled account ranks in the router.
 */
function paceScore(member: PoolMember | undefined): number {
  return member?.paceScore ?? Number.POSITIVE_INFINITY;
}

/** The sort key the strategy ranks an eligible member by, lower first. `priority` scores nothing. */
function keyOf(member: PoolMember | undefined, strategy: PoolStrategy): number {
  if (strategy === 'pace') return -paceScore(member);
  return 0;
}

/**
 * Where a member falls in the router's fall-through for a fresh session, lowest first: not winding down, then winding
 * down. The router drops the group from the whole selection set before it picks a tier, so it trails every better
 * member of every tier.
 */
function resortOf(pool: Pool, account: Account, now: number): { lastResort: LastResort | null; order: number } {
  const end = endOf(account, now);
  const windingDown = end !== null && now >= end - pool.windDownMinutes * 60_000;
  return { lastResort: windingDown ? 'winding_down' : null, order: windingDown ? 2 : 0 };
}

/** Which kind tier an account sits in; a member the accounts list does not carry has no kind, so it leads. */
const groupOf = (account: Account | undefined): 0 | 1 | 2 => (account?.kind === 'api_key' ? 2 : account?.extraUsage?.inUse ? 1 : 0);

/** The order the router walks for a fresh session of this pool, from the data the routes already serve. */
export function selectionOrder(pool: Pool, accounts: Account[], now: number = Date.now()): Tier[] {
  const byId = new Map(accounts.map((a) => [a.id, a]));
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
            end: endOf(account, now),
            ineligible,
            lastResort: ineligible === null ? lastResort : null,
            // The engine's count, one pin per agent and model, so a session whose subagents run on another model
            // counts more than once, as the pool's Active pins does.
            pins: m.activePins,
            rank: null,
          },
        ];
      });
    return { group, placements, priority };
  });
  // The group the router settles in is the best one any eligible member of the pool reaches.
  const settled = Math.min(...resort.values());
  const orderOf = (p: Placement) => resort.get(p.account.id) ?? 0;
  const memberOf = new Map(pool.members.map((m) => [m.accountId, m]));
  const keyFor = (p: Placement) => keyOf(memberOf.get(p.account.id), pool.strategy);
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
          keyFor(a) - keyFor(b) ||
          a.account.label.localeCompare(b.account.label),
      );
    let rank = 0;
    let previous: { deferred: boolean; order: number; score: number } | null = null;
    for (const p of eligible) {
      const score = keyFor(p);
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
 * account-scope cooldown end and its overage window's reset, which an overage verdict returns at too. A blocker
 * without an end (unavailable, a cooldown without an account-scope row, an overage window without a reset) gives no
 * return, and so does a budget verdict without an end.
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
  } else if (member.eligible === 'overage') {
    const reset = account?.overage?.reset ?? null;
    if (reset === null) return null;
    ends.push({ at: Date.parse(reset), parts: [name, { text: `'s overage window resets ${fmtRelative(reset, now)}` }] });
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
export function poolStatusSentence(pool: Pool, accounts: Account[] | null, now: number = Date.now()): PoolStatusSentence {
  const { status } = pool;
  const word = STATUS_WORD[status];
  const byId = new Map((accounts ?? []).map((a) => [a.id, a]));
  const nameOf = (id: string): StatusPart => {
    const account = byId.get(id);
    return account ? { accountId: account.id, label: account.label } : { text: id };
  };
  const say = (parts: StatusPart[]): PoolStatusSentence => ({ parts, status, word });
  if (status === 'disabled') return say([{ text: 'the pool accepts no requests, but its keys and members stay.' }]);
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
  const next = selectionOrder(pool, accounts, now).find((tier) => tier.next.length > 0)?.next ?? [];
  if (next.length === 0) return say([{ text: 'no listed member takes fresh sessions.' }]);
  return say([{ text: `${count}, and the next one goes to ` }, ...listOf(next.map(nameOf)), { text: '.' }]);
}
