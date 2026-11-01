import { Link } from 'react-router-dom';
import type { RoutingEvent, RoutingLogRow } from '@/api/types';
import { ChipView, rowValue } from '@/components/ChipView';
import { KeyValue } from '@/components/ui/primitives';
import type { WeekReading } from '@/lib/analytics';
import { detailPairs, type DetailPair, entityHref, logHref, requestLink } from '@/lib/routingLog';

/** The shedding round's pins, one row each in the round's order. */
function PinTable({ table }: { table: NonNullable<DetailPair['table']> }) {
  return (
    <table className="table event-detail__pins">
      <thead>
        <tr>
          {table.head.map((h) => (
            <th key={h}>{h}</th>
          ))}
        </tr>
      </thead>
      <tbody>
        {table.rows.map((cells, i) => (
          <tr key={i}>
            {cells.map((c, j) => (
              <td key={j} title={j === 0 ? undefined : c.title}>
                {j === 0 ? <ChipView chip={c} /> : c.value}
              </td>
            ))}
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/** An event row opened: When, Account and Pool, then the kind's own pairs, and the request it names in the foot. */
export function EventDetail({ now, row, rows, week }: { now: number; row: RoutingEvent; rows: readonly RoutingLogRow[]; week: WeekReading }) {
  const pairs = detailPairs(row, now, rows, week);
  return (
    <div className="event-detail">
      <KeyValue rows={pairs.map((p) => ({ label: p.label, value: p.table ? <PinTable table={p.table} /> : rowValue(p.chips) }))} />
      {row.requestId !== null && (
        <footer className="event-detail__foot">
          <Link to={logHref(requestLink(row.requestId, Date.parse(row.at), null, now))}>Source request</Link>
          <Link to={entityHref('request', row.requestId)}>Details</Link>
        </footer>
      )}
    </div>
  );
}
