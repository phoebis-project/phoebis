import { Link } from 'react-router-dom';
import { api } from '@/api';
import type { DecisionRecord, DecisionSummary } from '@/api/types';
import { DecisionStrip } from '@/components/DecisionStrip';
import { EntryChips } from '@/components/EntryChips';
import { Badge, Button, CopyButton, ErrorState, KeyValue, useDescription } from '@/components/ui/primitives';
import { useAsync } from '@/lib/hooks';
import {
  aroundRequest,
  aroundSubject,
  candidateCells,
  type CardRow,
  type Chip,
  clockMinute,
  DECISION_KIND_LABEL,
  DECISION_KIND_TONE,
  entityHref,
  type LogLink,
  outcomeRows,
  paceHeader,
  pinRows,
  requestRows,
  settingsRows,
  verdictChips,
  verdictSentence,
} from '@/lib/routingLog';

/**
 * One chip: a value or a key-value pair, the entity it names a link. A popover marks the chip its anchor stands for,
 * and `describedBy` points at the node that carries its hover for a reader.
 */
export function ChipView({ chip, describedBy, marked = false }: { chip: Chip; describedBy?: string; marked?: boolean }) {
  const value = chip.entity ? <Link to={entityHref(chip.entity.kind, chip.entity.id)}>{chip.value}</Link> : chip.href ? <Link to={chip.href}>{chip.value}</Link> : chip.value;
  const label = chip.labelEntity ? <Link to={entityHref(chip.labelEntity.kind, chip.labelEntity.id)}>{chip.label}</Link> : chip.label;
  return (
    <span className={`${chip.mono ? 'chip mono' : 'chip'}${marked ? ' is-marked' : ''}`} title={chip.title} aria-describedby={describedBy}>
      {label !== undefined && <span className="chip__label">{label}</span>}
      {value}
    </span>
  );
}

export function Chips({ chips }: { chips: Chip[] }) {
  return (
    <span className="chips">
      {chips.map((c, i) => (
        <ChipView key={`${c.label ?? ''}/${c.value}/${i}`} chip={c} />
      ))}
    </span>
  );
}

/** A row's value: one plain value or one entity reads as itself, anything more as a row of chips. */
export function rowValue(chips: Chip[]) {
  const [only] = chips;
  if (chips.length === 1 && only.label === undefined && only.title === undefined) {
    if (only.entity) return <Link to={entityHref(only.entity.kind, only.entity.id)}>{only.value}</Link>;
    if (only.href) return <Link to={only.href} className={only.mono ? 'mono' : undefined}>{only.value}</Link>;
    return only.value;
  }
  return <Chips chips={chips} />;
}

export const keyValueRows = (rows: CardRow[]) => rows.map((r) => ({ label: r.label, title: r.title, value: rowValue(r.chips) }));

/** A member's Pace score with its hover, described through the page's host as a `Badge` is. */
function PaceCell({ title, value }: { title: string | null; value: string }) {
  const described = useDescription(title ?? undefined);
  // Outside every host the title alone describes the cell: a node beside it would join the cell's text.
  const hosted = described && !described.sibling ? described.id : undefined;
  return (
    <td className="num" title={title ?? undefined} aria-describedby={hosted}>
      {value === '-' ? <span className="muted">-</span> : value}
    </td>
  );
}

/**
 * The open decision row's card: loads the record on mount and draws the six parts of the spec's card. The record is
 * read again when the row's answer moves, so a card opened on a running request completes when Follow brings it.
 */
export function DecisionCard({ now, onWiden, row }: { now: number; onWiden: (link: LogLink) => void; row: DecisionSummary }) {
  const answer = JSON.stringify([row.status, row.attemptCount, row.cache]);
  const loaded = useAsync(() => api.getDecision(row.requestId, row.seq), [row.requestId, row.seq, answer]);
  if (loaded.data === null && loaded.error !== null) {
    return (
      <div className="decision-card">
        <ErrorState title="Could not load the record" body={loaded.error} action={<Button onClick={loaded.reload}>Try again</Button>} />
      </div>
    );
  }
  if (loaded.data === null && loaded.loading) return <div className="decision-card muted">Loading the record</div>;
  if (loaded.data === null) return <div className="decision-card">The record left with the retention</div>;
  return <Card now={now} onWiden={onWiden} record={loaded.data} row={row} />;
}

