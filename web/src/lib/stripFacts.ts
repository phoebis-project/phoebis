import type { DecisionRecord, DecisionSummary, PrefixEntry, WalkFact, WalkStep } from '@/api/types';
import { has, lookup } from '@/lib/counts';
import { LEGACY_PARTITION_FACTS, PARTITION_FACTS, type Leaf, type Renderer, type Station, type StripAnchor, type StripModel, type Terminus } from '@/lib/decisionStrip';
import { fmtCompact, fmtInt, fmtPct } from '@/lib/format';
import type { BadgeTone } from '@/lib/quota';
import { accountChip, ageWord, candidateCells, type Chip, factChips, formatFact, hashTitle, keyWord, outcomeRows, paceHeader, PLACE_WORD, plain, rungEntry, shortHash, STEP_NAME, valueText, verdictTone, type VerdictTone, verdictWord, walkLines, warmUntil, WINDOW_WORD, words } from '@/lib/routingLog';

// A popover's plan: the facts of one station, leaf or terminus as plain data in the console's words, which
// `StripPopover` draws and the unit tests read.

/** A label and its chips. A rung's entry draws its hash first. */
export interface PlanRow { chips: Chip[]; entry?: PrefixEntry; label: string; marked?: boolean; title?: string }
export interface PlanGroup { label: string | null; rows: PlanRow[] }
export interface PlanColumn { key: string; label: string; numeric: boolean; title?: string }
/**
 * One row per account in the facts' order and one cell per column, null where the account lacks the fact. A cell
 * with a `tone` draws its text as a badge in it. `wraps` lets a figure break over two lines, which only the Fit
 * table's nine columns need to keep inside the popover.
 */
export interface PlanTable { columns: PlanColumn[]; loose: PlanRow[]; rows: { account: Chip; cells: Record<string, { text: string; title?: string; tone?: VerdictTone } | null> }[]; wraps: boolean }
export interface WindowRow { age: string; left: string; reset: string; source: string; used: string; window: string }
export type PopoverBlock =
  | { kind: 'candidate'; chips: Chip[]; windows: WindowRow[] }
  | { kind: 'chips'; chips: Chip[]; marked: number | null }
  | { kind: 'groups'; groups: PlanGroup[] }
  | { kind: 'sentence'; text: string }
  | { kind: 'table'; table: PlanTable };
export interface PopoverPlan {
  block: PopoverBlock;
  /** `chip` is a candidate's title, drawn as a badge in `tone` with its hover, and `account` links the candidate. */
  head: { account: Chip | null; badge: string | null; chip: Chip | null; second: string[]; tone: BadgeTone; word: string };
  renderer: Renderer;
  /** Every fact of the step the plan did not place, with why, so a test can hold that nothing is dropped silently. */
  unplaced: { key: string; reason: string }[];
}

export const SENTENCE = {
  notReached: 'The decision did not end here.',
  notRun: 'This step did not run.',
  placeholder: 'The record carries no step here.',
  rest: 'The rest of the walk did not run: the moves, the rungs and the partitions.',
  reserveNone: 'Nothing was booked.',
  reserveRenewed: 'Nothing was booked: the pin renewed in place.',
} as const;

interface GroupKeys { keys: readonly string[]; label: string | null }
const PIN_GROUPS: GroupKeys[] = [
  { keys: ['found', 'account_id', 'epoch'], label: 'Lookup' },
  { keys: ['idle_ms', 'refresh_age_ms'], label: 'Age' },
  { keys: ['warmth', 'cause', 'marked'], label: 'Warmth' },
];
/**
 * A key no group names falls to an unlabeled group, so every key the shed line writes has one. A projection's stamp and
 * wait, and the 5-hour window's age with its hold, name their window but join the round, since they say why the round
 * has not run yet.
 */
