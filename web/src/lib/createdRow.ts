/** The row a confirmed pairing created, while the list's read that should hold it is out: `reading` once that read's
 *  loading span starts, `unlisted` once it ended without the row. `named` holds once the address has named the id. */
export type CreatedRow = { id: string; named: boolean; phase: 'confirmed' | 'reading' | 'unlisted' };

/** What one render of the Accounts page hands the guard: the address's id, whether the loaded list holds the row and
 *  whether the list's read is out. */
type CreatedRowView = { accountId: string | undefined; listed: boolean; loading: boolean };

export const confirmedRow = (id: string): CreatedRow => ({ id, named: false, phase: 'confirmed' });

/** The guard's state after one render, null once it lets the id go, and the same object where nothing moved, since the
 *  page sets it during the render. An unlisted id goes once a read lists its row or once the address, having named the
 *  id, leaves it. */
export function nextCreatedRow(row: CreatedRow, view: CreatedRowView): CreatedRow | null {
  const named = row.named || view.accountId === row.id;
  let next = row;
  if (row.phase === 'unlisted') {
    if (view.listed || (named && view.accountId !== row.id)) return null;
  } else if (view.loading) {
    if (row.phase === 'confirmed') next = { ...row, phase: 'reading' };
  } else if (row.phase === 'reading') {
    if (view.listed) return null;
    next = { ...row, phase: 'unlisted' };
  }
  return next.named === named ? next : { ...next, named };
}
