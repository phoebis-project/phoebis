import { Fragment, useMemo } from 'react';
import type { SubscriptionCosts } from '@/api/types';
import { useFormatVersion } from '@/app/settings';
import { Card } from '@/components/ui/primitives';
import { bandCaption, bandNow, bandSentence, bandWeek, COST_WINDOWS, hhmm, nextLocalChange, partsIn, regimeNames, regimeWord, zoneCity } from '@/lib/costs';
import { figure, fmtPct, fmtShortTime, formatIn, getTimeFormat, viewingZone } from '@/lib/format';
import { MINUTE } from '@/lib/time';

const pad = (n: number) => String(n).padStart(2, '0');

/** A minute of the day on the console's 12-hour or 24-hour clock. */
export const minuteClock = (minutes: number): string => {
  // The band's end at the day's last minute reads 24:00, which no formatter prints.
  if (getTimeFormat() === '24h') return hhmm(minutes);
  return formatIn({ hour: '2-digit', hourCycle: 'h12', minute: '2-digit', timeZone: 'UTC' }, Date.UTC(2000, 0, 1, 0, minutes));
};

const clockAt = (at: number) => fmtShortTime(at);

/** "3 h 30 min", "45 min". */
const fmtSpan = (ms: number) => {
  const minutes = Math.round(ms / MINUTE);
  const hours = Math.floor(minutes / 60);
  return hours > 0 ? `${hours} h ${pad(minutes % 60)} min` : `${minutes} min`;
};

/** Whether the band runs now and when that changes, beside the band's grid and the plan rates' band control. */
export const nowLine = (costs: SubscriptionCosts, now: number): string => {
  const next = bandNow(costs, now);
  if ('unresolved' in next) return `This browser cannot read the zone ${next.unresolved}.`;
  if (next.at === null) return next.inside ? 'Inside the peak band now, with no end in the next eight days.' : 'Outside the peak band now, with no start in the next eight days.';
  return next.inside ? `Inside the peak band now, until ${clockAt(next.at)} (${fmtSpan(next.at - now)})` : `Outside the peak band now. It starts at ${clockAt(next.at)} (in ${fmtSpan(next.at - now)})`;
};

/** "1.43x": what a share costs at a capacity factor. */
const shareCost = (factor: number) => `${figure(1 / factor)}x`;
const factorText = (factor: number) => (factor === 1 ? `no band, holds ${fmtPct(factor)}` : `holds ${fmtPct(factor)}, a share costs ${shareCost(factor)}`);

const FACTOR_WINDOWS = COST_WINDOWS.map((w) => ({ key: w.factor, label: `${w.label} window` }));

const HOUR_TICKS = [0, 3, 6, 9, 12, 15, 18, 21, 24];
// Past 20:00 the now marker's label would run off the track's end, so it sits on the marker's left.
const LATE = 20 * 60;
const atMinute = (minutes: number) => `${(minutes / 1440) * 100}%`;

/** The peak band in the browser's zone over the week `bandWeek` draws, this week, or next week when the start of
 *  today's band (the next band day's on a day the band is off) falls in it, with the zone the file defines it in, now,
 *  and its weight per window. */
export function PeakBand({ costs, now }: { costs: SubscriptionCosts; now: number }) {
  const version = useFormatVersion();
  const zone = viewingZone();
  const week = useMemo(() => bandWeek(costs, now, zone), [costs, now, zone]);
  // The clock reads the time format from module state, so the setting's version is what redraws these two lines. The
  // memo reads the version so its dependency list can name it.
  const { caption, change } = useMemo(() => {
    void version;
    return {
      caption: bandCaption(costs, now, zone, minuteClock),
      change: nextLocalChange(costs, now, zone, minuteClock),
    };
  }, [costs, now, zone, version]);
  const nowMinute = useMemo(() => partsIn(now, zone).minute, [now, zone]);
  const { fiveHour } = costs.peak.capacityFactor;
  // The band and its factors are the base regime's. A regime that holds a window differently adds its own row.
  const factors = [
    ...FACTOR_WINDOWS.map(({ key, label }) => ({ factor: costs.peak.capacityFactor[key], label })),
    ...regimeNames(costs).flatMap((name) =>
      FACTOR_WINDOWS.filter(({ key }) => costs.regimes[name].peakCapacityFactor[key] !== costs.peak.capacityFactor[key]).map(({ key, label }) => ({ factor: costs.regimes[name].peakCapacityFactor[key], label: `${label}, ${regimeWord(name)}` })),
    ),
  ];
  const state = bandNow(costs, now);
  // A zone this browser cannot read gives no hours here, so the card draws no grid, no swatch and no change.
  const unresolved = 'unresolved' in state;
  return (
    <Card
      title="Peak band"
      subtitle={`${week.some((day) => day.today) ? 'This week.' : 'Next week.'} ${bandSentence(costs)}`}
    >
      <div className="cost-ref__band-side">
        <div>
          <p className="cost-ref__band-lead">{caption}</p>
          {!unresolved && (
            <div className="cost-ref__band">
              {week.map((day) => (
                <Fragment key={day.date}>
                  <span className={`cost-ref__band-day${day.today ? ' is-today' : ''}`}>{day.label}</span>
                  <span className="cost-ref__band-track">
                    {day.runs.map((r) => (
                      <span
                        key={r.from}
                        className="cost-ref__band-run"
                        style={{ left: atMinute(r.from), width: atMinute(r.to - r.from) }}
                        title={`${day.label}, ${minuteClock(r.from)} to ${minuteClock(r.to)} here. A 5-hour share costs ${shareCost(fiveHour)}.`}
                      />
                    ))}
                    {day.today && <span className={`cost-ref__band-now${nowMinute > LATE ? ' is-late' : ''}`} style={{ left: atMinute(nowMinute) }} title={`Now, ${clockAt(now)}`} />}
                  </span>
                </Fragment>
              ))}
              <span />
              <span className="cost-ref__band-axis" aria-hidden="true">
                {HOUR_TICKS.map((h) => (
                  <span key={h} style={{ left: `${(h / 24) * 100}%` }}>
                    {pad(h)}
                  </span>
                ))}
              </span>
            </div>
          )}
        </div>
        <div className="cost-ref__band-text">
          <p className="cost-ref__now">
            {!('unresolved' in state) && state.inside && <span className="cost-ref__swatch" aria-hidden="true" />}
            {nowLine(costs, now)}
          </p>
          {change && !unresolved && (
            <p className="cost-ref__note">
              {change.lasting
                ? `From ${change.from} the band runs ${change.text} here, once ${change.moved ? `${zoneCity(costs.peak.zone)} changes its clocks` : 'your clocks change'}.`
                : `${change.from} to ${change.to}: ${change.text} here, while the two zones change their clocks on different dates.`}
            </p>
          )}
          <div className="cost-ref__factor">
            {factors.map(({ factor, label }) => (
              <Fragment key={label}>
                <span>{label}</span>
                <span className="cost-ref__track">
                  <span className={`cost-ref__factor-bar${factor === 1 ? '' : ' cost-ref__factor-bar--band'}`} style={{ width: fmtPct(factor) }} />
                </span>
                <span className="cost-ref__val">{factorText(factor)}</span>
              </Fragment>
            ))}
          </div>
        </div>
      </div>
    </Card>
  );
}
