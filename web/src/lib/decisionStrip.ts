import type { DecisionKind, DecisionRecord, WalkStep } from '@/api/types';
import { lookup } from '@/lib/counts';
import { joinPresent } from '@/lib/format';
import { candidateVerdict, DECISION_KIND_LABEL, DECISION_KIND_TONE, formatFact, PLACE_WORD, STEP_NAME, strategyWord, tierOf, verdictTone, verdictWord, WAIT_REASON_WORD, wordOf, words } from '@/lib/routingLog';
import type { Chip, VerdictTone } from '@/lib/routingLog';

export type Renderer = 'candidate' | 'chips' | 'hold' | 'list' | 'outcome' | 'rung' | 'sentence' | 'table';
export type StripGroup = 'moves' | 'partitions' | 'rungs';
export type StubWord = 'fires' | 'match' | 'released';
export interface StationKind { group: StripGroup | null; grouped?: boolean; renderer: Renderer; stub: StubWord | null }

/** One entry per step id the engine records, keyed as the fixture keys them. The word is `STEP_NAME`'s. */
export const STATION_KINDS: Record<string, StationKind> = {
  admission: { group: null, renderer: 'list', stub: null },
  bound: { group: null, renderer: 'list', stub: null },
  cap_filter: { group: null, renderer: 'table', stub: null },
  eligible: { group: null, renderer: 'table', stub: null },
  exhausted: { group: null, renderer: 'list', stub: null },
  fit: { group: null, renderer: 'table', stub: null },
  hold: { group: null, renderer: 'hold', stub: null },
  'move:drain': { group: 'moves', renderer: 'list', stub: 'fires' },
  'move:overage': { group: 'moves', renderer: 'list', stub: 'fires' },
  'move:return_to_plan': { group: 'moves', renderer: 'list', stub: 'fires' },
  'move:shed': { group: 'moves', grouped: true, renderer: 'list', stub: 'fires' },
  'move:spent': { group: 'moves', renderer: 'list', stub: 'fires' },
  partitions: { group: 'partitions', renderer: 'list', stub: null },
  pick: { group: null, renderer: 'table', stub: null },
  pin: { group: null, grouped: true, renderer: 'list', stub: null },
  probe: { group: null, renderer: 'list', stub: null },
  renew: { group: null, renderer: 'list', stub: 'released' },
  reserve: { group: null, renderer: 'list', stub: null },
  'rung:content': { group: 'rungs', renderer: 'rung', stub: 'match' },
  'rung:static': { group: 'rungs', renderer: 'rung', stub: 'match' },
  'rung:with_main': { group: 'rungs', renderer: 'rung', stub: 'match' },
  selection: { group: null, renderer: 'sentence', stub: null },
};
/**
 * The stations of a version 1 record, which the engine wrote before the projection: the cool reading where the fit
 * station stands now, and no reservation.
 */
export const LEGACY_STATION_KINDS: Record<string, StationKind> = {
  admission: STATION_KINDS.admission,
  bound: STATION_KINDS.bound,
  cap_filter: STATION_KINDS.cap_filter,
  cool: { group: null, renderer: 'table', stub: null },
  eligible: STATION_KINDS.eligible,
  exhausted: STATION_KINDS.exhausted,
  hold: STATION_KINDS.hold,
  'move:drain': STATION_KINDS['move:drain'],
  'move:overage': STATION_KINDS['move:overage'],
  'move:return_to_plan': STATION_KINDS['move:return_to_plan'],
  'move:shed': STATION_KINDS['move:shed'],
  'move:spent': STATION_KINDS['move:spent'],
  partitions: STATION_KINDS.partitions,
  pick: STATION_KINDS.pick,
  pin: STATION_KINDS.pin,
  probe: STATION_KINDS.probe,
  renew: STATION_KINDS.renew,
  'rung:content': STATION_KINDS['rung:content'],
  'rung:static': STATION_KINDS['rung:static'],
  'rung:with_main': STATION_KINDS['rung:with_main'],
  selection: STATION_KINDS.selection,
};
export const GROUP_WORD: Record<StripGroup, string> = { moves: 'Moves', partitions: 'Partitions', rungs: 'Rungs' };
export const PARTITION_FACTS = ['spent', 'at_cap', 'no_fit', 'winding_down'] as const; // the engine's order
export const LEGACY_PARTITION_FACTS = ['spent', 'at_cap', 'above_ceiling', 'winding_down', 'floor'] as const; // a version 1 record's
export const PARTITION_STATION_WORD: Record<(typeof PARTITION_FACTS)[number] | (typeof LEGACY_PARTITION_FACTS)[number], string> = {
  above_ceiling: 'Above ceiling',
  at_cap: 'At cap',
  floor: 'Floor',
  no_fit: 'No fit',
  spent: 'Spent',
  winding_down: 'Winding down',
};
/** A partition value's badge word. */
export const PARTITION_WORD: Record<'fell_through' | 'held' | 'off', string> = { fell_through: 'fell through', held: 'held', off: 'off' };

/**
 * The strip label of a step station: under a bracket the group's prefix is dropped and the rest lowercased ("content"
 * under Rungs, "spent" under Moves). The popover's head is the station's `head`.
 */
export function stationWord(stepId: string): string {
  const name = lookup(STEP_NAME, stepId);
  if (name === undefined) return stepId;
  // A step both tables hold sits in one group in each, and the cool reading in none.
  const kind = lookup(STATION_KINDS, stepId) ?? lookup(LEGACY_STATION_KINDS, stepId) ?? null;
  const group = kind?.group ?? null;
  // The engine writes all five rules and all three rungs, so they always stand under their bracket, while the one
  // Partitions station of a continuation or a pinned line stands alone.
  return group === 'moves' || group === 'rungs' ? name.replace(/^[^:]*: /, '').toLowerCase() : name;
}

