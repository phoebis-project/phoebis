import { type ForwardedRef, forwardRef, type ReactNode, type Ref, useRef, useState } from 'react';
import { AddFilter } from '@/components/AddFilter';
import { ChipLine } from '@/components/ChipLine';
import { RangeControl } from '@/components/RangeControl';
import { RefreshControl, type RefreshControlProps } from '@/components/RefreshControl';
import type { Option, Preset } from '@/components/ui/OptionList';
import { SearchInput } from '@/components/ui/primitives';
import type { RangeValue } from '@/lib/address';
import { chipsOf, type Dimension, emptyValue, withoutValue } from '@/lib/filterBar';
import { useCommittedText } from '@/lib/hooks';

export interface FilterBarProps<F extends object> {
  /** The view's address from `useQueryFilters`, whose change folds an expanded chip line again. */
  address: string;
  /** The extras in the view's order: the group-by, Columns, Sort, Refresh all, the primary action last. */
  children?: ReactNode;
  /** The right edge's count ("25 shown, more available", or a node with the attention part in the critical ink), which Analytics leaves out. */
  count?: ReactNode;
  /**
   * The widest text the count draws in the view's usual range, such as a freshly read page's on Requests. Where given,
   * the count draws in a slot that keeps that width.
   */
  countSizer?: string;
  /** The Add filter list, which a view without filters leaves out. */
  dimensions?: Dimension[];
  disabled?: Record<string, string>;
  filters: F & { range?: RangeValue; search?: string };
  options?: Record<string, Option[]>;
  /** The search's placeholder and label, which a view without a search leaves out. The field reads and writes `filters.search` through useCommittedText. */
  placeholder?: string;
  presets?: Record<string, readonly Preset[]>;
  /**
   * The range slot, which a card view leaves out. It draws while `filters.range` holds a value, and `now` is the clock
   * and `sentence` the window the view's reads cover, as RangeControl reads them.
   */
  range?: { now: () => number; sentence?: string; window: { from: number; to: number } };
  refresh: RefreshControlProps;
  /**
   * The sticky variant (Analytics): sticky under the shell's top block, 8 px of vertical padding, the 92 % page tint
   * and the hairline below.
   */
  sticky?: boolean;
  update: (patch: Partial<F>) => void;
}

function Bar<F extends object>({ address, children, count, countSizer, dimensions, disabled, filters, options = {}, placeholder, presets, range, refresh, sticky = false, update }: FilterBarProps<F>, ref: ForwardedRef<HTMLDivElement>): JSX.Element {
  // TypeScript checks an object literal against `Partial<F>` only once `F` is known, so every patch the bar builds over
  // the generic `F` is asserted here, in one place.
  const patch = (next: Record<string, unknown>) => update(next as Partial<F>);
  // A hook runs on every render, so the field's draft stands even on a view that draws no search.
  const search = useCommittedText(filters.search ?? '', (next) => patch({ search: next }));
  const addFilterRef = useRef<HTMLButtonElement>(null);
  const [edit, setEdit] = useState<{ anchor: HTMLElement; field: string } | null>(null);

  const chips = dimensions === undefined ? [] : chipsOf(dimensions, filters, options);
  const remove = (field: string, value?: string) => {
    const dimension = dimensions?.find((d) => d.field === field);
    if (dimension !== undefined) patch({ [field]: withoutValue(dimension, filters, value) });
  };
  const clearAll = () => patch(Object.fromEntries((dimensions ?? []).map((d) => [d.field, emptyValue(d)])));
  const countSpan = <span className="filter-bar__count muted">{count}</span>;

  return (
    <div ref={ref} className={sticky ? 'filter-bar filter-bar--sticky' : 'filter-bar'}>
      <div className="filter-bar__row">
        {range !== undefined && filters.range !== undefined && <RangeControl now={range.now} onChange={(next) => patch({ range: next })} sentence={range.sentence} value={filters.range} window={range.window} />}
        {placeholder !== undefined && <SearchInput aria-label={placeholder} placeholder={placeholder} value={search.draft} onChange={(e) => search.onChange(e.target.value)} onBlur={search.onBlur} onKeyDown={search.onKeyDown} className="filter-bar__search" />}
        {dimensions !== undefined && <AddFilter buttonRef={addFilterRef} dimensions={dimensions} disabled={disabled} edit={edit} filters={filters} onEditDone={() => setEdit(null)} options={options} presets={presets} update={patch} />}
        {children}
        <div className="filter-bar__edge">
          <RefreshControl {...refresh} />
          {count !== undefined &&
            (countSizer === undefined ? (
              countSpan
            ) : (
              <span className="filter-bar__count-slot">
                <span className="filter-bar__count-sizer" aria-hidden="true">
                  {countSizer}
                </span>
                {countSpan}
              </span>
            ))}
        </div>
      </div>
      {/* Mounted with no chips too: the line draws nothing then, and its focus hand-off after Clear all or the last
          chip's x runs in its own effect on the render that empties it. */}
      {dimensions !== undefined && <ChipLine addFilterRef={addFilterRef} address={address} chips={chips} editing={edit?.anchor ?? null} onClearAll={clearAll} onOpen={(field, anchor) => setEdit({ anchor, field })} onRemove={remove} />}
    </div>
  );
}

/**
 * `div.filter-bar` (ref forwarded, for useStickyHeight) holding `div.filter-bar__row` in the slot order range, search,
 * Add filter, children, `div.filter-bar__edge` (RefreshControl, then `span.filter-bar__count`, which a `countSizer`
 * sets in `span.filter-bar__count-slot` over a hidden sizer of that text), and the ChipLine under the row, which draws
 * while a chip stands. The bar holds the chip-edit state and nothing else: its popovers hold the page's auto reload
 * through the page's PopoverHost.
 */
// forwardRef drops the generic, so its result is read back under the generic signature.
export const FilterBar = forwardRef(Bar) as <F extends object>(props: FilterBarProps<F> & { ref?: Ref<HTMLDivElement> }) => JSX.Element;
