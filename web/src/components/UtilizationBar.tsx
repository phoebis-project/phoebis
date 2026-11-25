import { useRef, type KeyboardEvent, type PointerEvent, type RefObject } from 'react';
import { Legend } from '@/components/charts/Tooltip';
import { fmtPct } from '@/lib/format';
import { WINDOW_LABEL } from '@/lib/windows';

/** A member's used share of each shared window, null where its row reports no such window or the router holds no
 *  share of it. */
export interface MemberReading {
  fiveHour: number | null;
  id: string;
  label: string;
  sevenDay: number | null;
}

interface Threshold {
  /** Whole percents for a drag, two decimals for a keyed step from a value the API set finer. */
  onChange: (percent: number) => void;
  value: number;
}

const KEY_STEPS: Record<string, number> = { ArrowDown: -1, ArrowLeft: -1, ArrowRight: 1, ArrowUp: 1, PageDown: -10, PageUp: 10 };
const clamp = (v: number, min: number, max: number) => Math.min(max, Math.max(min, v));

function Handle({ label, min, max, threshold, track }: { label: string; max: number; min: number; threshold: Threshold; track: RefObject<HTMLDivElement | null> }) {
  const { onChange, value } = threshold;
  // A refused value can lie past the track (a ceiling of 120) or under the slider's minimum (a ceiling of 40), so the
  // handle stays on the track and aria-valuenow inside the slider's bounds, while the text and title keep the value the
  // field holds.
  const placed = clamp(value, 0, 100);
  const toPointer = (e: PointerEvent<HTMLSpanElement>) => {
    const rect = track.current?.getBoundingClientRect();
    if (!rect || rect.width === 0) return;
    onChange(clamp(Math.round(((e.clientX - rect.left) / rect.width) * 100), min, max));
  };
  const onKeyDown = (e: KeyboardEvent<HTMLSpanElement>) => {
    const next = e.key === 'Home' ? min : e.key === 'End' ? max : e.key in KEY_STEPS ? Math.round((value + KEY_STEPS[e.key]) * 100) / 100 : null;
    if (next === null) return;
    e.preventDefault();
    onChange(clamp(next, min, max));
  };
  return (
    <span
      role="slider"
      tabIndex={0}
      aria-label={label}
      aria-orientation="horizontal"
      aria-valuemin={min}
      aria-valuemax={max}
      aria-valuenow={clamp(value, min, max)}
      aria-valuetext={`${value}%`}
      title={`${label}: ${value}%`}
      className="ubar__handle ubar__handle--ceiling"
      style={{ left: `${placed}%` }}
      onKeyDown={onKeyDown}
      onPointerDown={(e) => {
        // jsdom has no pointer capture; a browser keeps the drag on the handle once the pointer leaves it.
        e.currentTarget.setPointerCapture?.(e.pointerId);
        toPointer(e);
      }}
      onPointerMove={(e) => {
        if (e.currentTarget.hasPointerCapture?.(e.pointerId)) toPointer(e);
      }}
    />
  );
}

/** One kind of window's track: the members' readings of that window under the handle of its ceiling. */
function Track({ id, label, max, min, readings, threshold }: { id: '5h' | '7d'; label: string; max: number; min: number; readings: MemberReading[]; threshold: Threshold }) {
  const track = useRef<HTMLDivElement>(null);
  const windows = readings.flatMap((r) => {
    const used = id === '5h' ? r.fiveHour : r.sevenDay;
    // A tick at 0 would read a window with no share as an unused one.
    return used === null ? [] : [{ key: `${r.id}-${id}`, label: r.label, used }];
  });
  return (
    <div className="ubar__track" ref={track}>
      {windows.map((w) => (
        <span key={w.key} className={`ubar__tick ubar__tick--${id}`} style={{ left: `${clamp(w.used, 0, 1) * 100}%` }} title={`${w.label}: ${WINDOW_LABEL[id]} ${fmtPct(w.used)}`} />
      ))}
      <Handle label={label} min={min} max={max} threshold={threshold} track={track} />
    </div>
  );
}

/**
 * The members' current readings on two tracks of used shares, 0 to 100 %: the 5-hour readings under the 5-hour
 * ceiling's handle, then the 7-day readings under the weekly ceiling's. Each handle drags or takes the arrow keys, a
 * percent a step, between `min` and `max`. The pool holds a ceiling per kind of window, so each handle stands over the
 * readings its ceiling holds back and no reading of the other kind.
 */
export function UtilizationBar({ fiveHourCeiling, max, min, readings, weeklyCeiling }: { fiveHourCeiling: Threshold; max: number; min: number; readings: MemberReading[]; weeklyCeiling: Threshold }) {
  return (
    <div className="ubar">
      <Track id="5h" label="5-hour ceiling" min={min} max={max} readings={readings} threshold={fiveHourCeiling} />
      <Track id="7d" label="Weekly ceiling" min={min} max={max} readings={readings} threshold={weeklyCeiling} />
      {/* Placed rather than spread: spreading centers the middle mark between labels of unequal width, off the track's 50%. */}
      <div className="ubar__scale num" aria-hidden="true">
        <span style={{ left: 0, position: 'absolute' }}>0%</span>
        <span style={{ left: '50%', position: 'absolute', transform: 'translateX(-50%)' }}>50%</span>
        <span style={{ position: 'absolute', right: 0 }}>100%</span>
      </div>
      <Legend
        items={[
          { color: 'var(--series-1)', label: `${WINDOW_LABEL['5h']} reading` },
          { color: 'var(--series-4)', label: `${WINDOW_LABEL['7d']} reading` },
        ]}
      />
    </div>
  );
}
