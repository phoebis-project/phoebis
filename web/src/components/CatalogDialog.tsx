import { useRef } from 'react';
import { api } from '@/api';
import { IconRefresh } from '@/components/ui/icons';
import { Button, Dialog, Refusal, Skeleton } from '@/components/ui/primitives';
import { fmtAbsolute, fmtDate, fmtInt, fmtRelative } from '@/lib/format';
import { useAsync } from '@/lib/hooks';

const tokens = (n: number | null) => (n === null ? '-' : fmtInt(n));
// `created_at` reaches the console as the upstream sent it, which may be empty or not a date at all.
const released = (createdAt: string) => (Number.isNaN(Date.parse(createdAt)) ? createdAt || '-' : fmtDate(createdAt));

/** An account's live model list as the router last fetched it, read when the dialog opens and again on Refresh. */
export function CatalogDialog({ accountId, label, now, onClose, open }: { accountId: string; label: string; now: number; onClose: () => void; open: boolean }): JSX.Element {
  // Refresh asks the router to fetch the list anew, so the read it starts carries the flag and an opening's does not.
  const refreshNext = useRef(false);
  // Once per opening and account, a closed dialog answering null with no request, so a reopening starts empty.
  const read = useAsync(() => {
    if (!open) return Promise.resolve(null);
    const refresh = refreshNext.current;
    refreshNext.current = false;
    return refresh ? api.getAccountCatalog(accountId, true) : api.getAccountCatalog(accountId);
  }, [open, accountId]);
  // A failed read keeps the last list, so a failed Refresh draws its failure over the list it kept. A list read for
  // another opening or account draws nothing while the current read is out.
  const catalog = read.stale && read.error === null ? null : read.data;

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
            <Button
              size="sm"
              icon={<IconRefresh size={13} />}
              disabled={read.loading}
              onClick={() => {
                refreshNext.current = true;
                read.reload();
              }}
            >
              Refresh
            </Button>
          </div>
          {read.error !== null && <Refusal>{read.error}</Refusal>}
          {catalog === null ? (
            read.error === null && <Skeleton height={120} />
          ) : catalog.error ? (
            <Refusal>{catalog.error}</Refusal>
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
