import { useRef, type KeyboardEvent, type PointerEvent, type RefObject } from 'react';
import { Legend } from '@/components/charts/Tooltip';
import { fmtPct } from '@/lib/format';
import { MIGRATE_BAND } from '@/lib/presets';

/** A member's shared-window readings as fractions; null where its row reports no such window. */
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

function Handle({ kind, label, min, max, threshold, track }: { kind: 'migrate' | 'select'; label: string; max: number; min: number; threshold: Threshold; track: RefObject<HTMLDivElement | null> }) {
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
      className={`ubar__handle ubar__handle--${kind}`}
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
 * Both utilization thresholds as handles over the members' current 5-hour and 7-day readings, 0 to 100 %. A handle
 * drags or takes the arrow keys, a percent a step; a threshold that is off draws no handle, and the select-below
 * handle draws only with `expert`, since the plain view leaves that threshold to the expert view unless a refusal
 * shows its field there, when the caller passes the flag so the shown field keeps its handle. The band under the
 * migrate handle is shaded, since a shed session lands only on a member reading below it.
 */
export function UtilizationBar({ expert, migrate, migrateMin, readings, selectBelow }: { expert: boolean; migrate: Threshold | null; migrateMin: number; readings: MemberReading[]; selectBelow: Threshold | null }) {
  const track = useRef<HTMLDivElement>(null);
  const band = MIGRATE_BAND * 100;
  const ticks = readings.flatMap((r) =>
    (
      [
        ['5h', '5-hour', r.fiveHour],
        ['7d', '7-day', r.sevenDay],
      ] as const
    ).flatMap(([id, name, u]) => (u === null ? [] : [{ id, key: `${r.id}-${id}`, left: clamp(u, 0, 1) * 100, title: `${r.label}: ${name} ${fmtPct(u)}` }])),
  );
  return (
    <div className="ubar">
      <div className="ubar__track" ref={track}>
        {migrate && <span className="ubar__band" style={{ left: `${Math.max(migrate.value - band, 0)}%`, width: `${Math.min(band, migrate.value)}%` }} title={`The ${band}% band under the migrate threshold, which a shed session lands below`} />}
        {ticks.map((t) => (
          <span key={t.key} className={`ubar__tick ubar__tick--${t.id}`} style={{ left: `${t.left}%` }} title={t.title} />
        ))}
        {expert && selectBelow && <Handle kind="select" label="Select-below threshold" min={0} max={100} threshold={selectBelow} track={track} />}
        {migrate && <Handle kind="migrate" label="Migrate threshold" min={migrateMin} max={100} threshold={migrate} track={track} />}
      </div>
      {/* Placed rather than spread: spreading centres the middle mark between labels of unequal width, off the track's 50%. */}
      <div className="ubar__scale num" aria-hidden="true">
        <span style={{ left: 0, position: 'absolute' }}>0%</span>
        <span style={{ left: '50%', position: 'absolute', transform: 'translateX(-50%)' }}>50%</span>
        <span style={{ position: 'absolute', right: 0 }}>100%</span>
      </div>
      <Legend
        items={[
          { color: 'var(--series-1)', label: '5-hour reading' },
          { color: 'var(--series-4)', label: '7-day reading' },
          { color: 'var(--serious-soft)', label: 'Band' },
        ]}
      />
    </div>
  );
}