const SHED_GROUPS: GroupKeys[] = [
  {
    keys: ['projection_five_hour', 'horizon_five_hour', 'projection_seven_day', 'horizon_seven_day', 'projection_seven_day_fable', 'horizon_seven_day_fable', 'demand_five_hour', 'factor', 'ramp', 'slow_down', 'speed', 'ceiling', 'cooldown', 'window_share_fast'],
    label: 'Windows',
  },
  {
    keys: [
      'trigger', 'excess', 'fired_by', 'over_ms', 'over_since', 'over_since_five_hour', 'wait_five_hour', 'age_five_hour', 'hold_five_hour', 'over_since_seven_day', 'wait_seven_day', 'over_since_seven_day_fable',
      'wait_seven_day_fable', 'stretch_ms', 'threshold_ms', 'target', 'round', 'marked', 'marks', 'skipped',
    ],
    label: 'Round',
  },
  { keys: ['destinations', 'no_destination', 'ending'], label: 'Destinations' },
];
/** A version 1 record's shed rule read the lead over pace. */
const LEGACY_SHED_GROUPS: GroupKeys[] = [
  { keys: ['ahead_five_hour', 'elapsed_five_hour', 'deadline_five_hour', 'ahead_seven_day', 'elapsed_seven_day', 'deadline_seven_day', 'cooldown', 'window_share_fast'], label: 'Windows' },
  { keys: ['margin', 'threshold', 'round', 'marked', 'marks'], label: 'Round' },
  { keys: ['destinations', 'no_destination'], label: 'Destinations' },
];
const RENEW_GROUPS: GroupKeys[] = [
  { keys: ['lane'], label: 'Lane' },
  { keys: ['refresh_stamped', 'hashes_stamped'], label: 'Stamps' },
];
/** The return rule's mode decides which of the facts after it the rule read, so it reads first. */
const RETURN_GROUPS: GroupKeys[] = [{ keys: ['mode', 'kind', 'cold', 'dwell_age_ms', 'dwell_ms', 'destinations', 'no_destination', 'ending'], label: null }];
const GROUPS_OF: Record<string, GroupKeys[]> = { 'move:return_to_plan': RETURN_GROUPS, 'move:shed': SHED_GROUPS, partitions: [{ keys: PARTITION_FACTS, label: null }], pin: PIN_GROUPS, renew: RENEW_GROUPS };
const LEGACY_GROUPS_OF: Record<string, GroupKeys[]> = { ...GROUPS_OF, 'move:shed': LEGACY_SHED_GROUPS, partitions: [{ keys: LEGACY_PARTITION_FACTS, label: null }] };
/**
 * The Fit table's columns: the projection, the rewrite and the reserved share of every window the gate read, window by
 * window in the console's order, then the runway, the window that binds, the end and the verdict.
 */
const FIT_COLUMN_ORDER = ['projection', 'rewrite', 'reserved', 'runway_ms', 'binding', 'end', 'verdict'];
const fitRank = (key: string) => {
  const at = FIT_COLUMN_ORDER.findIndex((lead) => key === lead || key.startsWith(`${lead}_`));
  const window = Object.keys(WINDOW_WORD).findIndex((w) => key.endsWith(`_${w}`));
  return (at < 0 ? FIT_COLUMN_ORDER.length : at) * 10 + (window < 0 ? 9 : window);
};
/** A fact that names the account a step acted on reads as "on" that account, as its chip does. */
const ROW_LABEL: Record<string, string> = { account_id: 'on' };
const NUMERIC_UNITS = new Set(['count', 'ms', 'percent', 'percent_per_hour', 'tokens', 'usd']);
const NO_CHIP = 'it has no chip form';

/** The facts of `key` not yet drawn, marked drawn as they are taken. */
function taker(step: WalkStep, placed: Set<WalkFact>) {
  return (key: string) => {
    const facts = step.facts.filter((f) => f.key === key && !placed.has(f));
    for (const f of facts) placed.add(f);
    return facts;
  };
}

