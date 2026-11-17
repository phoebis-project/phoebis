import type { Account, AccountSlowDown, MemberEligibility, MemberFit, Pool, PoolMember, PoolStrategy } from '@/api/types';
import { MEMBER_FIT_VALUES } from '@/api/types';
import { sentences } from './analytics';
import { fmtAbsolute, fmtDuration, fmtPct, fmtPeriodEnd, fmtRelative, fmtShortTime, instant, noun, sameLocalDay } from './format';
import { endOf, isPaidSource, minuteSpan, planExtraUsage, planOverage, slowedOf } from './quota';
import { MINUTE } from './time';

export const STRATEGY_LABEL: Record<PoolStrategy, string> = { pace: 'Pace', priority: 'Priority, then random', round_robin: 'Round robin' };
/** The kind tiers, in the order selection walks them. The third holds the API-key and the usage-based accounts, the
 *  sources metered in dollars. */
export const GROUP_LABEL = ['plan', 'overage', 'metered'] as const;

/** Whether selection walks the kind tiers for this pool: the operator's answer alone. */
export const kindOrdered = (pool: Pool) => pool.preferSubscription;
export const RULE_NOTE = 'A request for a Fable model is placed on the Fable window instead, which this card does not draw.';

/** Why the router skips a member: its own verdict, as the pool view reports it. */
export type Ineligible = Exclude<MemberEligibility, 'eligible'>;
export const INELIGIBLE_LABEL: Record<Ineligible, string> = { budget: 'at budget', budget_unknown: 'budget unknown', cooldown: 'in cooldown', overage: 'in overage', spent: 'spend cap reached', subscription_ended: 'subscription ended', unavailable: 'not available', unknown: 'unknown' };

/** Why an eligible member takes a fresh session only once no better member can: the word of its fit. */
export type LastResort = Exclude<MemberFit, 'fits'>;
export const LAST_RESORT_LABEL: Record<LastResort, string> = { ending: 'winding down', least_bad: 'least bad', rewrite_only: 'rewrite only', spent: 'window spent' };

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
 * A badge that holds until a known reset: the time of the reset, and its date too when the reset falls on another day
 * than `now` in the viewer's zone, so a reset days out never reads as today's.
 */
function untilResetBadge(word: string, sentence: string, reset: string, now: number): StandingBadge {
  return { text: `${word} until ${sameLocalDay(reset, now) ? fmtShortTime(reset) : fmtPeriodEnd(reset)}`, title: `${sentence} until ${fmtAbsolute(reset)}` };
}

/** An overage verdict's badge, until the overage resets where the account knows when. */
export function overageBadge(reset: string | null, now: number): StandingBadge {
  if (reset === null) return { text: 'overage', title: 'In overage, reset unknown' };
  return untilResetBadge('overage', 'In overage', reset, now);
}

/** A spent verdict's badge, until the spend cap resets where the account knows when. */
export function spentBadge(reset: string | null, now: number): StandingBadge {
  if (reset === null) return { text: 'spend cap reached', title: 'At the spend cap, reset unknown' };
  return untilResetBadge('spend cap', 'At the spend cap', reset, now);
}

/** A winding-down member's badge: the time left before the plan's end, which the member is held back for. */
export function windingDownBadge(end: number, now: number): StandingBadge {
  const at = fmtAbsolute(end);
  return end > now ? { text: `winding down ${fmtDuration(end - now)}`, title: `Winding down to the plan's end at ${at}` } : { text: 'winding down', title: `Winding down past the plan's end at ${at}` };
}

/**
 * A speed factor as its chip reads it: `0.5x`, `0.35x`, `0.95x`, two decimals with a trailing zero dropped. A factor
 * under 1 reads `0.99x` at most, since the chip draws under 1 alone and a rounded `1x` would read as no slow-down.
 */
export const speedWord = (factor: number): string => `${factor < 1 ? Math.min(Number(factor.toFixed(2)), 0.99) : Number(factor.toFixed(2))}x`;

/**
 * The ramp's word: how far the span has ramped toward its factor, or that it has. A ramp under 1 reads 99% at most,
 * since a rounded `100%` would read as a ramp that is over.
 */
const rampWords = (ramp: number, factor: number): string => (ramp < 1 ? `Ramping, ${fmtPct(Math.min(ramp, 0.99))} of the way to ${speedWord(factor)}` : `Ramped to ${speedWord(factor)}`);

/**
 * `slowed since 17:40: the sessions on this account would pass the ceiling 42 min before the reset, over the 30 min
 * threshold`, or `under the 30 min threshold` while no round can fire, then one sentence each for the factor the pool
 * computed, the ramp where the entry is inside a span, and the speed the holds realized. With `since` null the words
 * start at `the sessions on this account`. Null where `slowDown` is null or the speed its holds realized is 1 or more.
 */