export type PathState = 'idle' | 'inferred' | 'taken';
export type StripShape = 'fresh' | 'pinned' | 'short';
export interface Station {
  /**
   * Unique in the strip: the step id, `partitions:<fact>` for a partition station on a fresh line, `move` for the Move
   * station.
   */
  id: string;
  /** The engine's step id the popover's plan reads: `partitions` for a partition station, the fired rule's for Move. */
  step: string;
  /** The walk's index of the recorded step, null for a placeholder and for Move. */
  index: number | null;
  /** The label on the strip: the short word, lowercased under a bracket ("shed", "content", "At cap"). */
  word: string;
  /**
   * The popover head's word: `STEP_NAME[step]` for a step station ("Shed"), "Move" for the Move station, the
   * partition's own word ("At cap") for a partition station on a fresh line.
   */
  head: string;
  /**
   * The badge's text (`verdictWord`'s, a partition's "2 out", "off", "held", "fell through", "not recorded", "not run",
   * empty under a bracket the record does not carry) and its tone. `verdict` is the raw word, null for a placeholder.
   */
  badge: string;
  tone: VerdictTone;
  verdict: string | null;
  /**
   * The second line of the fan's host, the slot and the Move station, one part per value so no part joins two
   * (["Pace P0"], ["cohort", "bound 1 min"], ["timed"], ["shed rule"]). Empty elsewhere.
   */
  second: string[];
  group: StripGroup | null;
  stub: StubWord | null;
  state: PathState;
  decisive: boolean;
  level: 'line' | 'track';
  renderer: Renderer;
  /** False for a step the table does not know, drawn as its id with the chips renderer. */
  known: boolean;
}
export interface Leaf { accountId: string | null; badge: string; chip: Chip | null; chosen: boolean; more: number; tone: 'accent' | 'neutral' | 'warning' } // `more` > 0 and `accountId` null on the "and N more" leaf
export type TerminusKind = 'chosen' | 'held' | 'held_reset' | 'held_rewrite' | 'held_slow_down' | 'refused';
export const TERMINUS_WORD: Record<TerminusKind, string> = {
  chosen: 'Chosen',
  held: `Held ${WAIT_REASON_WORD.cap}`,
  held_reset: `Held ${WAIT_REASON_WORD.reset}`,
  held_rewrite: `Held ${WAIT_REASON_WORD.rewrite}`,
  held_slow_down: `Held ${WAIT_REASON_WORD.slow_down}`,
  refused: 'Refused',
};
export interface Terminus { badge: string | null; drawn: boolean; kind: TerminusKind; state: PathState; tone: VerdictTone; word: string }
export interface Bracket { group: StripGroup; level: 'line' | 'track'; recorded: boolean; stations: string[]; word: string } // word "Rungs" or "Rungs, not recorded"
export interface Branch { from: string; kind: 'move' | 'rung' }
export interface StripModel { branch: Branch | null; brackets: Bracket[]; decisive: string | null; host: string | null; leaves: Leaf[]; outcome: DecisionKind; shape: StripShape; stations: Station[]; termini: Terminus[] }

/** The engine's order. */
const MOVE_RULES = ['move:spent', 'move:overage', 'move:return_to_plan', 'move:drain', 'move:shed'];
const RUNGS = ['rung:content', 'rung:with_main', 'rung:static'];
/** The steps a pinned walk records when the engine placed the request afresh after its rules. */
const SELECTION_STEPS = new Set(['exhausted', 'fit', 'hold', 'partitions', 'pick', 'reserve', ...RUNGS]);
/** The engine books after every walk it runs, so Reserve stands last, decides nothing and hosts no candidates. */
const RESERVE = 'reserve';
/** Walks that end before any selection: a refusal, a probe answered. */
const SHORT_ENDS = new Set(['admission', 'probe', 'selection']);
const SLOT_OF: Record<DecisionKind, string> = { exhausted: 'exhausted', held: 'hold', moved: 'pick', placed: 'pick', refused: 'exhausted', renewed: 'pick' };
const TERMINUS_OF: Record<DecisionKind, TerminusKind> = { exhausted: 'refused', held: 'held', moved: 'chosen', placed: 'chosen', refused: 'refused', renewed: 'chosen' };
const HELD_FOR: Record<string, TerminusKind> = { reset: 'held_reset', rewrite: 'held_rewrite', slow_down: 'held_slow_down' };
/** The holds with a terminus of their own, read off `HELD_FOR` so a new hold reason draws without a second edit. */
const HELD_TERMINI: ReadonlySet<TerminusKind> = new Set(Object.values(HELD_FOR));
/**
 * The terminus an outcome names: a hold for the window's reset is the fourth way out, a hold for the slow-down the
 * fifth, a hold for the rewrite in flight the sixth, every other hold a slot's.
 */
const terminusOf = (outcome: DecisionRecord['outcome']): TerminusKind => {
  const reason = outcome.kind === 'held' ? outcome.hold?.reason : undefined;
  return (reason === undefined ? undefined : lookup(HELD_FOR, reason)) ?? TERMINUS_OF[outcome.kind];
};
const MAX_LEAVES = 8;
const UNKNOWN_KIND: StationKind = { group: null, renderer: 'chips', stub: null };

const isRule = (step: string) => step.startsWith('move:');

/**
 * The engine never writes `bound` beside a rule, a partition, a rung or a pick: a bound request renews or exhausts on
 * its own path, so a walk that carries it expects no placeholder. A walk's end is its last step ahead of the Reserve
 * the engine writes behind every walk, a probe's pinned answer among them.
 */
function shapeOf(walk: WalkStep[]): StripShape {
  if (walk.some((s) => isRule(s.step))) return 'pinned';
  const last = walk.filter((s) => s.step !== RESERVE).at(-1)?.step;
  return walk.some((s) => s.step === 'bound') || (last !== undefined && SHORT_ENDS.has(last)) ? 'short' : 'fresh';
}

function expectedOf(shape: StripShape, walk: WalkStep[], kind: DecisionKind): string[] {
  if (shape === 'short') return [];
  // A placeholder the walk lacks stands before the Reserve the engine wrote last, never after it.
  const booked = walk.some((s) => s.step === RESERVE) ? [RESERVE] : [];
  if (shape === 'fresh') return ['pin', 'eligible', 'partitions', ...RUNGS, SLOT_OF[kind], ...booked];
  const lastRule = walk.reduce((at, s, i) => (isRule(s.step) ? i : at), -1);
  const fired = walk.some((s) => isRule(s.step) && s.verdict === 'fired');
  // A live pin that turned ineligible runs its rules and is then placed afresh, so the engine renews nothing. Every
  // walk books after it, a held one too, so Reserve says nothing of a selection.
  const selected = !fired && walk.some((s, i) => i > lastRule && s.step !== RESERVE && SELECTION_STEPS.has(s.step));
  return ['pin', 'eligible', ...MOVE_RULES, ...(selected ? [] : ['renew']), ...booked];
}

interface Entry { index: number | null; step: string }

