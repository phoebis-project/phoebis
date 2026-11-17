import type { PoolGroup } from './forms';

/** Each pool dialog group's rail label and the parent it sits under; `POOL_GROUPS` holds their order. */
export const POOL_RAIL: Record<PoolGroup, { label: string; parent: string }> = {
  autoMode: { label: 'Auto mode', parent: 'Pool' },
  bodyCapture: { label: 'Body capture', parent: 'Visibility' },
  cacheDiagnostics: { label: 'Diagnostics', parent: 'Visibility' },
  cacheTtl: { label: 'Cache TTL', parent: 'Caching' },
  failover: { label: 'Failover', parent: 'Load' },
  guardrails: { label: 'Guardrails', parent: 'Pool' },
  identity: { label: 'Identity', parent: 'Pool' },
  keepalive: { label: 'Keepalive', parent: 'Caching' },
  keys: { label: 'Keys', parent: 'Pool' },
  members: { label: 'Members', parent: 'Pool' },
  models: { label: 'Models', parent: 'Pool' },
  overage: { label: 'Overage and plan end', parent: 'Load' },
  pinning: { label: 'Pinning', parent: 'Routing' },
  placement: { label: 'Placement', parent: 'Routing' },
  selection: { label: 'Selection', parent: 'Routing' },
  shedding: { label: 'Load shedding', parent: 'Load' },
  slowDown: { label: 'Slow-down', parent: 'Load' },
  utilization: { label: 'Windows', parent: 'Load' },
};