const sentence = (text: string, unplaced: PopoverPlan['unplaced'] = []): Pick<PopoverPlan, 'block' | 'renderer' | 'unplaced'> => ({ block: { kind: 'sentence', text }, renderer: 'sentence', unplaced });
const groups = (renderer: Renderer, list: PlanGroup[], unplaced: PopoverPlan['unplaced']): Pick<PopoverPlan, 'block' | 'renderer' | 'unplaced'> => ({ block: { kind: 'groups', groups: list.filter((g) => g.rows.length > 0) }, renderer, unplaced });
const plainHead = (word: string, badge: string | null = null, tone: BadgeTone = 'neutral'): PopoverPlan['head'] => ({ account: null, badge, chip: null, second: [], tone, word });

/**
 * A fact as list rows: one led by the fact's word, or one a chip where its chips carry labels of their own (the lane's
 * speed, thinking and effort). An empty list reads none, and a fact with no chip form is set aside.
 */
function rowsOf(f: WalkFact, record: DecisionRecord, at: number, unplaced: PopoverPlan['unplaced']): PlanRow[] {
  const chips = factChips(f, record, at);
  const label = lookup(ROW_LABEL, f.key) ?? keyWord(f.key);
  if (chips.length === 0) {
    if (Array.isArray(f.value)) return [{ chips: [plain('none', label)], label }];
    unplaced.push({ key: f.key, reason: NO_CHIP });
    return [];
  }
  if (f.key === 'lane') return chips.map((c) => ({ chips: [c], label: c.label ?? label }));
  return [{ chips, label }];
}

/**
 * The list renderer: the step's groups in their order, each row in its group's key order, and every fact no group
 * names in a trailing unlabeled group. A refusal's admission reads its reason as `walkLines` words it, the client's
 * version off the row.
 */
function listOf(s: Station, step: WalkStep, record: DecisionRecord, row: DecisionSummary | undefined, at: number): Pick<PopoverPlan, 'block' | 'renderer' | 'unplaced'> {
  const unplaced: PopoverPlan['unplaced'] = [];
  const marked = s.id.startsWith('partitions:') ? s.id.slice('partitions:'.length) : null;
  const refusal = step.step === 'admission' && record.outcome.refusal !== null ? walkLines(record, row).find((l) => l.step === STEP_NAME.admission) : undefined;
  const placed = new Set<WalkFact>();
  // The "on" row names the account the step acted on, so the facts about that account read without it again.
  const on = step.facts.find((f) => f.key === 'account_id')?.value;
  // A move's gate kept its destinations whole, so the no-fit count reads 0 whatever order the gate took, and the row
  // names that order instead.
  const gates = step.step === 'partitions' ? record.walk.slice(0, record.walk.indexOf(step)).filter((w) => w.step === 'fit') : [];
  const gate = gates[gates.length - 1];
  const rowsFor = (f: WalkFact): PlanRow[] => {
    placed.add(f);
    if (refusal !== undefined && f.key === 'reason') return [{ chips: refusal.chips, label: keyWord(f.key) }];
    if (gate !== undefined && f.key === 'no_fit') return [{ chips: [plain(`by the gate's ${verdictWord(gate.verdict)} order`, keyWord(f.key))], label: keyWord(f.key), ...(f.key === marked ? { marked: true } : {}) }];
    const fact = f.key !== 'account_id' && f.accountId !== null && f.accountId === on ? { ...f, accountId: null } : f;
    return rowsOf(fact, record, at, unplaced).map((r) => (f.key === marked ? { ...r, marked: true } : r));
  };
  const groupsOf = record.version < 2 ? LEGACY_GROUPS_OF : GROUPS_OF;
  const list: PlanGroup[] = (lookup(groupsOf, step.step) ?? []).map((g) => ({ label: g.label, rows: g.keys.flatMap((k) => step.facts.filter((f) => f.key === k).flatMap(rowsFor)) }));
  list.push({ label: null, rows: step.facts.filter((f) => !placed.has(f)).flatMap(rowsFor) });
  return groups('list', list, unplaced);
}

