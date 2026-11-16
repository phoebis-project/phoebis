import { listWords } from '@/lib/format';

/**
 * A control of the Analytics filter bar that a card's read can pass by: a filter, or the Include probes toggle. The
 * range stands in force always, and the one card that ignores it, Member budgets, says so in a sentence of its own.
 */
export type AnalyticsControl = 'accounts' | 'agent' | 'clients' | 'keys' | 'models' | 'pools' | 'probes' | 'session' | 'userAgents';
/** A card, or a part of one, whose read applies fewer controls than the bar holds. */
export type AnalyticsCard = 'accountsTab' | 'burn' | 'guardrails' | 'placements' | 'refusals' | 'rewrites' | 'rewritesHeld' | 'unusedAllowance';

/** Per card, the controls its read applies: the members the page's call sends to the card's route. */
const APPLIED: Record<AnalyticsCard, readonly AnalyticsControl[]> = {
  // The accounts route reads the range alone, and the page keeps the selected accounts' rows itself.
  accountsTab: ['accounts'],
  // The burn route counts probe rows always.
  burn: ['accounts', 'keys', 'pools'],
  guardrails: [],
  // The route takes a probes member, which the page's call does not send.
  placements: ['accounts', 'clients', 'keys', 'models', 'pools', 'userAgents'],
  // The page's query carries no session and no agent.
  refusals: ['accounts', 'clients', 'keys', 'models', 'pools', 'probes', 'userAgents'],
  // The moves and the rewrite tokens come from the account rollups, which carry no pool.
  rewrites: ['accounts'],
  rewritesHeld: ['accounts', 'pools'],
  unusedAllowance: ['accounts'],
};

/** The filters in the bar's order, which a note keeps, each by the word a note names it with. */
const FILTERS: readonly { control: AnalyticsControl; word: string }[] = [
  { control: 'accounts', word: 'Accounts' },
  { control: 'keys', word: 'Keys' },
  { control: 'models', word: 'Models' },
  { control: 'pools', word: 'Pools' },
  { control: 'clients', word: 'Clients' },
  { control: 'userAgents', word: 'User agents' },
  { control: 'agent', word: 'Agent' },
  { control: 'session', word: 'Session' },
];

/** The note that names each control in force that a card's read ignores, in the bar's order, null where none is. */
export function ignoredNote(card: AnalyticsCard, inForce: ReadonlySet<AnalyticsControl>): string | null {
  const ignored = (control: AnalyticsControl) => inForce.has(control) && !APPLIED[card].includes(control);
  const filters = FILTERS.filter((f) => ignored(f.control)).map((f) => f.word);
  const toggle = ignored('probes');
  if (filters.length === 0 && !toggle) return null;
  // The filters share one plural noun, and the toggle follows them under a noun of its own.
  const run = filters.length === 0 ? null : `${listWords(filters)} ${filters.length === 1 ? 'filter' : 'filters'}`;
  const controls = run === null ? 'Include probes toggle' : toggle ? `${run} and the Include probes toggle` : run;
  return `The ${controls} above ${filters.length + (toggle ? 1 : 0) === 1 ? 'does' : 'do'} not apply here`;
}
