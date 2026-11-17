import { type Dispatch, type ReactNode, type SetStateAction, useMemo } from 'react';
import { api } from '@/api';
import type { Account, ApiKey, FeatureCall, FeatureCallQuery } from '@/api/types';
import { FEATURE_OUTCOME_VALUES, FEATURE_WORD_VALUES } from '@/api/types';
import { useAppSettings } from '@/app/settings';
import { usePaging } from '@/app/useFollow';
import { FilterBar } from '@/components/FilterBar';
import type { Option } from '@/components/ui/OptionList';
import { PagerNav } from '@/components/ui/PagerNav';
import { PopoverHost, useHoldHost } from '@/components/ui/Popover';
import { Card, EmptyState, ErrorState, SkeletonRows } from '@/components/ui/primitives';
import type { RangeValue } from '@/lib/address';
import { clock } from '@/lib/clock';
import { docsHref } from '@/lib/docs';
import { FEATURE_CALLS_DIMENSIONS } from '@/lib/filterBar';
import { effectiveInterval, rowsRefreshTitle } from '@/lib/follow';
import { fmtAbsolute, fmtDatedTime, fmtMs } from '@/lib/format';
import { FEATURE_LABEL } from '@/lib/forms';
import { useAsync } from '@/lib/hooks';

const COLUMNS = 8;
/** A longer path shows its first 24 characters, an ellipsis and its last 23, as long as the longest whole one. */
const PATH_SHOWN = 48;
const cutPath = (path: string) => (path.length > PATH_SHOWN ? `${path.slice(0, 24)}…${path.slice(-23)}` : path);

export interface FeatureCallFilters {
  accounts: string[];
  features: string[];
  keys: string[];
  outcomes: string[];
}
export const NO_FEATURE_CALL_FILTERS: FeatureCallFilters = { accounts: [], features: [], keys: [], outcomes: [] };

const FEATURE_OPTIONS: Option[] = FEATURE_WORD_VALUES.map((f) => ({ label: FEATURE_LABEL[f], value: f }));
const OUTCOME_OPTIONS: Option[] = FEATURE_OUTCOME_VALUES.map((o) => ({ label: o, value: o }));

/**
 * A failed call whose answer ended behind its head holds no code, so its status reads with the note, the one row where
 * a 200 still failed.
 */
function statusOf(call: FeatureCall): ReactNode {
  switch (call.outcome) {
    case 'canceled':
      return 'canceled';
    case 'failed':
      return call.errorCode !== null ? (
        <span className="mono">{call.errorCode}</span>
      ) : (
        <>
          {call.status ?? '-'} <span className="muted">ended early</span>
        </>
      );
    case 'forwarded':
      return call.status ?? '-';
  }
}

/**
 * The Requests view's Feature calls section: a bar with the view's range, four filters and Refresh, the table of the
 * calls the router sent to a feature host under an account, newest first, and the pager over the route's cursor. The
 * page holds the filters and the interval out of the address, so a link carries the range alone and a look at the
 * other section keeps them. A row opens nothing, since the table shows all it holds.
 */