const numeric = (f: WalkFact) => typeof f.value === 'number' || (f.unit !== null && NUMERIC_UNITS.has(f.unit));

/**
 * The table renderer: a row per account in the facts' order, a column per key the accounts carry, and the step's loose
 * facts as rows above it. The pick's scores read the candidates table's Pace cells under its Pace header, and the Fit
 * table orders its columns window by window and tones its verdicts.
 */
function tableOf(step: WalkStep, record: DecisionRecord, at: number, now: number): Pick<PopoverPlan, 'block' | 'renderer' | 'unplaced'> {
  const unplaced: PopoverPlan['unplaced'] = [];
  const columns: PlanColumn[] = [];
  const rows = new Map<string, PlanTable['rows'][number]>();
  const loose: PlanRow[] = [];
  for (const f of step.facts) {
    if (f.accountId === null) {
      loose.push(...rowsOf(f, record, at, unplaced));
      continue;
    }
    const pace = step.step === 'pick' && f.key === 'score';
    let column = columns.find((c) => c.key === f.key);
    if (column === undefined) {
      column = { key: f.key, label: pace ? paceHeader(record) : keyWord(f.key), numeric: false };
      columns.push(column);
    }
    column.numeric ||= numeric(f);
    const account = rows.get(f.accountId) ?? { account: accountChip(f.accountId), cells: {} };
    rows.set(f.accountId, account);
    if (account.cells[f.key] !== undefined) {
      unplaced.push({ key: f.key, reason: 'a second value for one account' });
      continue;
    }
    const member = pace ? record.candidates.find((c) => c.accountId === f.accountId) : undefined;
    const cells = member === undefined ? null : candidateCells(member, record, now);
    const toned = step.step === 'fit' && f.key === 'verdict' && typeof f.value === 'string' ? { tone: verdictTone(f.value, 'fit') } : {};
    account.cells[f.key] = cells === null ? { text: valueText(f.key, f.value, f.unit, at), ...toned } : { text: cells.pace, ...(cells.paceTitle === null ? {} : { title: cells.paceTitle }) };
  }
  if (step.step === 'fit') {
    // The engine writes no runway for an unbounded one, so a row with a projection and no runway reads unbounded, as
    // the card's Runway does, and only a row the gate projected nothing for lacks the fact.
    const unbounded = [...rows.values()].filter((r) => Object.keys(r.cells).some((key) => key.startsWith('projection_')) && r.cells.runway_ms === undefined);
    if (unbounded.length > 0 && !columns.some((c) => c.key === 'runway_ms')) columns.push({ key: 'runway_ms', label: keyWord('runway_ms'), numeric: true });
    for (const r of unbounded) r.cells.runway_ms = { text: 'unbounded' };
    columns.sort((a, b) => fitRank(a.key) - fitRank(b.key));
  }
  // A cell the account lacks stays blank.
  const table = [...rows.values()].map((r) => ({ account: r.account, cells: Object.fromEntries(columns.map((c) => [c.key, r.cells[c.key] ?? null])) }));
  return { block: { kind: 'table', table: { columns, loose, rows: table, wraps: step.step === 'fit' } }, renderer: 'table', unplaced };
}

/**
 * The rung renderer: the reason, then the entry the rung found (the accepted one on a match, the deepest refused one
 * otherwise) on the member that holds it, then any fact left. The entry places the step's own depth, prefix, account
 * and hash facts.
 */
