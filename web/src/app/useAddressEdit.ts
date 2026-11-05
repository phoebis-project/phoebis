import { useEffect } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';
import { EDIT_PARAM, GROUP_PARAM } from '@/lib/address';
import { addressTo, useLatest } from '@/lib/hooks';
import { useSession } from './session';

/**
 * The address's one-shot `edit` and `group`: once `items` has loaded, the entry is replaced by the address without
 * both, so closing the dialog, a reload or Back does not open it again, and the named item opens on the named group, or
 * the toast says that no item of `noun` carries the id. `open` is read through a ref, so the effect keys on the list and
 * the address alone, and each page guards the group and picks its default group there.
 */
export function useAddressEdit<T extends { id: string }>(items: readonly T[] | null, noun: string, open: (item: T, group: string | null) => void): void {
  const location = useLocation();
  const navigate = useNavigate();
  const { notify } = useSession();
  const openRef = useLatest(open);
  useEffect(() => {
    const params = new URLSearchParams(location.search);
    const edit = params.get(EDIT_PARAM);
    if (edit === null || !items) return;
    const group = params.get(GROUP_PARAM);
    params.delete(EDIT_PARAM);
    params.delete(GROUP_PARAM);
    navigate(addressTo(location.hash, location.pathname, params), { replace: true });
    const item = items.find((x) => x.id === edit);
    if (!item) {
      notify(`${noun} ${edit} not found`);
      return;
    }
    openRef.current(item, group);
  }, [items, location, navigate, noun, notify, openRef]);
}
