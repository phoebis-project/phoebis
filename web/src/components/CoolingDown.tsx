import { useState } from 'react';
import { Link } from 'react-router-dom';
import { Button, Card } from '@/components/ui/primitives';
import { fmtAbsolute, fmtRelative } from '@/lib/format';

/** One scope an upstream cools under: an account's own cooldown row with its id, or a models-state row as is. */
export type CoolingRow = { accountId: string; reason: string; scope: string; until: string };

const MODEL_SCOPE_PREFIX = 'model:';

/** `Whole upstream` for the account scope, the model id for `model:<id>`. */
export function scopeLabel(scope: string): string {
  if (scope === 'account') return 'Whole upstream';
  return scope.startsWith(MODEL_SCOPE_PREFIX) ? scope.slice(MODEL_SCOPE_PREFIX.length) : scope;
}

/**
 * The rows an upstream (or a model group) is cooling under, with a clear button per row when `onClear` is given.
 * A model group's rows all share its scope, so `showUpstream` names the upstream each row belongs to.
 */
export function CoolingDown({ now, onClear, rows, showUpstream = false, title }: { now: number; onClear?: (row: CoolingRow) => void; rows: CoolingRow[]; showUpstream?: boolean; title: string }): JSX.Element {
  // Null until the operator toggles, so a block that was empty when it mounted still opens once a row arrives.
  const [toggled, setToggled] = useState<boolean | null>(null);
  const open = toggled ?? rows.length > 0;
  const earliest = rows.reduce<string | null>((min, r) => (min === null || Date.parse(r.until) < Date.parse(min) ? r.until : min), null);
  return (
    <Card
      title={title}
      subtitle={
        earliest === null ? (
          'Nothing cooling down'
        ) : (
          <span title={fmtAbsolute(earliest)}>
            {rows.length} {rows.length === 1 ? 'scope' : 'scopes'} cooling, the earliest ends {fmtRelative(earliest, now)}
          </span>
        )
      }
      actions={
        <Button size="sm" variant="ghost" aria-expanded={open} onClick={() => setToggled(!open)}>
          {open ? 'Hide' : 'Show'}
        </Button>
      }
    >
      {open && (
        <>
          {rows.length > 0 && (
            <div className="table-wrap">
              <table className="table">
                <thead>
                  <tr>
                    {showUpstream && <th>Upstream</th>}
                    <th>Scope</th>
                    <th>Reason</th>
                    <th>Until</th>
                    {onClear && <th />}
                  </tr>
                </thead>
                <tbody>
                  {rows.map((r) => (
                    <tr key={`${r.accountId}/${r.scope}`}>
                      {showUpstream && (
                        <td>
                          <Link to={`/upstreams/${r.accountId}`}>{r.accountId}</Link>
                        </td>
                      )}
                      <td>{r.scope === 'account' ? scopeLabel(r.scope) : <span className="mono">{scopeLabel(r.scope)}</span>}</td>
                      <td className="mono">{r.reason}</td>
                      <td className="num">
                        {fmtAbsolute(r.until)}
                        <span className="sub">{fmtRelative(r.until, now)}</span>
                      </td>
                      {onClear && (
                        <td className="right">
                          <Button size="sm" onClick={() => onClear(r)}>
                            Clear
                          </Button>
                        </td>
                      )}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
          <p className="muted detail__note">
            A deadline is the upstream's own reset or retry-after plus a one-second margin, or the configured fallback when the upstream named none. A deadline passing promises nothing about availability: the next request finds out.
          </p>
        </>
      )}
    </Card>
  );
}
