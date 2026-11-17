import { has, lookup } from '@/lib/counts';

/** The keys the upstream is known to send, in the order the console lists them. */
export const TRANSFORMATION_KEYS = [
  'thinking_dropped.end_user_binding_mismatch',
  'thinking_dropped.model_binding_mismatch',
  'thinking_dropped.organization_binding_mismatch',
  'thinking_dropped.prefix_binding_mismatch',
  'thinking_mismatch_allowed.prefix_binding_mismatch',
] as const;

const TRANSFORMATION_LABEL: Record<(typeof TRANSFORMATION_KEYS)[number], string> = {
  'thinking_dropped.end_user_binding_mismatch': 'Dropped, other end user',
  'thinking_dropped.model_binding_mismatch': 'Dropped, other model',
  'thinking_dropped.organization_binding_mismatch': 'Dropped, other organization',
  'thinking_dropped.prefix_binding_mismatch': 'Dropped, prefix changed',
  'thinking_mismatch_allowed.prefix_binding_mismatch': 'Let through, prefix changed',
};

/** A key's label, and the key as sent where the console does not know it. */
export const transformationLabel = (key: string): string => lookup(TRANSFORMATION_LABEL, key) ?? key;

/**
 * Whether the key names one of the two account reasons, under which the upstream dropped a block made for another end
 * user or another organization. The test reads the key's suffix as the router's rollup does: a word of a key may hold
 * a dot, so a split at the first dot could cut the type.
 */
export const accountBound = (key: string): boolean => key.endsWith('.end_user_binding_mismatch') || key.endsWith('.organization_binding_mismatch');

/** A map's keys in the order the console lists them: the known keys in their order, then every other key by name. */
export function transformationOrder(map: Readonly<Record<string, unknown>>): string[] {
  const known: readonly string[] = TRANSFORMATION_KEYS;
  return [...known.filter((key) => has(map, key)), ...Object.keys(map).filter((key) => !known.includes(key)).sort()];
}
