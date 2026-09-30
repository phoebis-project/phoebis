import type { AutoMode } from '@/api/types';

const AUTO_MODE_LABEL: Record<AutoMode, string> = {
  body_shape: 'refused for its body shape',
  server: 'server-side check requested',
  stripped: "server-side check dropped by this key's setting",
};

/** The Request shape row's text. */
export function autoModeLabel(mode: AutoMode | null): string {
  return mode === null ? 'not requested' : AUTO_MODE_LABEL[mode];
}