function rungOf(step: WalkStep, record: DecisionRecord, at: number, now: number): Pick<PopoverPlan, 'block' | 'renderer' | 'unplaced'> {
  const unplaced: PopoverPlan['unplaced'] = [];
  const placed = new Set<WalkFact>();
  const take = taker(step, placed);
  const rows = (fs: WalkFact[]) => fs.flatMap((f) => rowsOf(f, record, at, unplaced));
  const list: PlanGroup[] = [{ label: null, rows: rows(take('reason')) }];
  const entry = rungEntry(step, record);
  if (entry) {
    for (const key of ['depth', 'prefix_tokens', 'account_id']) take(key);
    for (const f of take('hash')) unplaced.push({ key: f.key, reason: 'drawn from the entry' });
    const holder = record.candidates.find((c) => [c.prefix.content, c.prefix.contentMatch, c.prefix.static, c.prefix.staticMatch].includes(entry));
    const until = warmUntil(entry, now);
    list.push({
      label: 'Entry',
      rows: [
        { chips: [], entry, label: 'hash' },
        { chips: [plain(fmtInt(entry.depth), 'depth')], label: 'depth' },
        { chips: [plain(fmtCompact(entry.prefixTokens), 'prefix')], label: 'prefix' },
        { chips: [plain(formatFact(entry.writtenAt, 'time', now), 'written')], label: 'written' },
        { chips: [plain(formatFact(entry.ttlMs, 'ms', now), 'TTL'), ...(until === null ? [] : [plain(until)])], label: 'TTL' },
        { chips: [plain(entry.live ? 'yes' : 'no', 'live')], label: 'live' },
        ...(entry.refusedBy === null ? [] : [{ chips: [plain(words(entry.refusedBy), 'refused by')], label: 'refused by' }]),
        ...(holder === undefined ? [] : [{ chips: [accountChip(holder.accountId, 'on')], label: 'on' }]),
      ],
    });
  } else {
    list[0].rows.push(...rows(['depth', 'prefix_tokens', 'account_id'].flatMap(take)));
    // A boundary's hash draws from the entry that names its warmth too, so without one the fact is set aside.
    for (const f of take('hash')) unplaced.push({ key: f.key, reason: 'no entry to draw it from' });
  }
  list.push({ label: null, rows: rows(step.facts.filter((f) => !placed.has(f))) });
  return groups('rung', list, unplaced);
}

/** The hold renderer: where it held, the account it held on, that account's slots against its cap, and the bound. */
function holdOf(step: WalkStep, record: DecisionRecord, at: number): Pick<PopoverPlan, 'block' | 'renderer' | 'unplaced'> {
  const unplaced: PopoverPlan['unplaced'] = [];
  const placed = new Set<WalkFact>();
  const take = taker(step, placed);
  const rows: PlanRow[] = take('place').map((f) => {
    const place = String(f.value);
    return { chips: [plain(lookup(PLACE_WORD, place) ?? words(place), keyWord(f.key))], label: keyWord(f.key) };
  });
  for (const f of take('account_id')) {
    rows.push(...rowsOf(f, record, at, unplaced));
    const member = record.candidates.find((c) => c.accountId === (f.accountId ?? f.value));
    if (member !== undefined) rows.push({ chips: [plain(`${member.inFlight} of ${member.inFlightCap}`, 'slot')], label: 'slot' });
  }
  // The session pin's exemption is placed rather than held, so its step carries no bound.
  rows.push(...take('bound_ms').flatMap((f) => rowsOf(f, record, at, unplaced)));
  return groups('hold', [{ label: null, rows }, { label: null, rows: step.facts.filter((f) => !placed.has(f)).flatMap((f) => rowsOf(f, record, at, unplaced)) }], unplaced);
}