/** The recorded steps in the record's order, each expected step the record lacks placed where the shape expects it. */
function merge(walk: WalkStep[], expected: string[]): Entry[] {
  const recorded = new Set(walk.map((s) => s.step));
  const out: Entry[] = [];
  let next = 0;
  const lacking = (to: number) => {
    for (; next < to; next += 1) if (!recorded.has(expected[next])) out.push({ index: null, step: expected[next] });
  };
  walk.forEach((s, index) => {
    const at = expected.indexOf(s.step, next);
    if (at >= 0) {
      lacking(at);
      next = at + 1;
    }
    out.push({ index, step: s.step });
  });
  lacking(expected.length);
  return out;
}

const factOf = (s: WalkStep, key: string) => s.facts.find((f) => f.key === key)?.value;

function secondOf(s: WalkStep, now: number): string[] {
  switch (s.step) {
    case 'pick': {
      const strategy = factOf(s, 'strategy');
      return typeof strategy === 'string' ? [strategyWord(strategy, tierOf(factOf(s, 'tier')))] : [];
    }
    case 'hold': {
      const place = factOf(s, 'place');
      const bound = factOf(s, 'bound_ms');
      return [...(typeof place === 'string' ? [wordOf(PLACE_WORD, place)] : []), ...(typeof bound === 'number' ? [`bound ${formatFact(bound, 'ms', now)}`] : [])];
    }
    case 'exhausted': {
      const kind = factOf(s, 'kind');
      return typeof kind === 'string' ? [words(kind)] : [];
    }
    default:
      return [];
  }
}

function partitionBadge(s: WalkStep | null, key: string): { badge: string; tone: VerdictTone; verdict: string | null } {
  const value = s === null ? undefined : factOf(s, key);
  if (value === undefined) return { badge: 'not recorded', tone: 'neutral', verdict: null };
  if (typeof value === 'number') return { badge: `${value} out`, tone: value === 0 ? 'good' : 'warning', verdict: String(value) };
  const word = String(value);
  return { badge: wordOf(PARTITION_WORD, word), tone: verdictTone(word, 'partitions'), verdict: word };
}

/** A station with the walk's verdict it reads its state by: the step's own, the Move station's badge word. */
interface Draft { placeholder: boolean; station: Station; stepVerdict: string | null }

const blank = (id: string, step: string, index: number | null, kind: StationKind, known: boolean): Station => ({
  badge: '',
  decisive: false,
  group: kind.group,
  head: step,
  id,
  index,
  known,
  level: 'line',
  renderer: kind.renderer,
  second: [],
  state: 'idle',
  step,
  stub: kind.stub,
  tone: 'neutral',
  verdict: null,
  word: step,
});

/** The station table and the partition facts a record draws through, by the version that wrote it. */
interface Maps { kinds: Record<string, StationKind>; partitions: readonly (keyof typeof PARTITION_STATION_WORD)[] }
const MAPS: Maps = { kinds: STATION_KINDS, partitions: PARTITION_FACTS };
const LEGACY_MAPS: Maps = { kinds: LEGACY_STATION_KINDS, partitions: LEGACY_PARTITION_FACTS };

function draftsOf(walk: WalkStep[], entries: Entry[], shape: StripShape, now: number, maps: Maps): Draft[] {
  const drafts: Draft[] = [];
  const seen = new Set<string>();
  for (const { index, step } of entries) {
    const s = index === null ? null : walk[index];
    if (step === 'partitions' && shape === 'fresh') {
      for (const key of maps.partitions) {
        const station = blank(`partitions:${key}`, step, index, maps.kinds.partitions, true);
        const part = partitionBadge(s, key);
        drafts.push({ placeholder: s === null, station: { ...station, ...part, head: PARTITION_STATION_WORD[key], word: PARTITION_STATION_WORD[key] }, stepVerdict: s?.verdict ?? null });
      }
      continue;
    }
    const kind = lookup(maps.kinds, step);
    const known = kind !== undefined;
    // A step the walk records twice keeps one id per station.
    const id = seen.has(step) ? `${step}#${index}` : step;
    seen.add(id);
    const station = blank(id, step, index, kind ?? UNKNOWN_KIND, known);
    if (known) {
      station.word = stationWord(step);
      station.head = STEP_NAME[step];
    }
    if (s !== null) {
      station.badge = verdictWord(s.verdict);
      station.tone = known ? verdictTone(s.verdict, step) : 'neutral';
      station.verdict = s.verdict;
      station.second = secondOf(s, now);
    }
    drafts.push({ placeholder: s === null, station, stepVerdict: s?.verdict ?? null });
  }
  return drafts;
}

/**
 * The fired rule's continuation moves to the track behind the Move station. The rules it cut short and Renew stay on
 * the line, Renew recorded there when the cap filter dropped every destination and the pin renewed in place.
 */
function branchOf(walk: WalkStep[], drafts: Draft[], shape: StripShape, kind: DecisionKind, kinds: Record<string, StationKind>): Branch | null {
  const firedAt = walk.findIndex((s) => isRule(s.step) && s.verdict === 'fired');
  if (shape === 'pinned' && firedAt >= 0) {
    const fired = walk[firedAt].step;
    const cap = walk.find((s, i) => i > firedAt && s.step === 'cap_filter');
    const raw = kind === 'moved' ? 'moved' : (cap?.verdict ?? walk[firedAt].verdict);
    const firedKind = lookup(kinds, fired);
    const move: Station = {
      ...blank('move', fired, null, firedKind ?? UNKNOWN_KIND, firedKind !== undefined),
      badge: verdictWord(raw),
      group: null,
      head: 'Move',
      level: 'track',
      second: [`${stationWord(fired)} rule`],
      stub: null,
      tone: verdictTone(raw, cap === undefined ? fired : 'cap_filter'),
      verdict: raw,
      word: 'Move',
    };
    let first = -1;
    drafts.forEach((d, i) => {
      const s = d.station;
      if (s.index !== null && s.index > firedAt && !isRule(s.step) && s.step !== 'renew') {
        s.level = 'track';
        // The track is the walk's way off the line, so its stations carry no stub of their own.
        s.stub = null;
        if (first < 0) first = i;
      }
      if (s.step === fired && s.index === firedAt) s.stub = null;
    });
    const at = first >= 0 ? first : drafts.findIndex((d) => d.station.index === firedAt) + 1;
    drafts.splice(at, 0, { placeholder: false, station: move, stepVerdict: raw });
    return { from: fired, kind: 'move' };
  }
  if (shape === 'short' || firedAt >= 0) return null;
  const rung = walk.findIndex((s) => RUNGS.includes(s.step) && s.verdict === 'match');
  if (rung < 0) return null;
  const from = drafts.find((d) => d.station.index === rung);
  if (from === undefined) return null;
  from.station.stub = null;
  return { from: from.station.id, kind: 'rung' };
}

