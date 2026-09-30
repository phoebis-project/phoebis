import type { AutoMode } from '@/api/types';

const AUTO_MODE_LABEL: Record<AutoMode, string> = {
  body_shape: 'refused for its body shape',
  server: 'asked for a server-side check under an on setting',
  stripped: 'carried the safeguards member under an off setting, which the dispatch drops',
};

/** The Request shape row's text. */
export function autoModeLabel(mode: AutoMode | null): string {
  return mode === null ? 'not requested' : AUTO_MODE_LABEL[mode];
}
