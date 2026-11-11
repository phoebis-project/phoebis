import type { AttentionItem, ResetGrantState, ResetOutcome } from '@/api/types';
import { lookup } from '@/lib/counts';
import { DAY, HOUR, MINUTE, NOW, ago, ahead, iso } from './time';

/** A grant as the upstream's block lists it, a member the router reads as its default where absent left optional. */
export interface MockGrant {
  blocking?: string[];
  clears: string[];
  ends_at: string;
  gives_back_weekly?: boolean;
  id: string;
  label: string;
  paused?: boolean;
  resets_left: number;
  resets_total: number;
  starts_at: string;
  type?: string;
  unavailable_reason?: string | null;
  usable_now: boolean;
}

/**
 * One seed's block in the upstream's field names, beside two members the block itself does not carry: the Fable
 * week's entry of the usage body's `weekly_scoped_shares`, and `uses`, the instant of each use through the router by
 * grant id, which the router's ledger holds since the upstream's block names the time of none.
 */
export interface MockBlock {
  cooldown_until: string | null;
  eligible: boolean;
  grants: readonly MockGrant[];
  ineligible_reason: string | null;
  /** The Fable week's `used_percent_of_weekly`, null where the body carries no share. */
  used_percent_of_weekly: number | null;
  uses: Readonly<Record<string, number>>;
}

/** The route's 200 for a claim in its own names and types, without the member it builds after the claim. */
export interface MockUse {
  cleared: string[];
  cooldown_until: number | null;
  outcome: ResetOutcome;
  reason: string | null;
  resets_left: number | null;
  result: string;
  weekly_resets_at: number | null;
}

/** A use's answer, or the route's refusal beside the status the upstream answered, which the use's ledger row keeps. */
export type MockAnswer = { answer: MockUse } | { refusal: { code: string; message: string; status: number }; upstream: number };

/** The router's name for a window a grant names. */
const routerWindow = (name: string): string => (name === 'seven_day_overage_included' ? 'seven_day_fable' : name);

/** The client's kind, as the router sets it at the parse. */
function kindOf(g: MockGrant): string {
  if (g.gives_back_weekly) return 'fable';
  if (g.type !== undefined) return g.type;
  if (!g.clears.includes('five_hour')) return 'weekly';
  return g.clears.length === 1 ? 'session' : 'full';
}

/** One promotion gave every grant of the story, so they share its label and its start. */
const PROMOTION = { label: 'Launch reset', starts_at: ago(10 * DAY) } as const;
/** A usage-based seat's block: no grant, and the reason the upstream gives for one. */
const SEAT: MockBlock = { cooldown_until: null, eligible: false, grants: [], ineligible_reason: 'seat', used_percent_of_weekly: null, uses: {} };
const ELIGIBLE = { cooldown_until: null, eligible: true, ineligible_reason: null, used_percent_of_weekly: null, uses: {} } as const;

/** One seed's block as the upstream's usage body carries it, in the upstream's field names. */
export const RESET_BLOCKS: Readonly<Record<string, MockBlock>> = {
  contract: SEAT,
  // Inside the warning's five days, so the Dashboard warns about it and its item dates from 12 hours back.
  nightly: { ...ELIGIBLE, grants: [{ ...PROMOTION, clears: ['five_hour', 'seven_day_overage_included'], ends_at: ahead(4 * DAY + 12 * HOUR), id: 'rg-nightly-1', resets_left: 1, resets_total: 1, usable_now: true }] },
  // The upstream answers both grants usable, and one kind offers one grant at a time, so the second waits.
  primary: {
    ...ELIGIBLE,
    grants: [
      { ...PROMOTION, clears: ['seven_day_overage_included'], ends_at: ahead(118 * DAY), gives_back_weekly: true, id: 'rg-primary-1', resets_left: 1, resets_total: 1, usable_now: true },
      { ...PROMOTION, clears: ['seven_day_overage_included'], ends_at: ahead(118 * DAY), gives_back_weekly: true, id: 'rg-primary-2', resets_left: 1, resets_total: 1, usable_now: true },
    ],
    used_percent_of_weekly: 6,
  },
  reserve: { ...ELIGIBLE, eligible: false, grants: [], ineligible_reason: 'tier' },
  secondary: { ...ELIGIBLE, grants: [{ ...PROMOTION, clears: ['five_hour'], ends_at: ahead(20 * DAY), id: 'rg-secondary-1', resets_left: 1, resets_total: 1, type: 'session', usable_now: true }] },
  // Used through the router 3 hours back. The upstream keeps a used grant listed, with none left and no time.
  studio: {
    ...ELIGIBLE,
    grants: [{ ...PROMOTION, clears: ['seven_day_overage_included'], ends_at: ahead(20 * DAY), id: 'rg-studio-1', resets_left: 0, resets_total: 1, unavailable_reason: 'unavailable', usable_now: false }],
    uses: { 'rg-studio-1': NOW - 3 * HOUR },
  },
  // Ends outside the warning's days, so its cooldown stands with no warning beside it.
  workshop: {
    ...ELIGIBLE,
    cooldown_until: ahead(40 * MINUTE),
    grants: [{ ...PROMOTION, clears: ['seven_day_overage_included'], ends_at: ahead(20 * DAY), id: 'rg-workshop-1', resets_left: 1, resets_total: 1, unavailable_reason: 'cooldown', usable_now: false }],
  },
  zenith: SEAT,
};