export function speedHover(slowDown: AccountSlowDown | null, now: number): string | null {
  if (slowDown === null || slowDown.speed >= 1) return null;
  // The round fires on a stretch above the threshold, so one at it reads under.
  const side = slowDown.stretchMs > slowDown.thresholdMs ? 'over' : 'under';
  const words = `the sessions on this account would pass the ceiling ${minuteSpan(slowDown.stretchMs)} before the reset, ${side} the ${minuteSpan(slowDown.thresholdMs)} threshold`;
  const first = slowDown.since === null ? words : `slowed since ${instant(Date.parse(slowDown.since), now)}: ${words}`;
  return sentences([first, `The pool computed ${speedWord(slowDown.factor)}`, slowDown.ramp === null ? null : rampWords(slowDown.ramp, slowDown.factor), `The holds realized ${speedWord(slowDown.speed)}`]);
}

/** The speed chip of a slow-down: the speed the holds realized with `speedHover`'s words, nothing without one. */
export const slowDownChip = (slowDown: AccountSlowDown | null, now: number): { hover: string | null; speed: number | null } =>
  slowDown === null ? { hover: null, speed: null } : { hover: speedHover(slowDown, now), speed: slowDown.speed };

/**
 * A serving member's speed chip: its account's slow-down with the hover where the account is loaded, else the speed
 * the holds realized as the pools route serves it on the member, with no hover. A loaded account draws none, whatever
 * the member carries, where `slowedOf` reads no slow-down: none in force, a realized speed of 1, no 5-hour window at
 * all, or a 5-hour window reset since the read.
 */