/**
 * The last recorded step that ran decides the walk, Reserve aside, and the slot the shape expects stands in when none
 * did.
 */
function decisiveOf(walk: WalkStep[], drafts: Draft[], shape: StripShape, kind: DecisionKind): number {
  let last = -1;
  for (let i = walk.length - 1; i >= 0 && last < 0; i -= 1) if (walk[i].verdict !== 'not_run' && walk[i].step !== RESERVE) last = i;
  if (last >= 0) {
    let at = -1;
    drafts.forEach((d, i) => {
      if (d.station.index === last) at = i;
    });
    return at;
  }
  const slot = shape === 'fresh' ? SLOT_OF[kind] : shape === 'pinned' ? 'renew' : null;
  return slot === null ? -1 : drafts.findIndex((d) => d.station.id === slot);
}

function settle(drafts: Draft[], decisive: number): void {
  const before: Partial<Record<Station['level'], PathState>> = {};
  drafts.forEach((d, i) => {
    const s = d.station;
    const atOrBefore = decisive >= 0 && i <= decisive;
    if (d.placeholder) {
      s.state = atOrBefore ? 'inferred' : 'idle';
      s.badge = atOrBefore ? 'not recorded' : 'not run';
      s.tone = 'neutral';
    } else {
      // The walk books after its decision, so a booking was reached, and so was a Reserve that booked nothing straight
      // after a taken station. One past the walk's idle stations (a hold's cut-short rules) stands idle with them.
      const booked = s.step === RESERVE && d.stepVerdict !== 'not_run' && (d.stepVerdict === 'booked' || before[s.level] === 'taken');
      s.state = d.stepVerdict !== 'not_run' && (atOrBefore || booked) ? 'taken' : 'idle';
      if (s.state === 'idle') s.tone = 'neutral';
    }
    s.decisive = i === decisive;
    before[s.level] = s.state;
  });
}

function bracketsOf(stations: Station[]): Bracket[] {
  const out: Bracket[] = [];
  for (const level of ['line', 'track'] as const) {
    const groups = new Map<StripGroup, Station[]>();
    for (const s of stations) if (s.level === level && s.group !== null) groups.set(s.group, [...(groups.get(s.group) ?? []), s]);
    for (const [group, members] of groups) {
      if (members.length < 2) continue;
      const recorded = members.some((s) => s.index !== null);
      // Five "not recorded" badges a unit apart overlap, so the bracket's word says it once for the group.
      if (!recorded) for (const s of members) Object.assign(s, { badge: '', tone: 'neutral' });
      out.push({ group, level, recorded, stations: members.map((s) => s.id), word: recorded ? GROUP_WORD[group] : `${GROUP_WORD[group]}, not recorded` });
    }
  }
  return out;
}

function hostOf(record: DecisionRecord, stations: Station[], shape: StripShape, branch: Branch | null): string | null {
  if (record.candidates.length === 0) return null;
  const pick = stations.find((s) => s.step === 'pick' && s.index !== null);
  if (pick !== undefined) return pick.id;
  if (shape === 'fresh') return stations.find((s) => s.id === SLOT_OF[record.outcome.kind])?.id ?? null;
  const walk = record.walk.filter((s) => s.step !== RESERVE);
  if (shape === 'pinned' && walk[walk.length - 1]?.step === 'renew') return 'renew';
  const track = stations.filter((s) => s.level === 'track' && s.step !== RESERVE);
  // A rung that matched before the continuation's last leaves the later rungs unrun, so it places the request itself.
  if (branch?.kind === 'move' && track.length > 0) return (track.find((s) => s.decisive) ?? track[track.length - 1]).id;
  // A hold on the pin's own account ends a walk whose later rules did not run, so the step that held hosts it.
  const recorded = stations.filter((s) => s.index !== null && s.step !== RESERVE);
  const ran = recorded.filter((s) => s.verdict !== 'not_run');
  return (ran[ran.length - 1] ?? recorded[recorded.length - 1])?.id ?? null;
}

function leavesOf(record: DecisionRecord, now: number): Leaf[] {
  const all: Leaf[] = record.candidates.map((c) => {
    const verdict = candidateVerdict(c, now);
    return { accountId: c.accountId, badge: verdict.badge, chip: verdict.chip, chosen: verdict.chosen, more: 0, tone: verdict.badgeTone };
  });
  if (all.length <= MAX_LEAVES) return all;
  const shown = all.slice(0, MAX_LEAVES);
  const chosenAt = all.findIndex((l) => l.chosen);
  if (chosenAt >= MAX_LEAVES) shown[MAX_LEAVES - 1] = all[chosenAt];
  const more = all.length - MAX_LEAVES;
  return [...shown, { accountId: null, badge: `and ${more} more`, chip: null, chosen: false, more, tone: 'neutral' }];
}

function terminiOf(outcome: DecisionRecord['outcome'], shape: StripShape): Terminus[] {
  const kind = outcome.kind;
  const named = terminusOf(outcome);
  // The fourth, fifth and sixth ways out are drawn only where a walk took one, in the place of Held for a slot.
  const termini: TerminusKind[] = ['chosen', HELD_TERMINI.has(named) ? named : 'held', 'refused'];
  return termini.map((t) => ({
    badge: t === named ? DECISION_KIND_LABEL[kind] : null,
    // A pinned walk ends in Chosen, and in the terminus its outcome names when that is another.
    drawn: shape !== 'pinned' || t === 'chosen' || t === named,
    kind: t,
    state: t === named ? 'taken' : 'idle',
    tone: t === named ? DECISION_KIND_TONE[kind] : 'neutral',
    word: TERMINUS_WORD[t],
  }));
}