function stationPlan(s: Station, record: DecisionRecord, row: DecisionSummary | undefined, now: number): PopoverPlan {
  const head: PopoverPlan['head'] = { account: null, badge: s.badge === '' ? null : s.badge, chip: null, second: s.second, tone: s.tone, word: s.head };
  const at = Date.parse(record.request.at);
  // The Move station stands for the rule that fired.
  const index = s.id === 'move' ? record.walk.findIndex((w) => w.step === s.step && w.verdict === 'fired') : s.index;
  const step = index === null || index < 0 ? null : record.walk[index];
  const folded = step !== null && s.id !== 'move' && step.verdict === 'not_run' ? walkLines(record, row).find((l) => l.collapsed?.includes(step.step)) : undefined;
  if (step !== null && folded?.collapsed !== undefined) {
    const unplaced = step.facts.map((f) => ({ key: f.key, reason: 'a rule the fired rule cut short reads the rules that did not run' }));
    return { block: { chips: folded.chips, kind: 'chips', marked: folded.collapsed.indexOf(step.step) }, head, renderer: 'chips', unplaced };
  }
  if (s.step === 'selection') return { head, ...sentence(SENTENCE.rest, (step?.facts ?? []).map((f) => ({ key: f.key, reason: 'the rest of the walk reads its sentence' }))) };
  if (step === null) return { head, ...sentence(SENTENCE.placeholder) };
  if (step.verdict === 'not_run' && step.facts.length === 0) return { head, ...sentence(SENTENCE.notRun) };
  if (step.step === 'reserve' && step.verdict === 'none') {
    // A booking of nothing has no shares to list, so the sentence says so and leaves its account to the card.
    const unplaced = step.facts.map((f) => ({ key: f.key, reason: 'a booking of nothing reads its sentence' }));
    return { head, ...sentence(record.outcome.kind === 'renewed' ? SENTENCE.reserveRenewed : SENTENCE.reserveNone, unplaced) };
  }
  switch (s.renderer) {
    case 'hold':
      return { head, ...holdOf(step, record, at) };
    case 'list':
      return { head, ...listOf(s, step, record, row, at) };
    case 'rung':
      return { head, ...rungOf(step, record, at, now) };
    case 'table':
      return { head, ...tableOf(step, record, at, now) };
    default: {
      // A step the console has no words for reads as chips, the one form every fact has.
      const unplaced: PopoverPlan['unplaced'] = [];
      const chips = step.facts.flatMap((f) => {
        const out = factChips(f, record, at);
        if (out.length === 0) unplaced.push({ key: f.key, reason: NO_CHIP });
        return out;
      });
      return { block: { chips, kind: 'chips', marked: null }, head, renderer: 'chips', unplaced };
    }
  }
}

/** The card's candidate columns after Account, for the "and N more" leaf's table of the members it stands for. */
function remainingOf(model: StripModel, record: DecisionRecord, now: number): PlanTable {
  const shown = new Set(model.leaves.map((l) => l.accountId));
  const windows = Object.entries(WINDOW_WORD).map(([key, label]) => ({ key, label, numeric: true }));
  const columns: PlanColumn[] = [
    { key: 'tier', label: 'Tier', numeric: true },
    ...windows,
    { key: 'projection', label: 'Projection', numeric: true },
    { key: 'runway', label: 'Runway', numeric: true },
    { key: 'in_flight', label: 'In flight', numeric: true },
    { key: 'pace', label: paceHeader(record), numeric: true },
    { key: 'prefix', label: 'Prefix', numeric: false },
    { key: 'verdict', label: 'Verdict', numeric: false },
  ];
  const rows = record.candidates
    .filter((c) => !shown.has(c.accountId))
    .map((c) => {
      const cells = candidateCells(c, record, now);
      // A window reads its utilization with its reset and its reading's source on hover. A key's one merged reading
      // stands under 5h with the other two blank.
      const window = (i: number) => {
        const lines = cells.windows[i];
        return lines === undefined ? null : { text: lines[0], ...(lines.length > 1 ? { title: lines.slice(1).join('\n') } : {}) };
      };
      const entry = cells.prefix.hash;
      const chip = cells.verdict.chip;
      return {
        account: cells.account,
        cells: {
          in_flight: { text: cells.inFlight },
          pace: { text: cells.pace, ...(cells.paceTitle === null ? {} : { title: cells.paceTitle }) },
          prefix: entry === null ? null : { text: shortHash(entry.hash), title: [hashTitle(entry), cells.prefix.until].filter((t) => t !== null).join('\n') },
          // A projection reads its first window, the others on hover, as a window cell reads its reset.
          projection: { text: cells.projection[0], ...(cells.projection.length > 1 ? { title: cells.projection.join('\n') } : {}) },
          runway: { text: cells.runway },
          tier: { text: cells.tier },
          verdict: { text: chip === null ? cells.verdict.badge : `${cells.verdict.badge} ${chip.value}`, ...(chip?.title === undefined ? {} : { title: chip.title }) },
          ...Object.fromEntries(windows.map((w, i) => [w.key, window(i)])),
        },
      };
    });
  return { columns, loose: [], rows, wraps: false };
}

