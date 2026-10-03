/** Delivers the latest value at most once per frame, however many arrive inside one; `flush` delivers now, `cancel` drops a pending one. */
export function coalesce<T>(deliver: (value: T) => void, schedule = (cb: () => void) => requestAnimationFrame(cb), cancel = (id: number) => cancelAnimationFrame(id)) {
  let pending: { value: T } | null = null;
  let id: number | null = null;
  const flush = () => {
    id = null;
    if (pending === null) return;
    const { value } = pending;
    pending = null;
    deliver(value);
  };
  return {
    cancel: () => {
      if (id !== null) cancel(id);
      id = null;
      pending = null;
    },
    flush,
    push: (value: T) => {
      pending = { value };
      if (id === null) id = schedule(flush);
    },
  };
}
