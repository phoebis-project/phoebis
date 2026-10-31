import type { ApiKey, Pool } from '@/api/types';

/** A key with no override of its own captures for as long as its pool says. */
export const effectiveCaptureHours = (key: Pick<ApiKey, 'bodyCaptureHours'>, pool: Pick<Pool, 'bodyCaptureHours'> | null): number => key.bodyCaptureHours ?? pool?.bodyCaptureHours ?? 0;