/** The strip's stations, brackets, branch, host, decisive station, leaves and termini for one record. */
export function stationModel(record: DecisionRecord, now: number): StripModel {
  const walk = record.walk;
  const kind = record.outcome.kind;
  const shape = shapeOf(walk);
  // A version 1 record was written before the fit gate and the reservation, with the cool reading and five partitions.
  const maps = record.version < 2 ? LEGACY_MAPS : MAPS;
  const drafts = draftsOf(walk, merge(walk, expectedOf(shape, walk, kind)), shape, now, maps);
  const branch = branchOf(walk, drafts, shape, kind, maps.kinds);
  const decisive = decisiveOf(walk, drafts, shape, kind);
  settle(drafts, decisive);
  const stations = drafts.map((d) => d.station);
  return {
    branch,
    brackets: bracketsOf(stations),
    decisive: decisive >= 0 ? stations[decisive].id : null,
    host: hostOf(record, stations, shape, branch),
    leaves: leavesOf(record, now),
    outcome: kind,
    shape,
    stations,
    termini: terminiOf(record.outcome, shape),
  };
}

const joined = (parts: (string | null | undefined)[]) => joinPresent(parts, ', ');

/**
 * The accessible name of a station button, from `head`: "Pick, chosen, Pace P0", "Shed, fired", "Eligible, not
 * recorded".
 */
export function stationName(s: Station): string {
  // A station under a bracket the record does not carry draws no badge, so its name says what the bracket says.
  return joined([s.head, s.verdict === null && s.badge === '' ? 'not recorded' : s.badge, ...s.second]);
}

/** "primary, ranked 2nd", "and 4 more". */
export function leafName(l: Leaf): string {
  return l.more > 0 ? l.badge : joined([l.accountId, l.badge, l.chip?.value]);
}

/** "Chosen, placed", "Held for a slot, not reached". */
export function terminusName(t: Terminus): string {
  return joined([t.word, t.drawn && t.state !== 'idle' && t.badge !== null ? t.badge : 'not reached']);
}

export interface Point { x: number; y: number }
export interface StripPath { d: string; kind: 'curve' | 'edge' | 'fan' | 'stub' | 'tick' | 'track'; state: PathState }
export interface StripLayout {
  brackets: { from: number; to: number; word: string; y: number }[];
  height: number;
  /** A station label's box: `top` its upper edge, `height` from the label's lines. */
  labels: Record<string, { above: boolean; height: number; top: number; width: number }>;
  /** A leaf's right edge on its row, and the widest it draws so its left edge stays inside the strip. */
  leaves: (Point & { maxWidth: number })[];
  paths: StripPath[];
  stations: Record<string, Point>;
  termini: Partial<Record<TerminusKind, Point>>;
  unit: number;
  width: number;
  y0: number;
  yu: number;
}

export const RADIUS = 6.5;
export const TERMINUS_RADIUS = 7.5;
const LABEL_MIN = 58;
const LABEL_MAX = 110;
const IN_GROUP = 1;
const BETWEEN = 1.45;
const END_FRESH = 1.9;
// A partition's badge can wrap to a word wider than its label (`through` of a failover's `fell through`, a 63 px pill),
// which stands past the label's edges, so two partitions stand at least this far apart to keep the pill clear of a
// neighbor's 58 px label.
const PARTITION_GAP = 61;
// The shortest run from a line's last station to the termini, in units, so the termini stand past the last station at
// any width.
const END_MIN = 1;
const END_PINNED = 2.2;
const LEFT = 36;
const RIGHT = 150;
const UNIT_MAX = 104;
const Y0 = 104;
// A matched rung's track needs its own headroom above the line.
const Y0_RUNG = 128;
const YU = 56;
// A move's continuation draws two levels, and fifteen stations share the card's width on a shed move, so it spaces
// tighter on narrower margins.
const MOVE_Y0 = 176;
const MOVE_YU = 96;
const RISE = 0.55;
const MOVE_IN_GROUP = 0.9;
const MOVE_BETWEEN = 1.2;
const BEFORE_PICK = 1.35;
const MOVE_LEFT = 24;
const MOVE_RIGHT = 84;
const MOVE_END = 1;
const STUB_ACROSS = 10;
const STUB_UP = 16;
const INTO_TERMINUS = 8;
const TERMINUS_DROP: Record<TerminusKind, number> = { chosen: 0, held: 36, held_reset: 36, held_rewrite: 36, held_slow_down: 36, refused: 72 };
const KNEE = 0.42;
const KNEE_EASE = 18;
const CORNER = 22;
const BEND = 16;
const RISE_LEAN = 4;
const FIRST_ROW = 76;
const ROW = 30;
const ROW_FOOT = 4;
// The three termini's words need this much room below the line however few the leaves.
const BELOW_LINE = 104;
const TICK = 14;
const LEAF_GAP = 16;
// The room a leaf keeps from the strip's left edge, which the strip cannot scroll to.
const LEAF_EDGE = 8;
/**
 * The story's widest leaf measures 325 px in Chromium (an "out" badge and a 28-character obstacle chip), so a host this
 * far from the strip's left edge keeps it whole past the gap and the edge's room. A wider leaf cuts its account short.
 */
export const LEAF_ROOM = 350;
/** A leaf is centered on its row and 24 px tall. */
export const LEAF_HALF = 12;
// The label box the strip's stylesheet draws: its near edge 12 px off the station's center, a 16 px line for the name
// and one for the second line, the console's 22 px badge box (an 18 px line in 2 px of padding), 3 px apart.
const LABEL_OFFSET = 12;
const LABEL_LINE = 16;
const LABEL_BADGE = 22;
const LABEL_GAP = 3;
// A label is centered on its station, so one this much narrower than the way to its nearest neighbor shares no pixel
// with that neighbor's, and a click on its words opens its own popover.
const LABEL_CLEAR = 4;
const BRACKET_OVERHANG = 14;
const BRACKET_RISE = 36;
// A track bracket clears the labels the track's stations carry above it.
const TRACK_BRACKET_RISE = 62;
/** A bracket's ticks reach this far down from its line, and its word stands this far above it. */
export const BRACKET_TICK = 6;
export const BRACKET_WORD_RISE = 20;
// The room a bracket's ticks keep above a label under them.
const BRACKET_CLEAR = 3;

const sum = (weights: number[]) => weights.reduce((n, w) => n + w, 0);
const sameGroup = (a: Station, b: Station) => a.group !== null && a.group === b.group;
const labelWidth = (unit: number) => Math.max(LABEL_MIN, Math.min(unit * 1.02, LABEL_MAX));

