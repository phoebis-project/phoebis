import type { Api } from './client';
import { HttpApi } from './http';

export { CACHE_TTL_DEFAULTS, KEEPALIVE_DEFAULTS } from './client';
export { ApiError } from './http';
export * from './types';

/** The fixture-backed mock, for the smoke runner and the visual reference; never in the served bundle. */
export const MOCK = import.meta.env.VITE_PHOEBIS_MOCK === '1';

export const api: Api = MOCK ? new (await import('@/mock')).MockApi() : new HttpApi();
