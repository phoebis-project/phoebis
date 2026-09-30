import type { AutoMode } from '@/api/types';

const AUTO_MODE_LABEL: Record<AutoMode, string> = {
  client_stage1: 'client classifier, stage 1',
  client_stage2: 'client classifier, stage 2',
  server: 'server-side check requested',
  stripped: "server-side check dropped by this key's setting",
};

// The two checks answer in overlapping vocabularies, so each verdict is worded by the check that produced it.
const SERVER_VERDICT_LABEL: Record<string, string> = {
  flagged: 'flagged',
  none: 'no verdict returned',
  not_flagged: 'not flagged',
  skipped: 'skipped',
  unavailable: 'unavailable',
  unrecognized: 'unrecognized result',
  unsupported: 'unsupported',
};

const CLASSIFIER_VERDICT_LABEL: Record<string, string> = {
  flagged: 'flagged',
  not_flagged: 'not flagged',
  refused: 'refused',
  unreadable: 'unreadable answer',
};

// A verdict is whatever the ledger stored, so a lookup must not answer `constructor` or `__proto__` with a member
// every object inherits. The lib target predates `Object.hasOwn`.
const hasOwn = (o: object, key: string) => Object.prototype.hasOwnProperty.call(o, key);

/** The Request shape row's text. */
export function autoModeLabel(mode: AutoMode | null): string {
  return mode === null ? 'not requested' : AUTO_MODE_LABEL[mode];
}

/** The attempt line's text for a verdict, worded for the check that produced it. */
export function verdictLabel(mode: AutoMode | null, verdict: string, detail: string | null): string {
  const words = mode === 'server' ? SERVER_VERDICT_LABEL : mode === 'client_stage1' || mode === 'client_stage2' ? CLASSIFIER_VERDICT_LABEL : {};
  const label = hasOwn(words, verdict) ? words[verdict] : verdict;
  return detail ? `${label} (${detail})` : label;
}