/** The way on from a point to a terminus: straight when they share a level, else down through the knee. */
function tail(from: Point, to: Point): string {
  if (from.y === to.y) return `H${to.x}`;
  const run = (to.x - from.x) * KNEE;
  const knee = from.x + run;
  // A move's track has one unit to its terminus, so at a narrow width the ease would start behind the station.
  const ease = Math.min(KNEE_EASE, run);
  return `H${knee - ease} C${knee} ${from.y} ${knee} ${to.y} ${knee + ease} ${to.y} H${to.x}`;
}

const curve = (from: Point, to: Point) => `M${from.x} ${from.y} ${tail(from, to)}`;

/**
 * A taken curve from a station the walk runs on past. It drops from the circle at once and runs below the line at
 * `y0`, at the Refused terminus's depth, from a track station as from a line one, so no accent lies along an idle
 * station, its label, an edge or the line's own idle curve into a terminus.
 */
function drop(from: Point, to: Point, y0: number): string {
  const lane = Math.max(to.y, y0 + TERMINUS_DROP.refused);
  const into = lane === to.y ? `H${to.x}` : `H${to.x - CORNER} C${to.x} ${lane} ${to.x} ${lane} ${to.x} ${lane - CORNER} V${to.y + TERMINUS_RADIUS}`;
  return `M${from.x} ${from.y + RADIUS} V${lane - CORNER} C${from.x} ${lane} ${from.x} ${lane} ${from.x + CORNER} ${lane} ${into}`;
}

const labelHeight = (s: Station) => LABEL_LINE + (s.second.length > 0 ? LABEL_GAP + LABEL_LINE : 0) + (s.badge !== '' ? LABEL_GAP + LABEL_BADGE : 0);

/** A station's label height in px, from the rendered label where the caller measured one. */
export type LabelHeights = Readonly<Record<string, number>>;

/** A path does not run on from a station the walk never reached. */
const leaving = (from: Station, into: PathState): PathState => (from.state === 'idle' ? 'idle' : into);

/**
 * `into` is the terminus a track carries the taken path into, and `descent` the line station a track comes down into.
 */
interface Frame { descent: string | null; into: TerminusKind | null; line: Station[]; points: Record<string, Point>; paths: StripPath[]; unit: number; xEnd: number; y0: number; yu: number }

/** The stations after `from` are taken Reserves alone, which carry the walk on to its way out. */
const booksOn = (stations: Station[], from: number) => from >= 0 && stations.slice(from + 1).every((s) => s.step === RESERVE && s.state === 'taken');

function edgesAndStubs(line: Station[], points: Record<string, Point>): StripPath[] {
  const paths: StripPath[] = line.slice(1).map((s, i) => ({ d: `M${points[line[i].id].x} ${points[s.id].y} H${points[s.id].x}`, kind: 'edge', state: leaving(line[i], s.state) }));
  for (const s of line) {
    if (s.stub === null) continue;
    const p = points[s.id];
    paths.push({ d: `M${p.x} ${p.y} l${STUB_ACROSS} ${s.stub === 'released' ? STUB_UP : -STUB_UP}`, kind: 'stub', state: 'idle' });
  }
  return paths;
}

function finish(model: StripModel, width: number, f: Frame, measured: LabelHeights | undefined): StripLayout {
  const { line, paths, points, unit, xEnd, y0, yu } = f;
  // A rendered label is taller than counted where its words wrapped, which only the console's font decides.
  const heightOf = (s: Station) => {
    const h = measured?.[s.id];
    return h !== undefined && h > 0 ? h : labelHeight(s);
  };
  const termini: Partial<Record<TerminusKind, Point>> = {};
  const last = line[line.length - 1];
  const decided = model.stations.find((s) => s.id === model.decisive);
  // A track that comes down into a line station hands the taken path on from there, and the Reserve after the decision,
  // the walk's last station, carries it on too.
  const descended = f.descent === null ? undefined : line.find((s) => s.id === f.descent);
  const decisive = descended ?? (decided !== undefined && last !== undefined && booksOn(line, line.indexOf(decided)) ? last : decided);
  let dropFrom: string | null = null;
  for (const t of model.termini) {
    if (!t.drawn) continue;
    const at = { x: xEnd, y: y0 + TERMINUS_DROP[t.kind] };
    termini[t.kind] = at;
    // A walk that ends before the slot still reaches its terminus, so the taken path runs there from the station that
    // decided it, unless a track already carries it there.
    const runs = t.state === 'taken' && t.kind !== f.into;
    const from = runs ? (decisive ?? last) : last;
    if (from === undefined) continue;
    const drops = runs && from !== last;
    if (drops) dropFrom = from.id;
    paths.push({ d: drops ? drop(points[from.id], at, y0) : curve(points[from.id], at), kind: 'curve', state: runs ? leaving(from, from.state) : 'idle' });
  }
  const labels: StripLayout['labels'] = {};
  // A host the track comes down into keeps its label below the line, clear of the accent entering it from above. A
  // station a taken curve drops from carries its label above, clear of the drop.
  const aboveOf = (s: Station) => s.level === 'track' || (s.id === model.host && s.id !== f.descent) || s.id === dropFrom;
  for (const s of model.stations) {
    const above = aboveOf(s);
    const height = heightOf(s);
    const p = points[s.id];
    // The unit spaces stations by their groups alone, so two a unit apart, or the rules a move spreads below its track,
    // stand closer than the unit's label is wide.
    const near = model.stations.reduce((n, o) => (o !== s && o.level === s.level && aboveOf(o) === above ? Math.min(n, Math.abs(points[o.id].x - p.x)) : n), Infinity);
    labels[s.id] = { above, height, top: above ? p.y - LABEL_OFFSET - height : p.y + LABEL_OFFSET, width: Math.min(labelWidth(unit), near - LABEL_CLEAR) };
  }
  let firstRow = y0 + FIRST_ROW;
  const leaves: StripLayout['leaves'] = [];
  if (model.host !== null && model.leaves.length > 0) {
    const host = points[model.host];
    // The fan drops from the circle, or from a label below the line past its bottom edge, the rows moving down when
    // that label would reach the first leaf.
    const label = labels[model.host];
    const top = label.above ? host.y + RADIUS : label.top + label.height;
    firstRow = Math.max(firstRow, top + LEAF_HALF);
    // A leaf runs left from the fan as far as its words take it, so the rows keep below every label under a line that
    // reaches past the leaves' right edge.
    for (const s of model.stations) {
      const l = labels[s.id];
      if (s.id !== model.host && !l.above && points[s.id].x - l.width / 2 < host.x - LEAF_GAP) firstRow = Math.max(firstRow, l.top + l.height + LABEL_GAP + LEAF_HALF);
    }
    const rows = model.leaves.map((_, i) => firstRow + i * ROW);
    paths.push({ d: `M${host.x} ${top} V${rows[rows.length - 1]}`, kind: 'fan', state: 'idle' });
    model.leaves.forEach((l, i) => {
      paths.push({ d: l.chosen ? `M${host.x} ${top} V${rows[i]} H${host.x - TICK}` : `M${host.x} ${rows[i]} H${host.x - TICK}`, kind: 'tick', state: l.chosen ? 'taken' : 'idle' });
      leaves.push({ maxWidth: host.x - LEAF_GAP - LEAF_EDGE, x: host.x - LEAF_GAP, y: rows[i] });
    });
  }
  const brackets = model.brackets.map((b) => {
    const xs = b.stations.map((id) => points[id].x);
    const rise = b.level === 'line' ? y0 - BRACKET_RISE : yu - TRACK_BRACKET_RISE;
    const clear = b.stations.filter((id) => labels[id].above).map((id) => labels[id].top - BRACKET_TICK - BRACKET_CLEAR);
    return { from: Math.min(...xs) - BRACKET_OVERHANG, to: Math.max(...xs) + BRACKET_OVERHANG, word: b.word, y: Math.min(rise, ...clear) };
  });
  return {
    brackets,
    height: Math.max(firstRow + Math.max(model.leaves.length, 1) * ROW + ROW_FOOT, y0 + BELOW_LINE),
    labels,
    leaves,
    paths,
    stations: points,
    termini,
    unit,
    width,
    y0,
    yu,
  };
}

