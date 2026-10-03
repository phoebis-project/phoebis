import type { PoolGroup } from './forms';

/** Each pool dialog group's rail label and the parent it sits under; `POOL_GROUPS` holds their order. */
export const POOL_RAIL: Record<PoolGroup, { label: string; parent: string }> = {
  autoMode: { label: 'Auto mode', parent: 'Pool' },
  bodyCapture: { label: 'Body capture', parent: 'Visibility' },
  cacheDiagnostics: { label: 'Cache diagnostics', parent: 'Visibility' },
  cacheTtl: { label: 'Cache TTL', parent: 'Caching' },
  failover: { label: 'Failover', parent: 'Load' },
  identity: { label: 'Identity', parent: 'Pool' },
  keepalive: { label: 'Keepalive', parent: 'Caching' },
  keys: { label: 'Keys', parent: 'Pool' },
  members: { label: 'Members', parent: 'Pool' },
  models: { label: 'Models', parent: 'Pool' },
  overage: { label: 'Overage and plan end', parent: 'Load' },
  pinning: { label: 'Pinning', parent: 'Routing' },
  placement: { label: 'Placement', parent: 'Routing' },
  probeTuning: { label: 'Probe tuning', parent: 'Caching' },
  selection: { label: 'Selection', parent: 'Routing' },
  shedding: { label: 'Load shedding', parent: 'Load' },
  utilization: { label: 'Utilization thresholds', parent: 'Load' },
};