function leafPlan(leaf: Leaf, model: StripModel, record: DecisionRecord, now: number): PopoverPlan {
  const member = leaf.accountId === null ? undefined : record.candidates.find((c) => c.accountId === leaf.accountId);
  if (member === undefined) {
    return { block: { kind: 'table', table: remainingOf(model, record, now) }, head: plainHead(leaf.badge), renderer: 'table', unplaced: [] };
  }
  const cells = candidateCells(member, record, now);
  const out = member.verdict === 'out';
  const entry = cells.prefix.hash;
  const chips: Chip[] = [
    plain(cells.plan, 'plan'),
    plain(cells.tier, 'tier'),
    plain(cells.inFlight, 'in flight'),
    ...(member.pace === null ? [] : [{ ...plain(cells.pace, 'Pace'), ...(cells.paceTitle === null ? {} : { title: cells.paceTitle }) }]),
    ...(!out && cells.verdict.chip !== null ? [cells.verdict.chip] : []),
    ...(entry === null ? [] : [{ label: 'prefix', mono: true, title: hashTitle(entry), value: shortHash(entry.hash) }]),
  ];
  // The console's three windows in their order, then any other the account reports, a key's worst bucket among them.
  const keys = [...Object.keys(WINDOW_WORD).filter((k) => has(member.windows, k)), ...Object.keys(member.windows).filter((k) => !has(WINDOW_WORD, k))];
  const windows: WindowRow[] = keys.map((k) => {
    const w = member.windows[k];
    return { age: ageWord(w.ageMs), left: fmtPct(1 - w.utilization), reset: w.reset === null ? 'no reset' : formatFact(w.reset, 'time', now), source: w.source, used: fmtPct(w.utilization), window: lookup(WINDOW_WORD, k) ?? keyWord(k) };
  });
  const title = out && cells.verdict.chip !== null ? cells.verdict.chip : plain(cells.verdict.badge);
  return { block: { chips, kind: 'candidate', windows }, head: { account: cells.account, badge: null, chip: title, second: [], tone: leaf.tone, word: member.accountId }, renderer: 'candidate', unplaced: [] };
}

function terminusPlan(t: Terminus, record: DecisionRecord, row: DecisionSummary | undefined, now: number): PopoverPlan {
  if (t.state !== 'taken') return { head: plainHead(t.word), ...sentence(SENTENCE.notReached) };
  const rows = outcomeRows(record, now, row).map((r) => ({ chips: r.chips, label: r.label, ...(r.title === undefined ? {} : { title: r.title }) }));
  return { head: plainHead(t.word, t.badge, t.tone), ...groups('outcome', [{ label: null, rows }], []) };
}

/**
 * The plan of the popover `anchor` opens on the strip `model` drew from `record`. An anchor the model does not hold
 * reads as a placeholder, since the record carries nothing there.
 */
export function anchorPlan(anchor: StripAnchor, model: StripModel, record: DecisionRecord, row: DecisionSummary | undefined, now: number): PopoverPlan {
  const leaf = anchor.kind === 'leaf' ? model.leaves[anchor.index] : undefined;
  if (leaf !== undefined) return leafPlan(leaf, model, record, now);
  const t = anchor.kind === 'terminus' ? model.termini.find((x) => x.kind === anchor.id) : undefined;
  if (t !== undefined) return terminusPlan(t, record, row, now);
  const s = anchor.kind === 'station' ? model.stations.find((x) => x.id === anchor.id) : undefined;
  if (s !== undefined) return stationPlan(s, record, row, now);
  return { head: plainHead(''), ...sentence(SENTENCE.placeholder) };
}
