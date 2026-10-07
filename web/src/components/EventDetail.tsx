import { Link } from 'react-router-dom';
import type { RoutingEvent } from '@/api/types';
import { KeyValue } from '@/components/ui/primitives';
import { detailPairs, type DetailPair, entityHref, logHref, requestLink } from '@/lib/routingLog';
import { ChipView, rowValue } from './DecisionCard';

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
              <td key={j}>{j === 0 ? <ChipView chip={c} /> : c.value}</td>
            ))}
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/** An event row opened: When, Account and Pool, then the kind's own pairs, and the request it names in the foot. */
export function EventDetail({ now, row }: { now: number; row: RoutingEvent }) {
  const pairs = detailPairs(row, now);
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
