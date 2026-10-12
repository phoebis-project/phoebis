import { useRef, type KeyboardEvent, type PointerEvent, type RefObject } from 'react';
import { Legend } from '@/components/charts/Tooltip';
import { fmtPct } from '@/lib/format';
import { leadWords } from '@/lib/presets';
import { aheadOf, paceEnd, toEndWords } from '@/lib/quota';
import { WINDOW_HOURS } from '@/lib/windows';

/**
 * A member's shared-window readings, each with the reset its lead over pace needs, and the account's end (`endOf`),
 * which the pace runs to where it comes first. Null where its row reports no such window.
 */
export interface MemberReading {
  end: number | null;
  fiveHour: { resetsAt: string | null; utilization: number } | null;
  id: string;
  label: string;
  sevenDay: { resetsAt: string | null; utilization: number } | null;
}

interface Threshold {
  /** Whole percents for a drag, two decimals for a keyed step from a value the API set finer. */
  onChange: (percent: number) => void;
  value: number;
}

const KEY_STEPS: Record<string, number> = { ArrowDown: -1, ArrowLeft: -1, ArrowRight: 1, ArrowUp: 1, PageDown: -10, PageUp: 10 };
const clamp = (v: number, min: number, max: number) => Math.min(max, Math.max(min, v));
/** A lead's place on the lead track, which runs from 100 points behind pace to 100 ahead. A lead past either end stays on the track. */
const onLead = (lead: number) => clamp((lead + 1) / 2, 0, 1) * 100;

function Handle({ label, min, max, threshold, track }: { label: string; max: number; min: number; threshold: Threshold; track: RefObject<HTMLDivElement | null> }) {
  const { onChange, value } = threshold;
  // A refused value can lie past the track (a select-below of 120), so the handle and aria-valuenow stay on the track
  // while the text and title keep the value the field holds.
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
      aria-valuenow={placed}
      aria-valuetext={`${value}%`}
      title={`${label}: ${value}%`}
      className="ubar__handle ubar__handle--select"
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

/**
 * Two tracks over the members' current 5-hour and 7-day readings. The upper one is their used shares, 0 to 100 %, with
 * the select-below threshold as a handle: it drags or takes the arrow keys, a percent a step, and draws while the
 * threshold is on and only with `expert`, since the plain view leaves that threshold to the expert view unless a
 * refusal shows its field there, when the caller passes the flag so the shown field keeps its handle. The shedding
 * margin is a lead over pace, which no point on an axis of used shares can stand for, so it draws on the lower track,
 * where each reading is a point at its lead: the margin as a line and the cool band under it while shed-ahead is on.
 */
export function UtilizationBar({ expert, margin, now, readings, selectBelow }: { expert: boolean; margin: number | null; now: number; readings: MemberReading[]; selectBelow: Threshold | null }) {
  const track = useRef<HTMLDivElement>(null);
  // The window's length comes off WINDOW_HOURS, the table the decoder sets a reading's periodHours from, so the lead
  // here is the one the Quota card's pace words print.
  const windows = readings.flatMap((r) =>
    (
      [
        ['5h', '5-hour', r.fiveHour],
        ['7d', '7-day', r.sevenDay],
      ] as const
    ).flatMap(([id, name, w]) => (w === null ? [] : [{ end: paceEnd(w.resetsAt, r.end), id, key: `${r.id}-${id}`, label: r.label, lead: aheadOf(w, WINDOW_HOURS[id] * 3_600_000, now, r.end), name, used: w.utilization }])),
  );
  // The band spans the leads a cool destination may hold above pace, up to the margin less the hysteresis band of 5
  // points, so a margin at or under 5 leaves none to shade.
  const band = margin === null ? 0 : clamp(margin - 5, 0, 100) / 2;
  return (
    <div className="ubar">
      <div className="ubar__track" ref={track}>
        {windows.map((w) => (
          <span key={w.key} className={`ubar__tick ubar__tick--${w.id}`} style={{ left: `${clamp(w.used, 0, 1) * 100}%` }} title={`${w.label}: ${w.name} ${fmtPct(w.used)}`} />
        ))}
        {expert && selectBelow && <Handle label="Select-below threshold" min={0} max={100} threshold={selectBelow} track={track} />}
      </div>
      {/* Placed rather than spread: spreading centers the middle mark between labels of unequal width, off the track's 50%. */}
      <div className="ubar__scale num" aria-hidden="true">
        <span style={{ left: 0, position: 'absolute' }}>0%</span>
        <span style={{ left: '50%', position: 'absolute', transform: 'translateX(-50%)' }}>50%</span>
        <span style={{ position: 'absolute', right: 0 }}>100%</span>
      </div>
      <div className="ubar__track ubar__track--lead" role="img" aria-label="Lead over pace by member and window">
        {band > 0 && <span className="ubar__band" style={{ left: '50%', width: `${band}%` }} title="cool under here" />}
        {windows.map((w) => (
          <span key={w.key} className={`ubar__tick ubar__tick--${w.id}`} style={{ left: `${onLead(w.lead)}%` }} title={`${w.label}: ${w.name} ${fmtPct(w.used)} used, ${fmtPct(w.used - w.lead)} elapsed${toEndWords(w.end, now)}, ${leadWords(w.lead)}`} />
        ))}
        {margin !== null && <span className="ubar__margin" style={{ left: `${onLead(margin / 100)}%` }} title={`Shedding margin: ${margin} ${margin === 1 ? 'point' : 'points'}`} />}
      </div>
      <div className="ubar__scale num" aria-hidden="true">
        <span style={{ left: 0, position: 'absolute' }}>-100</span>
        <span style={{ left: '50%', position: 'absolute', transform: 'translateX(-50%)' }}>on pace</span>
        <span style={{ position: 'absolute', right: 0 }}>+100</span>
      </div>
      <Legend
        items={[
          { color: 'var(--series-1)', label: '5-hour reading' },
          { color: 'var(--series-4)', label: '7-day reading' },
          ...(margin !== null ? [{ color: 'var(--serious)', label: 'Shedding margin' }] : []),
        ]}
      />
    </div>
  );
}