/** Each grant's state at `now` by the router's rule, so a mock account reads its resets as a real one does. */
export function grantStates(block: MockBlock, now: number): { reason: string | null; state: ResetGrantState }[] {
  const cooldown = block.cooldown_until === null ? null : Date.parse(block.cooldown_until);
  const openKinds: string[] = [];
  const plain = (state: ResetGrantState) => ({ reason: null, state });
  return block.grants.map((g) => {
    if (g.resets_left === 0) return plain('used');
    if (Date.parse(g.ends_at) <= now) return plain('ended');
    if (g.paused) return plain('paused');
    // The block's cooldown names no grant, and a claim's answer writes it before the next read.
    if (cooldown !== null && cooldown > now) return plain('cooldown');
    const reason = g.unavailable_reason ?? null;
    // The clock tells that a cooldown ended and nothing about a limit or a block.
    const freed = cooldown !== null && (reason === 'cooldown' || (reason === null && (g.blocking ?? []).length === 0));
    if (g.usable_now || freed) {
      const kind = kindOf(g);
      if (openKinds.includes(kind)) return plain('waiting');
      openKinds.push(kind);
      return plain('usable');
    }
    return reason === 'cooldown' ? plain('cooldown') : { reason, state: 'blocked' };
  });
}

/** The count every reader shows: an ended grant's resets are lost, and every other grant counts whatever its state. */
const resetsLeft = (block: MockBlock, now: number): number => block.grants.filter((g) => Date.parse(g.ends_at) > now).reduce((sum, g) => sum + g.resets_left, 0);

/** The last read of every block that answered 200. */
const READ_AT = NOW - 20 * MINUTE;

/** A seed's `limit_resets` member in the route's own names, null for a seed with no block. */
export function limitResetsWire(seed: string, blocks: Readonly<Record<string, MockBlock>>, now: number): Record<string, unknown> | null {
  const block = lookup(blocks, seed);
  if (block === undefined) return null;
  const states = grantStates(block, now);
  const cooldown = block.cooldown_until === null ? null : Date.parse(block.cooldown_until);
  return {
    cooldown_until: cooldown !== null && cooldown > now ? cooldown : null,
    eligible: block.eligible,
    fable_week_share: block.used_percent_of_weekly === null ? null : block.used_percent_of_weekly / 100,
    grants: block.grants.map((g, i) => ({
      blocking: (g.blocking ?? []).map(routerWindow),
      clears: g.clears.map(routerWindow),
      ends_at: Date.parse(g.ends_at),
      id: g.id,
      kind: kindOf(g),
      label: g.label,
      reason: states[i].reason,
      resets_left: g.resets_left,
      resets_total: g.resets_total,
      starts_at: Date.parse(g.starts_at),
      state: states[i].state,
      used_at: g.resets_left === 0 ? (lookup(block.uses, g.id) ?? null) : null,
    })),
    ineligible_reason: block.ineligible_reason,
    left: resetsLeft(block, now),
    read_at: READ_AT,
  };
}

