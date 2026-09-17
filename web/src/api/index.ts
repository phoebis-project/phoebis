import { MockApi } from '@/mock';
import type { Api } from './client';

export type { Api, CreateKeyInput, KeyInput, PoolInput } from './client';
export { CACHE_TTL_DEFAULTS, KEEPALIVE_TUNING_DEFAULTS } from './client';
export * from './types';

export const api: Api = new MockApi();