function Card({ now, onWiden, record, row }: { now: number; onWiden: (link: LogLink) => void; record: DecisionRecord; row: DecisionSummary }) {
  const at = Date.parse(row.at);
  const settings = settingsRows(record);
  const widenings: { label: string; link: LogLink }[] = [
    ...(row.sessionId !== null ? [{ label: 'this session', link: { session: row.sessionId, ...aroundSubject(at, now) } }] : []),
    ...(row.accountId !== null ? [{ label: 'this account', link: { accounts: [row.accountId], ...aroundRequest(at, null, now) } }] : []),
    { label: 'this minute', link: clockMinute(at, now) },
  ];
  return (
    <div className="decision-card">
      <div className="decision-card__verdict">
        <Badge tone={DECISION_KIND_TONE[row.kind]}>{DECISION_KIND_LABEL[row.kind]}</Badge>
        <span className="decision-card__sentence">
          {verdictSentence(record, row).map((c, i) => (
            <span key={i}>{c.entity ? <Link to={entityHref(c.entity.kind, c.entity.id)}>{c.value}</Link> : c.value}</span>
          ))}
        </span>
        <Chips chips={verdictChips(record, row)} />
      </div>

      <section>
        <h4 className="decision-card__title">How the router decided</h4>
        <DecisionStrip now={now} record={record} row={row} />
      </section>

      {record.candidates.length > 0 && (
        <section>
          <h4 className="decision-card__title">Candidates, in the walk&apos;s order</h4>
          <div className="table-wrap">
            <table className="table decision-card__candidates">
              <thead>
                <tr>
                  <th>Account</th>
                  <th>Tier</th>
                  <th>5h</th>
                  <th>7d</th>
                  <th>Fable</th>
                  <th>In flight</th>
                  <th>{paceHeader(record)}</th>
                  <th>Prefix</th>
                  <th>Verdict</th>
                </tr>
              </thead>
              <tbody>
                {record.candidates.map((c) => {
                  const cells = candidateCells(c, record, now);
                  const tinted = ['chosen', 'held_on', 'renewed'].includes(c.verdict);
                  return (
                    <tr key={c.accountId} className={tinted ? 'is-chosen' : undefined}>
                      <td>
                        <Link to={entityHref('account', c.accountId)}>{c.accountId}</Link>
                        <span className="sub">{cells.plan}</span>
                      </td>
                      <td className="num">{cells.tier}</td>
                      {cells.windows.length === 1 ? (
                        <td colSpan={3} className="num">
                          {cells.windows[0][0]}
                        </td>
                      ) : (
                        cells.windows.map((lines, i) => (
                          <td key={i} className="num decision-card__window">
                            {lines.map((line, j) => (
                              <span key={j} className={j === 0 ? undefined : 'sub'}>
                                {line}
                              </span>
                            ))}
                          </td>
                        ))
                      )}
                      <td className="num">{cells.inFlight}</td>
                      <PaceCell title={cells.paceTitle} value={cells.pace} />
                      <td className="decision-card__prefix">{cells.prefix.hash ? <EntryChips entry={cells.prefix.hash} now={now} /> : <span className="muted">-</span>}</td>
                      <td className="decision-card__judged">
                        <Badge tone={c.verdict === 'out' ? 'warning' : tinted ? 'accent' : 'neutral'}>{cells.verdict.badge}</Badge>
                        {cells.verdict.chip && (
                          <span className="decision-card__why">
                            <ChipView chip={cells.verdict.chip} />
                          </span>
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </section>
      )}

      <div className="decision-card__folds">
        <details className="decision-card__fold">
          <summary>Request facts</summary>
          <KeyValue rows={keyValueRows(requestRows(record, row))} />
        </details>
        <details className="decision-card__fold">
          <summary>Pin and warmth</summary>
          <KeyValue rows={keyValueRows(pinRows(record, now))} />
        </details>
        <details className="decision-card__fold">
          <summary>Settings in force</summary>
          {settings === null ? <p className="muted">The key and its pool had left the registry</p> : <KeyValue className="decision-card__settings" rows={keyValueRows(settings)} />}
        </details>
      </div>

      <section>
        <h4 className="decision-card__title">Outcome</h4>
        <KeyValue className="decision-card__outcome" rows={keyValueRows(outcomeRows(record, now, row))} />
      </section>

      <footer className="decision-card__foot">
        <span className="muted">Widen:</span>
        {widenings.map((w) => (
          <button key={w.label} type="button" className="link-btn" onClick={() => onWiden(w.link)}>
            {w.label}
          </button>
        ))}
        <span className="decision-card__record muted">Record v{record.version}</span>
        <CopyButton text={JSON.stringify(record.raw, null, 2)} label="Copy as JSON" />
      </footer>
    </div>
  );
}