/** A reset of a grant that clears the Fable week: none left of one, and the week cleared. */
const FABLE_RESET: MockAnswer = { answer: { cleared: ['seven_day_fable'], cooldown_until: null, outcome: 'used', reason: null, resets_left: 0, result: 'reset', weekly_resets_at: null } };

/** What a use of each grant answers: the route's 200 in its own names, or the refusal the call throws. */
export const RESET_ANSWERS: Readonly<Record<string, MockAnswer>> = {
  // A cooldown on a first attempt is a refusal, whose instant the router folds into the block.
  'rg-nightly-1': { answer: { cleared: [], cooldown_until: NOW + 25 * MINUTE, outcome: 'refused', reason: null, resets_left: null, result: 'cooldown', weekly_resets_at: null } },
  'rg-primary-1': FABLE_RESET,
  'rg-primary-2': FABLE_RESET,
  // A status other than 401, 403 or 429 leaves the claim unsettled.
  'rg-secondary-1': { refusal: { code: 'limit_reset_unanswered', message: 'upstream: HTTP 503', status: 502 }, upstream: 503 },
  // The grant is usable once the block's cooldown passed, 40 minutes after the story's clock.
  'rg-workshop-1': FABLE_RESET,
};

/** The block a claim's answer leaves, folded as the router's store folds it, the use stamped at `at` where it was used. */
export function foldAnswer(block: MockBlock, grantId: string, answer: MockUse, at: number): MockBlock {
  const count = (g: MockGrant): number => answer.resets_left ?? (answer.result === 'reset' ? Math.max(0, g.resets_left - 1) : answer.result === 'already_used' ? 0 : g.resets_left);
  const used = answer.outcome === 'used';
  // The share was the Fable week's part of the 7-day window, and a use that gives the week back spends it.
  const givesBack = used && block.grants.some((g) => g.id === grantId && g.gives_back_weekly);
  return {
    ...block,
    cooldown_until: answer.cooldown_until === null ? block.cooldown_until : iso(answer.cooldown_until),
    grants: block.grants.map((g) => (g.id === grantId ? { ...g, resets_left: count(g) } : g)),
    used_percent_of_weekly: givesBack && block.used_percent_of_weekly !== null ? 0 : block.used_percent_of_weekly,
    uses: used ? { ...block.uses, [grantId]: at } : block.uses,
  };
}

/**
 * The reset item of an account whose block holds an unused reset that ends within `days`, as the reader builds it: about
 * the grant that ends first, dated from its end less the days and never before its start. Null where none does.
 */
export function resetEnding(account: string, block: MockBlock | undefined, now: number, days: number): Pick<AttentionItem, 'detail' | 'since'> | null {
  if (block === undefined) return null;
  const ending = block.grants.filter((g) => g.resets_left > 0 && Date.parse(g.ends_at) > now && Date.parse(g.ends_at) <= now + days * DAY);
  // The first of several that end together, as the block lists them.
  const first = ending.reduce<MockGrant | null>((a, g) => (a === null || Date.parse(g.ends_at) < Date.parse(a.ends_at) ? g : a), null);
  if (first === null) return null;
  const end = Date.parse(first.ends_at);
  return {
    detail: {
      account,
      clears: first.clears.map(routerWindow),
      days_left: Math.floor((end - now) / DAY),
      ends_at: end,
      ending: ending.reduce((sum, g) => sum + g.resets_left, 0),
      grant: first.id,
      grant_label: first.label,
      resets_left: resetsLeft(block, now),
    },
    since: iso(Math.max(Date.parse(first.starts_at), end - days * DAY)),
  };
}

/**
 * The wall's `reset` member as the reader joins it: the first grant in the block's order that is usable now and clears
 * a window of `atLimit`, the router's names of the windows the reading holds at their limit. Null where none does.
 */
export function wallReset(block: MockBlock | undefined, atLimit: readonly string[], now: number): { clears: string[]; grant: string; label: string } | null {
  if (block === undefined) return null;
  const states = grantStates(block, now);
  for (const [i, g] of block.grants.entries()) {
    const clears = g.clears.map(routerWindow).filter((w) => atLimit.includes(w));
    if (states[i].state === 'usable' && clears.length > 0) return { clears, grant: g.id, label: g.label };
  }
  return null;
}
