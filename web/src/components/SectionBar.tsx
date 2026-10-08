import { Tabs } from '@/components/ui/primitives';

/**
 * A page's sticky section bar, shared by the Analytics and Quota pages: the `.section-bar` around `Tabs` labeled
 * "Sections", one tab per section and the shown one selected. `top` is the sticky offset, and without it the
 * stylesheet's `top: 52px` docks the bar under the top bar.
 */
export function SectionBar<T extends string>({ onChoose, sections, shown, top }: { onChoose: (id: T) => void; sections: readonly { id: T; label: string }[]; shown: T; top?: number }): JSX.Element {
  return (
    <div className="section-bar" style={top === undefined ? undefined : { top }}>
      <Tabs<T> label="Sections" value={shown} onChange={onChoose} tabs={sections.map((s) => ({ controls: s.id === shown ? s.id : undefined, id: `tab-${s.id}`, label: s.label, value: s.id }))} />
    </div>
  );
}
