import { type ReactNode, useState } from 'react';
import { Link } from 'react-router-dom';
import { Button, Card } from '@/components/ui/primitives';
import { fmtAbsolute, fmtRelative } from '@/lib/format';

/** One scope an account cools under: an account's own cooldown row with its id. */
export type CoolingRow = { accountId: string; reason: string; scope: string; until: string };

const MODEL_SCOPE_PREFIX = 'model:';

/** `Whole account` for the account scope, `Fast mode` and `Fast mode credits` for the fast lane's rate limit and
 *  usage-credits refusal, the model id for `model:<id>`. */
export function scopeLabel(scope: string): string {
  if (scope === 'account') return 'Whole account';
  if (scope === 'fast') return 'Fast mode';
  if (scope === 'fast_credits') return 'Fast mode credits';
  return scope.startsWith(MODEL_SCOPE_PREFIX) ? scope.slice(MODEL_SCOPE_PREFIX.length) : scope;
}

/**
 * The rows an account is cooling under, with a clear button per row when `onClear` is given. `showAccount` names the
 * account each row belongs to where the rows span several accounts.
 */
export function CoolingDown({ busy = false, now, onClear, rows, showAccount = false, title }: { busy?: boolean; now: number; onClear?: (row: CoolingRow) => void; rows: CoolingRow[]; showAccount?: boolean; title: ReactNode }): JSX.Element {
  // Null until the operator toggles, so a block that was empty when it mounted still opens once a row arrives.
  const [toggled, setToggled] = useState<boolean | null>(null);
  const open = toggled ?? rows.length > 0;
  const earliest = rows.reduce<string | null>((min, r) => (min === null || Date.parse(r.until) < Date.parse(min) ? r.until : min), null);
  return (
    <Card
      className="cooling-down"
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
                    {showAccount && <th className="col-fit">Account</th>}
                    <th className="col-fit">Scope</th>
                    <th className="col-fill">Reason</th>
                    <th className="col-fit">Until</th>
                    {onClear && <th className="col-fit" />}
                  </tr>
                </thead>
                <tbody>
                  {rows.map((r) => (
                    <tr key={`${r.accountId}/${r.scope}`}>
                      {showAccount && (
                        <td className="col-fit">
                          <Link to={`/accounts/${r.accountId}`}>{r.accountId}</Link>
                        </td>
                      )}
                      <td className="col-fit">{r.scope === 'account' ? scopeLabel(r.scope) : <span className="mono">{scopeLabel(r.scope)}</span>}</td>
                      <td className="mono col-fill" title={r.reason}>{r.reason}</td>
                      <td className="num col-fit">
                        {fmtAbsolute(r.until)}
                        <span className="sub">{fmtRelative(r.until, now)}</span>
                      </td>
                      {onClear && (
                        <td className="right col-fit">
                          <Button size="sm" disabled={busy} onClick={() => onClear(r)}>
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