export function memberSpeed(member: Pick<PoolMember, 'speed'>, account: Account | null, now: number): { hover: string | null; speed: number | null } {
  if (account === null) return { hover: null, speed: member.speed };
  return slowDownChip(slowedOf(account, now), now);
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

/** Whether a member is inside the pool's wind-down before its window's end, which a member with no end never is. */
export const windingDown = (pool: Pick<Pool, 'windDownMinutes'>, end: number | null, now: number): boolean => end !== null && now >= end - pool.windDownMinutes * MINUTE;

/**
 * Where a member falls in the router's ladder for a fresh session, lowest first: its fit's rank, a member the router
 * answers no fit for among the fitting ones, and every word past `fits` its last resort. The router lands on the best
 * rung before it picks a tier, so a member trails every better member of every tier.
 */
function resortOf({ fit }: Pick<PoolMember, 'fit'>): { lastResort: LastResort | null; order: number } {
  return fit === null || fit === 'fits' ? { lastResort: null, order: 0 } : { lastResort: fit, order: MEMBER_FIT_VALUES.indexOf(fit) };
}

/**
 * The runway a placement ranks by on the rungs of `rewrite_only` and `least_bad`, an unbounded one the longest of all,
 * and 0 on every other rung, where the strategy alone orders the members.
 */
function runwayOf(placement: Placement, member: Pick<PoolMember, 'runwayMs'> | undefined): number {
  if (placement.lastResort !== 'rewrite_only' && placement.lastResort !== 'least_bad') return 0;
  return member?.runwayMs ?? Number.POSITIVE_INFINITY;
}

/** A kind tier by `GROUP_LABEL`'s order: a paid source files with the metered sources whatever its overage reads. */
export const kindGroup = (paid: boolean, overage: boolean): 0 | 1 | 2 => (paid ? 2 : overage ? 1 : 0);

/**
 * Which kind tier an account sits in. A member the accounts list does not carry has no kind, so it leads, and a
 * usage-based seat is a paid source, so it files with the API keys whatever its extra usage flags read.
 */
const groupOf = (account: Account | undefined): 0 | 1 | 2 => (account === undefined ? 0 : kindGroup(isPaidSource(account), account.extraUsage?.inUse === true));

/**
 * The order the router walks for a fresh session of this pool, from the data the routes already serve. `next` is the
 * lead of the first tier that holds a member of the best rung any eligible member reaches, and on a rung that ranks by
 * the runway, the lead among the members with the longest runway over the whole pool.
 */
export function selectionOrder(pool: Pool, accounts: Account[], now: number = Date.now()): Tier[] {
  const byId = new Map(accounts.map((a) => [a.id, a]));
  const memberOf = new Map(pool.members.map((m) => [m.accountId, m]));
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
        const { lastResort, order } = resortOf(m);
        if (ineligible === null) resort.set(account.id, order);
        return [
          {
            account,
            // Under the kind order an account in overage already has a tier of its own, so deferring it inside
            // one would rank it behind its own siblings twice.
            deferred: !grouped && pool.overagePolicy === 'prefer_plan' && Boolean(planExtraUsage(account)?.inUse),
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
  // The rung the router settles on is the best one any eligible member of the pool reaches.
  const settled = Math.min(...resort.values());
  const orderOf = (p: Placement) => resort.get(p.account.id) ?? 0;
  const runwayFor = (p: Placement) => runwayOf(p, memberOf.get(p.account.id));
  // On a rung that ranks by the runway the router keeps the longest one over the whole pool before it picks a tier.
  // Every other rung reads 0, so its first tier names `next` as the tier walk finds it.
  const longest = Math.max(
    ...built
      .flatMap((t) => t.placements)
      .filter((p) => p.ineligible === null && orderOf(p) === settled)
      .map(runwayFor),
  );
  const keyFor = (p: Placement) => keyOf(memberOf.get(p.account.id), pool.strategy);
  let nextFound = false;
  return built.map(({ group, placements, priority }) => {
    // The rung leads the sort, since the router lands on the best rung before it picks a tier, and on the rungs of
    // `rewrite_only` and `least_bad` the longer runway follows it. Inside that the router's tier is (priority,
    // deferred): plan-funded members rank ahead of deferred ones, then the strategy orders inside each half, and equal
    // scores share a rank because the router draws among them at random.
    const eligible = placements
      .filter((p) => p.ineligible === null)
      .sort(
        (a, b) =>
          orderOf(a) - orderOf(b) ||
          runwayFor(b) - runwayFor(a) ||
          Number(a.deferred) - Number(b.deferred) ||
          keyFor(a) - keyFor(b) ||
          a.account.label.localeCompare(b.account.label),
      );
    let rank = 0;
    let previous: { deferred: boolean; order: number; runway: number; score: number } | null = null;
    for (const p of eligible) {
      const score = keyFor(p);
      const runway = runwayFor(p);
      if (!previous || previous.order !== orderOf(p) || previous.runway !== runway || previous.deferred !== p.deferred || previous.score !== score) rank += 1;
      // Round robin and priority both score every member 0, so the rank moves across a rung's boundary, a runway's and
      // the deferred one alone: each member of a subtier is equally the next, and the router rotates the plan-funded
      // subtier on its own while it lasts, so an overage sibling ranks behind it rather than beside it.
      p.rank = rank;
      previous = { deferred: p.deferred, order: orderOf(p), runway, score };
    }
    const ordered = [...eligible, ...placements.filter((p) => p.ineligible !== null).sort((a, b) => a.account.label.localeCompare(b.account.label))];
    const lead = eligible.find((p) => orderOf(p) === settled && runwayFor(p) === longest)?.rank;
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
 * verdict names the first blocker alone (unavailable, cooldown, budget unknown, budget, spent, overage), while
 * `budgetUntil` is set whenever a budget holds the member, and under a pool's `never` overage policy the account's
 * overage holds it until its window resets. So a member in cooldown can sit at its budget and in overage too, and it
 * returns at the latest of its budget's period end (daily, monthly, or plain "budget" when no spend report names the
 * period), its latest account-scope cooldown end and its overage window's reset, which an overage verdict returns at
 * too. A spent verdict returns at its spend cap's reset. A blocker without an end (a budget the degraded ledger leaves
 * unknown, a cooldown without an account-scope row, an overage window or a spend cap without a reset, or an unavailable
 * member) gives no return, and so does a budget verdict without an end.
 */
function returnOf(pool: Pool, member: PoolMember, name: StatusPart, account: Account | undefined, now: number): { at: number; parts: StatusPart[] } | null {
  const endAt = (reset: string, words: string) => ({ at: Date.parse(reset), parts: [name, { text: `'s ${words} ${fmtRelative(reset, now)}` }] });
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
    ends.push(endAt(until, 'cooldown ends'));
  } else if (member.eligible === 'overage') {
    const reset = account?.overage?.reset ?? null;
    if (reset === null) return null;
    ends.push(endAt(reset, 'overage window resets'));
  } else if (member.eligible === 'spent') {
    const reset = account?.spend?.resetsAt ?? null;
    if (reset === null) return null;
    ends.push(endAt(reset, 'spend cap resets'));
  } else if (member.eligible !== 'budget') {
    return null;
  }
  // The router records no overage evidence for a usage-based seat, so the policy never holds one.
  const overage = account === undefined ? null : planOverage(account);
  if (ends.length > 0 && pool.overagePolicy === 'never' && overage?.inUse) {
    const { reset } = overage;
    if (reset === null) return null;
    ends.push(endAt(reset, 'overage window resets'));
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
  const count = eligible.length === members ? `all ${members} members take fresh sessions` : `${eligible.length} of ${members} members ${noun(eligible.length, 'takes', 'take')} fresh sessions`;
  if (accounts === null) return say([{ text: members === 1 ? "the pool's one member takes every fresh session." : `${count}.` }]);
  if (members === 1) return say([nameOf(eligible[0].accountId), { text: ' takes every fresh session.' }]);
  const next = selectionOrder(pool, accounts, now).find((tier) => tier.next.length > 0)?.next ?? [];
  if (next.length === 0) return say([{ text: 'no listed member takes fresh sessions.' }]);
  return say([{ text: `${count}, and the next one goes to ` }, ...listOf(next.map(nameOf)), { text: '.' }]);
}
