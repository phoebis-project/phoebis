import type { RequestEvent } from '@/api/types';

/** The subset of a request the drawer's trail is filtered against. */
export interface TrailRequest {
  agentId: string | null;
  canonicalModel: string;
  id: string;
  keyId: string | null;
  sessionId: string | null;
}

/**
 * The session's events by time alone, so a move or a release belongs to `request` only when it is the same
 * agent's pin, keyed by key, session, agent, and canonical model; a model's rejection or permit only when it
 * carries the request's id.
 */
export function requestTrail(events: RequestEvent[], request: TrailRequest): RequestEvent[] {
  return events.filter((e) =>
    e.kind === 'pin_migrated' || e.kind === 'pin_expired'
      ? e.detail !== null &&
        e.keyId === request.keyId &&
        e.sessionId === request.sessionId &&
        e.model === request.canonicalModel &&
        e.detail.agent_id === request.agentId
      : (e.kind === 'model_rejected' || e.kind === 'model_permitted') && e.detail?.source_request_id === request.id,
  );
}
