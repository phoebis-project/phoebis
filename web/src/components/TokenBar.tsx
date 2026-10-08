import type { Usage } from '@/api/types';
import { TOKEN_SERIES } from '@/components/charts/theme';
import { useChartInk } from '@/components/charts/useChartInk';
import { fmtInt } from '@/lib/format';
import { tokenBarLabel, tokenSegments } from '@/lib/tokenBar';

/** The request's token kinds as one horizontal bar in the Tokens chart's order and colors, with the chart's legend under it. */
export function TokenBar({ usage }: { usage: Usage }) {
  const ink = useChartInk();
  const segments = tokenSegments(usage);
  const countOf = new Map(segments.map((s) => [s.key, s.count]));
  return (
    <div className="token-bar">
      <div className="token-bar__row">
        <div className="token-bar__track" role="img" aria-label={tokenBarLabel(usage)}>
          {segments.map((s) => (
            <span key={s.key} className="token-bar__seg" style={{ width: `${s.share * 100}%`, background: ink.series[s.slot] }} title={`${s.label}: ${fmtInt(s.count)}`} />
          ))}
        </div>
        {segments.length === 0 && <span className="muted">no tokens</span>}
      </div>
      <ul className="token-bar__legend">
        {TOKEN_SERIES.map((s) => (
          <li key={s.key}>
            <span className="token-bar__key" style={{ background: ink.series[s.slot] }} />
            <span>{s.label}</span>
            <span className="num">{fmtInt(countOf.get(s.key) ?? 0)}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}
