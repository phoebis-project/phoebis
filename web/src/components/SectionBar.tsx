import { Tabs } from '@/components/ui/primitives';

const tabId = (section: string): string => `tab-${section}`;
/** The attributes of the panel that the bar's tab for `section` controls. */
export const sectionPanel = (section: string): { 'aria-labelledby': string; id: string; role: 'tabpanel' } =>
  ({ 'aria-labelledby': tabId(section), id: section, role: 'tabpanel' });

/**
 * A page's sticky section bar, shared by the Analytics, Quota and Requests pages: the `.section-bar` around `Tabs`
 * labeled "Sections", one tab per section and the shown one selected. The stylesheet docks the bar under the shell's
 * sticky block through its `--main-top` token. `offset` is the height of the page's own sticky bars above this one,
 * added to the token so the bar docks under the banner, the top bar and those bars together.
 */
export function SectionBar<T extends string>({ offset, onChoose, sections, shown }: { offset?: number; onChoose: (id: T) => void; sections: readonly { id: T; label: string }[]; shown: T }): JSX.Element {
  return (
    <div className="section-bar" style={offset === undefined ? undefined : { top: `calc(var(--main-top) + ${offset}px)` }}>
      <Tabs<T> label="Sections" value={shown} onChange={onChoose} tabs={sections.map((s) => ({ controls: s.id === shown ? s.id : undefined, id: tabId(s.id), label: s.label, value: s.id }))} />
    </div>
  );
}
