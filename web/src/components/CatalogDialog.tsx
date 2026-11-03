import { useCallback, useEffect, useRef, useState } from 'react';
import { api } from '@/api';
import type { AccountCatalog } from '@/api/types';
import { IconRefresh, IconWarning } from '@/components/ui/icons';
import { Button, Dialog, Skeleton } from '@/components/ui/primitives';
import { fmtAbsolute, fmtDate, fmtInt, fmtRelative, messageOf } from '@/lib/format';

const tokens = (n: number | null) => (n === null ? '-' : fmtInt(n));
// `created_at` reaches the console as the upstream sent it, which may be empty or not a date at all.
const released = (createdAt: string) => (Number.isNaN(Date.parse(createdAt)) ? createdAt || '-' : fmtDate(createdAt));

function Failure({ message }: { message: string }) {
  return (
    <div className="callout callout--critical" role="alert">
      <IconWarning size={16} />
      <div>{message}</div>
    </div>
  );
}

/** An account's live model list as the router last fetched it, read when the dialog opens and again on Refresh. */
export function CatalogDialog({ accountId, label, now, onClose, open }: { accountId: string; label: string; now: number; onClose: () => void; open: boolean }): JSX.Element {
  const [catalog, setCatalog] = useState<AccountCatalog | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  // Only the newest read lands: a Refresh overtakes the read the dialog opened with, and a close drops both.
  const latest = useRef(0);

  const read = useCallback(
    (refresh: boolean) => {
      const seq = ++latest.current;
      setLoading(true);
      setFailure(null);
      (refresh ? api.getAccountCatalog(accountId, true) : api.getAccountCatalog(accountId)).then(
        (answer) => {
          if (seq !== latest.current) return;
          setCatalog(answer);
          setLoading(false);
        },
        (e: unknown) => {
          if (seq !== latest.current) return;
          setFailure(messageOf(e));
          setLoading(false);
        },
      );
    },
    [accountId],
  );

  // The dialog reads once per open and account.
  useEffect(() => {
    if (!open) return;
    // eslint-disable-next-line react-hooks/set-state-in-effect -- each open starts from an empty catalog, which the read it starts fills
    setCatalog(null);
    read(false);
    return () => {
      latest.current += 1;
    };
  }, [open, read]);

  // Built while open alone: the account detail renders the closed dialog on its clock, and each row formats a date.
  const models = open && catalog ? [...catalog.models].sort((a, b) => a.id.localeCompare(b.id)) : [];
  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={`Catalog of ${label}`}
      width={960}
      footer={
        <Button variant="primary" onClick={onClose}>
          Close
        </Button>
      }
    >
      {open && (
        <>
          <div className="row">
            <span className="muted" title={catalog ? fmtAbsolute(catalog.fetchedAt) : undefined}>
              {catalog ? `As of ${fmtRelative(catalog.fetchedAt, now)}` : 'Reading the catalog…'}
            </span>
            <Button size="sm" icon={<IconRefresh size={13} />} disabled={loading} onClick={() => read(true)}>
              Refresh
            </Button>
          </div>
          {failure ? (
            <Failure message={failure} />
          ) : catalog === null ? (
            <Skeleton height={120} />
          ) : catalog.error ? (
            <Failure message={catalog.error} />
          ) : models.length === 0 ? (
            <p className="muted">The upstream lists no models.</p>
          ) : (
            <div className="table-wrap">
              <table className="table">
                <thead>
                  <tr>
                    <th className="col-fill">Model</th>
                    <th className="col-fill">Name</th>
                    <th className="col-fit">Released</th>
                    <th className="right col-fit">Input tokens</th>
                    <th className="right col-fit">Output tokens</th>
                    <th className="col-fit">Effort levels</th>
                  </tr>
                </thead>
                <tbody>
                  {models.map((m) => (
                    <tr key={m.id}>
                      <td className="col-fill" title={m.id}>
                        <div className="catalog__model">
                          <span className="mono catalog__id">{m.id}</span>
                        </div>
                      </td>
                      <td className="col-fill" title={m.displayName || undefined}>{m.displayName || '-'}</td>
                      <td className="num col-fit">{released(m.createdAt)}</td>
                      <td className="right num col-fit">{tokens(m.maxInputTokens)}</td>
                      <td className="right num col-fit">{tokens(m.maxTokens)}</td>
                      <td className="col-fit">{m.effortLevels.length ? m.effortLevels.join(', ') : '-'}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </>
      )}
    </Dialog>
  );
}