export function FeatureCallsTable({
  accounts,
  disabled,
  filters,
  intervalMs,
  keys,
  onFiltersChange,
  onIntervalChange,
  onRangeChange,
  range,
  refresh,
  span,
}: {
  accounts: readonly Account[];
  /** The reason Add filter gives a dimension whose option list did not load. */
  disabled: Record<string, string>;
  filters: FeatureCallFilters;
  /** The auto reload interval in milliseconds, 0 for off. */
  intervalMs: number;
  keys: readonly ApiKey[];
  onFiltersChange: Dispatch<SetStateAction<FeatureCallFilters>>;
  onIntervalChange: (ms: number) => void;
  onRangeChange: (range: RangeValue) => void;
  range: RangeValue;
  /** The page's Refresh, which takes the range again for both sections, and why it reads nothing new. */
  refresh: { disabled: string | undefined; onRefresh: () => void };
  /** The range's window in Unix milliseconds, whose end a custom range's read sends and a preset's leaves out. */
  span: { from: number; to: number };
}): JSX.Element {
  const { timeFormat } = useAppSettings();
  const { cause: holdCause, host } = useHoldHost();
  // A cursor belongs to the range and the filters it was read under, so a change of either starts the walk again.
  const address = JSON.stringify([range, span.from, span.to, filters]);
  const { cursor, reset, setPaging, stack } = usePaging(address);
  const custom = range.preset === 'custom';
  // A preset's read sends no end, so auto reload and a section opened long after the page took its range reach the
  // calls received since, and a cursor page is bounded by its cursor. Keyed on the span itself, so Refresh on a custom
  // range, which keeps its instants, still reads the window again.
  const query = useMemo<FeatureCallQuery>(
    () => ({ accounts: filters.accounts, cursor, features: FEATURE_WORD_VALUES.filter((f) => filters.features.includes(f)), from: span.from, keys: filters.keys, outcomes: FEATURE_OUTCOME_VALUES.filter((o) => filters.outcomes.includes(o)), to: custom ? span.to : undefined }),
    [cursor, custom, filters, span],
  );
  // The rows under the reader would move on an older page, so its cursor holds the reload until the first page returns.
  const calls = useAsync(() => api.listFeatureCalls(query), [query], { paused: holdCause !== null || cursor !== undefined, refreshMs: effectiveInterval(intervalMs, refresh.disabled !== undefined) });
  const rows = calls.data?.featureCalls ?? [];
  const nextCursor = calls.data?.nextCursor ?? null;
  const keyNames = useMemo(() => new Map(keys.map((k) => [k.id, k.name])), [keys]);
  const accountLabels = useMemo(() => new Map(accounts.map((a) => [a.id, a.label])), [accounts]);
  const options = useMemo<Record<string, Option[]>>(
    () => ({ accounts: accounts.map((a) => ({ label: a.label, value: a.id })), features: FEATURE_OPTIONS, keys: keys.map((k) => ({ label: k.name, value: k.id })), outcomes: OUTCOME_OPTIONS }),
    [accounts, keys],
  );
  const filtered = Object.values(filters).some((values) => values.length > 0);
  const update = (patch: Partial<FeatureCallFilters & { range: RangeValue }>) => {
    const { range: next, ...picked } = patch;
    if (next !== undefined) onRangeChange(next);
    if (Object.keys(picked).length > 0) onFiltersChange((f) => ({ ...f, ...picked }));
  };
  const goNext = () => {
    if (nextCursor !== null) setPaging({ cursor: nextCursor, for: address, stack: [...stack, cursor] });
  };
  const goPrevious = () => {
    const earlier = [...stack];
    const previous = earlier.pop();
    setPaging({ cursor: previous, for: address, stack: earlier });
  };

  return (
    <PopoverHost.Provider value={host}>
      <FilterBar
        address={address}
        dimensions={FEATURE_CALLS_DIMENSIONS}
        disabled={disabled}
        filters={{ ...filters, range }}
        options={options}
        range={{ now: clock.now, window: span }}
        refresh={{
          disabled: refresh.disabled,
          intervalMs,
          onIntervalChange,
          onRefresh: refresh.onRefresh,
          paused: holdCause ?? (cursor !== undefined ? 'olderPage' : null),
          refreshTitle: custom ? rowsRefreshTitle(true, range.from, range.to) : 'Reads the list again now',
        }}
        update={update}
      />
      <Card padded={false} className="card--table">
        <div className="table-wrap">
          <table className={`table requests__table${timeFormat === '12h' ? ' requests__table--12h' : ''}`}>
            <thead>
              <tr>
                <th className="col-fill">Time</th>
                <th className="col-fit">Key</th>
                <th className="col-fit">Feature</th>
                <th className="col-fit">Account</th>
                <th className="col-fit">Method</th>
                <th className="col-fit">Path</th>
                <th className="col-fit">Status</th>
                <th className="right col-fit">Duration</th>
              </tr>
            </thead>
            <tbody>
              {calls.loading && calls.data === null
                ? <SkeletonRows count={8} colSpan={COLUMNS} />
                : rows.map((c) => (
                    <tr key={c.id}>
                      <td className="num requests__time col-fill" title={fmtAbsolute(c.receivedAt)}>
                        {fmtDatedTime(c.receivedAt)}
                        <span className="sub mono" title={c.id}>{c.id}</span>
                      </td>
                      <td className="mono col-fit" title={c.keyId}>{keyNames.get(c.keyId) ?? c.keyId}</td>
                      <td className="col-fit">{FEATURE_LABEL[c.feature]}</td>
                      <td className="col-fit" title={c.accountId}>{accountLabels.get(c.accountId) ?? c.accountId}</td>
                      <td className="mono col-fit">{c.method}</td>
                      <td className="mono col-fit" title={c.path}>{cutPath(c.path)}</td>
                      <td className="col-fit">{statusOf(c)}</td>
                      <td className="right num col-fit">{fmtMs(c.durationMs)}</td>
                    </tr>
                  ))}
              {!calls.loading && rows.length === 0 && (
                <tr className="table__detail">
                  <td colSpan={COLUMNS}>
                    {calls.error ? (
                      <ErrorState title="Could not load the feature calls" body={calls.error} retry={calls.reload} />
                    ) : filtered ? (
                      <EmptyState title="No feature calls match" body="Widen the range or clear a filter." />
                    ) : (
                      // The range always narrows the list, so the title names it: the router may hold calls outside it.
                      <EmptyState
                        title="No feature calls in this range"
                        body="A row appears once a key's pool selects an account for a feature and a client in a feature setup uses it."
                        action={
                          <a href={docsHref('claude-ai-features')} target="_blank" rel="noreferrer">
                            How the claude.ai features work
                          </a>
                        }
                      />
                    )}
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
        <footer className="pager">
          <PagerNav atFirst={stack.length === 0} atLast={nextCursor === null} onFirst={reset} onNext={goNext} onPrevious={goPrevious} />
        </footer>
      </Card>
    </PopoverHost.Provider>
  );
}