/** One line of stations: a fresh placement, a renewal, a short walk. */
function layoutLine(model: StripModel, width: number, measured: LabelHeights | undefined, dy: number): StripLayout {
  const line = model.stations;
  const weights = line.slice(1).map((s, i) => (sameGroup(line[i], s) ? IN_GROUP : BETWEEN));
  const end = model.shape === 'pinned' ? END_PINNED : END_FRESH;
  const available = width - LEFT - RIGHT;
  const partition = line.slice(1).map((s, i) => s.group === 'partitions' && sameGroup(line[i], s));
  const floored = partition.filter(Boolean).length;
  const free = Math.min(UNIT_MAX, available / (sum(weights) + end));
  // The room the partitions' floor takes past their unit comes out of the run to the termini while that run keeps a
  // unit, so the gaps between groups keep their width: a tight card's widest pill (an exhausted decision's 79 px
  // `exhausted`) needs every pixel of them. Past that, the floor comes out of the unit inside the width with the run
  // held at a unit, so every station stays left of the termini at any width.
  const extra = (u: number) => floored * Math.max(0, PARTITION_GAP - IN_GROUP * u);
  const unit = extra(free) <= (end - END_MIN) * free ? free : (available - floored * PARTITION_GAP) / (sum(weights) - floored * IN_GROUP + END_MIN);
  const gaps = weights.map((w, i) => (partition[i] ? Math.max(w * unit, PARTITION_GAP) : w * unit));
  const run = Math.max(END_MIN * unit, end * unit - extra(unit));
  const y0 = (model.branch?.kind === 'rung' ? Y0_RUNG : Y0) + dy;
  const yu = YU + dy;
  const points: Record<string, Point> = {};
  let x = LEFT;
  line.forEach((s, i) => {
    if (i > 0) x += gaps[i - 1];
    points[s.id] = { x, y: y0 };
  });
  let xEnd = x + run;
  // A leaf runs left from its host, so a short line moves right until the widest leaf fits there, as far as the
  // termini's margin allows.
  const host = model.host !== null && model.leaves.length > 0 ? points[model.host] : undefined;
  const shift = host === undefined ? 0 : Math.min(Math.max(0, LEAF_ROOM - host.x), Math.max(0, width - RIGHT - xEnd));
  if (shift > 0) {
    for (const p of Object.values(points)) p.x += shift;
    xEnd += shift;
  }
  const paths = edgesAndStubs(line, points);
  // A rung that placed the request passes the slot it never ran, and comes down into the Reserve it booked when the
  // line ends in one, which then runs on into Chosen.
  const last = line[line.length - 1];
  const booked = model.branch?.kind === 'rung' && last?.step === RESERVE && last.state === 'taken' ? last : undefined;
  if (model.branch?.kind === 'rung') {
    const from = points[model.branch.from];
    const to = booked === undefined ? { x: xEnd, y: y0 - INTO_TERMINUS } : { x: points[booked.id].x, y: y0 };
    paths.push({ d: `M${from.x} ${y0} C${from.x} ${yu + BEND} ${from.x} ${yu} ${from.x + CORNER} ${yu} H${to.x - CORNER} C${to.x} ${yu} ${to.x} ${yu + BEND} ${to.x} ${to.y}`, kind: 'track', state: 'taken' });
  }
  return finish(model, width, { descent: booked?.id ?? null, into: model.branch?.kind === 'rung' && booked === undefined ? 'chosen' : null, line, paths, points, unit, xEnd, y0, yu }, measured);
}

/**
 * A move's continuation: the line to the fired rule, the rise to the Move station and the steps after it on the
 * track. The rules it cut short and Renew spread on the line below, clear of the line the candidates hang from.
 */
