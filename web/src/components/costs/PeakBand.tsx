import { Fragment, useMemo } from 'react';
import type { SubscriptionCosts } from '@/api/types';
import { useFormatVersion } from '@/app/settings';
import { Card } from '@/components/ui/primitives';
import { bandCaption, bandNow, bandWeek, nextLocalChange, zoneCity } from '@/lib/costs';
import { fmtShortTime, getTimeFormat } from '@/lib/format';

/** The browser's zone, the one every time the console shows reads in. */
export const viewingZone = (): string => Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC';

const pad = (n: number) => String(n).padStart(2, '0');

/** A minute of the day on the console's 12-hour or 24-hour clock. */
export const minuteClock = (minutes: number): string => {
  // The band's end at the day's last minute reads 24:00, which no formatter prints.
  if (getTimeFormat() === '24h') return `${pad(Math.floor(minutes / 60))}:${pad(minutes % 60)}`;
  return new Date(Date.UTC(2000, 0, 1, 0, minutes)).toLocaleTimeString('en-US', { hour: '2-digit', hourCycle: 'h12', minute: '2-digit', timeZone: 'UTC' });
};

const clockAt = (at: number) => fmtShortTime(new Date(at).toISOString());

/** "3 h 30 min", "45 min". */
const fmtSpan = (ms: number) => {
  const minutes = Math.round(ms / 60_000);
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
const shareCost = (factor: number) => `${Number((1 / factor).toPrecision(3))}x`;
const holds = (factor: number) => `${Math.round(factor * 100)}%`;
const factorText = (factor: number) => (factor === 1 ? `no band, holds ${holds(factor)}` : `holds ${holds(factor)}, a share costs ${shareCost(factor)}`);

const HOUR_TICKS = [0, 3, 6, 9, 12, 15, 18, 21, 24];
// Past 20:00 the now marker's label would run off the track's end, so it sits on the marker's left.
const LATE = 20 * 60;
const atMinute = (minutes: number) => `${(minutes / 1440) * 100}%`;

/** The peak band in the browser's zone over the week `bandWeek` draws, this week or, on a day the band is off before
 *  its next band day's week, next week, with the zone the file defines it in, now, and its weight per window. */
export function PeakBand({ costs, now }: { costs: SubscriptionCosts; now: number }) {
  const version = useFormatVersion();
  const zone = viewingZone();
  const week = useMemo(() => bandWeek(costs, now, zone), [costs, now, zone]);
  // The clock reads the time format from module state, so the setting's version is what redraws these two lines.
  const caption = useMemo(() => bandCaption(costs, now, zone, minuteClock), [costs, now, zone, version]);
  const change = useMemo(() => nextLocalChange(costs, now, zone, minuteClock), [costs, now, zone, version]);
  const nowMinute = useMemo(() => {
    const parts = new Intl.DateTimeFormat('en-US', { hour: 'numeric', hourCycle: 'h23', minute: 'numeric', timeZone: zone }).formatToParts(now);
    return Number(parts.find((p) => p.type === 'hour')?.value) * 60 + Number(parts.find((p) => p.type === 'minute')?.value);
  }, [now, zone]);
  const { fiveHour, sevenDay } = costs.peak.capacityFactor;
  const state = bandNow(costs, now);
  // A zone this browser cannot read gives no hours here, so the card draws no grid, no swatch and no change.
  const unresolved = 'unresolved' in state;
  return (
    <Card title="Peak band" subtitle={`${week.some((day) => day.today) ? 'This week.' : 'Next week.'} A request completing inside the band spends more of its 5-hour window.`}>
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
            {[
              { factor: fiveHour, label: '5-hour window' },
              { factor: sevenDay, label: '7-day window' },
            ].map(({ factor, label }) => (
              <Fragment key={label}>
                <span>{label}</span>
                <span className="cost-ref__track">
                  <span className={`cost-ref__factor-bar${factor === 1 ? '' : ' cost-ref__factor-bar--band'}`} style={{ width: holds(factor) }} />
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
