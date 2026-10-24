import { Tabs } from '@/components/ui/primitives';

/**
 * A page's sticky section bar, shared by the Analytics and Quota pages: the `.section-bar` around `Tabs` labeled
 * "Sections", one tab per section and the shown one selected. The stylesheet docks the bar under the shell's sticky
 * block through its `--main-top` token. `offset` is the height of the page's own sticky bars above this one, added to
 * the token so the bar docks under the banner, the top bar and those bars together.
 */
export function SectionBar<T extends string>({ offset, onChoose, sections, shown }: { offset?: number; onChoose: (id: T) => void; sections: readonly { id: T; label: string }[]; shown: T }): JSX.Element {
  return (
    <div className="section-bar" style={offset === undefined ? undefined : { top: `calc(var(--main-top) + ${offset}px)` }}>
      <Tabs<T> label="Sections" value={shown} onChange={onChoose} tabs={sections.map((s) => ({ controls: s.id === shown ? s.id : undefined, id: `tab-${s.id}`, label: s.label, value: s.id }))} />
    </div>
  );
}