function layoutMove(model: StripModel, width: number, fired: string, measured: LabelHeights | undefined, dy: number): StripLayout {
  const y0 = MOVE_Y0 + dy;
  const yu = MOVE_YU + dy;
  const lineStations = model.stations.filter((s) => s.level === 'line');
  const firedAt = lineStations.findIndex((s) => s.id === fired);
  const before = lineStations.slice(0, firedAt + 1);
  const after = lineStations.slice(firedAt + 1);
  const track = model.stations.filter((s) => s.level === 'track');
  const renewed = after.find((s) => s.step === 'renew' && s.index !== null);
  const lineWeights = before.slice(1).map((s, i) => (sameGroup(before[i], s) ? MOVE_IN_GROUP : MOVE_BETWEEN));
  const trackWeights = track.slice(1).map((s, i) => (s.step === 'pick' ? BEFORE_PICK : sameGroup(track[i], s) ? MOVE_IN_GROUP : MOVE_BETWEEN));
  // A pin renewed in place takes one more unit for Renew past the track's last station.
  const total = sum(lineWeights) + RISE + sum(trackWeights) + (renewed === undefined ? 0 : 1) + MOVE_END;
  const unit = Math.min(UNIT_MAX, (width - MOVE_LEFT - MOVE_RIGHT) / total);
  const points: Record<string, Point> = {};
  let x = MOVE_LEFT;
  before.forEach((s, i) => {
    if (i > 0) x += lineWeights[i - 1] * unit;
    points[s.id] = { x, y: y0 };
  });
  const xFired = x;
  x += RISE * unit;
  track.forEach((s, i) => {
    if (i > 0) x += trackWeights[i - 1] * unit;
    points[s.id] = { x, y: yu };
  });
  const xLast = x;
  if (renewed !== undefined) {
    x += unit;
    points[renewed.id] = { x, y: y0 };
  }
  const xEnd = x + MOVE_END * unit;
  const spread = after.filter((s) => s !== renewed);
  const host = model.host === null ? undefined : points[model.host];
  const to = renewed !== undefined ? x : host !== undefined ? host.x : x;
  spread.forEach((s, i) => {
    points[s.id] = { x: xFired + ((to - xFired) * (i + 1)) / (spread.length + 1), y: y0 };
  });
  const line = [...before, ...spread, ...(renewed === undefined ? [] : [renewed])];
  const paths = edgesAndStubs(line, points);
  const xMove = track.length > 0 ? points[track[0].id].x : xFired;
  const rise = `M${xFired} ${y0} C${xFired} ${yu + CORNER} ${xFired + RISE_LEAN} ${yu} ${xMove} ${yu}`;
  const down = (xTo: number, yTo: number) => `H${xTo - CORNER} C${xTo} ${yu} ${xTo} ${yu + BEND} ${xTo} ${yTo}`;
  // The continuation ends at the terminus its outcome names, as the line does, or comes down into Renew when the pin
  // renewed in place.
  const named = model.termini.find((t) => t.state === 'taken')?.kind ?? 'chosen';
  const end = renewed !== undefined ? down(points[renewed.id].x, y0) : named === 'chosen' ? down(xEnd, y0 - INTO_TERMINUS) : `H${xLast} ${tail({ x: xLast, y: yu }, { x: xEnd, y: y0 + TERMINUS_DROP[named] })}`;
  const decided = track.findIndex((s) => s.decisive);
  const last = track[track.length - 1];
  const booked = decided >= 0 && decided < track.length - 1 && last.step === RESERVE && last.state === 'taken' ? last : undefined;
  if (decided >= 0 && !booksOn(track, decided) && booked !== undefined) {
    // A rung that matched before the track's last leaves the later rungs unrun, and the Reserve it booked carries the
    // walk on, so the taken path passes under them between the track and the line, as a line's rung passes the slot it
    // never ran, and comes back up into the Reserve.
    const from = points[track[decided].id];
    const to = points[booked.id];
    const lane = (yu + y0) / 2;
    const under = `V${lane - CORNER} C${from.x} ${lane} ${from.x} ${lane} ${from.x + CORNER} ${lane} H${to.x - CORNER} C${to.x} ${lane} ${to.x} ${lane} ${to.x} ${lane - CORNER} V${yu}`;
    paths.push({ d: `${rise} H${from.x} ${under} ${end}`, kind: 'track', state: 'taken' });
    paths.push({ d: `M${from.x} ${yu} H${to.x}`, kind: 'track', state: 'idle' });
  } else if (decided >= 0 && !booksOn(track, decided)) {
    // A rung that matched before the track's last leaves the later rungs unrun, so the taken path drops from it as from
    // a line's decisive station, and the track runs on past it idle.
    const from = points[track[decided].id];
    paths.push({ d: `${rise} H${from.x} ${drop(from, { x: xEnd, y: y0 + TERMINUS_DROP[named] }, y0)}`, kind: 'track', state: 'taken' });
    paths.push({ d: `M${from.x} ${yu} ${end}`, kind: 'track', state: 'idle' });
  } else paths.push({ d: `${rise} ${end}`, kind: 'track', state: 'taken' });
  return finish(model, width, { descent: renewed?.id ?? null, into: renewed === undefined ? named : null, line, paths, points, unit, xEnd, y0, yu }, measured);
}

/**
 * Every coordinate and path the strip draws, for a width the caller floors at 640 px. `measured` holds the heights of
 * the labels the caller drew taller than the counted lines, and a missing or zero reading keeps the count.
 */
export function layoutStrip(model: StripModel, width: number, measured?: LabelHeights): StripLayout {
  const draw = (dy: number) => (model.branch?.kind === 'move' ? layoutMove(model, width, model.branch.from, measured, dy) : layoutLine(model, width, measured, dy));
  const first = draw(0);
  // A taller label above its line, or the bracket it lifts, can reach past the surface's top, so the whole strip moves
  // down by that much.
  const reach = Math.min(0, ...Object.values(first.labels).map((l) => l.top), ...first.brackets.map((b) => b.y - BRACKET_WORD_RISE));
  return reach < 0 ? draw(Math.ceil(-reach)) : first;
}

/** What a click on the strip names: a station by its model id, a leaf by its index, a terminus by its kind. */
export type StripAnchor =
  | { kind: 'station'; id: string }
  | { kind: 'leaf'; index: number }
  | { kind: 'terminus'; id: TerminusKind };

/** The one open popover across every strip on the page, keyed by the strip that owns it. */
export interface OpenPopover {
  anchor: StripAnchor;
  focus: boolean;
  pinned: boolean;
  stripId: string;
}

let openPopover: OpenPopover | null = null;
const popoverListeners = new Set<() => void>();

/**
 * The page's one popover as a module store the strips read through `useSyncExternalStore`, so the routing log's cards
 * and the request drawer share it with no provider. `set` takes a value or an updater and notifies only on a change.
 */
export const popoverStore = {
  get: (): OpenPopover | null => openPopover,
  set(next: OpenPopover | null | ((o: OpenPopover | null) => OpenPopover | null)): void {
    const value = typeof next === 'function' ? next(openPopover) : next;
    if (value === openPopover) return;
    openPopover = value;
    for (const listener of popoverListeners) listener();
  },
  subscribe(listener: () => void): () => void {
    popoverListeners.add(listener);
    return () => {
      popoverListeners.delete(listener);
    };
  },
};
