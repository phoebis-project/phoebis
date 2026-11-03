import { useState } from 'react';
import { Link } from 'react-router-dom';
import { api } from '@/api';
import type { DecisionRecord, DecisionSummary } from '@/api/types';
import { ChipView, Chips, chipValue, rowValue } from '@/components/ChipView';
import { DecisionStrip } from '@/components/DecisionStrip';
import { EntryChips } from '@/components/EntryChips';
import { Badge, Button, CopyButton, ErrorState, KeyValue, useHostedDescription } from '@/components/ui/primitives';
import { failedFirst, useAsync } from '@/lib/hooks';
import {
  aroundRequest,
  aroundSubject,
  candidateCells,
  type CardRow,
  clockMinute,
  DECISION_KIND_LABEL,
  DECISION_KIND_TONE,
  entityHref,
  formatFact,
  type LogLink,
  outcomeRows,
  paceHeader,
  pinRows,
  rankCandidates,
  requestRows,
  settingsRows,
  verdictChips,
  verdictSentence,
} from '@/lib/routingLog';

/**
 * A row with `more` draws its compact chips or its full list as `expanded` reads, the control after them on their line.
 * The card holds the flag, so a card opened on another row starts folded.
 */
export const keyValueRows = (rows: CardRow[], expanded = false, setExpanded?: (expanded: boolean) => void) =>
  rows.map((r) => ({
    label: r.label,
    title: r.title,
    value:
      r.more && setExpanded ? (
        <Chips chips={expanded ? r.more.all : r.chips}>
          <button type="button" className="link-btn" aria-expanded={expanded} onClick={() => setExpanded(!expanded)}>
            {expanded ? 'Latest 3' : `All ${r.more.count} entries`}
          </button>
        </Chips>
      ) : (
        rowValue(r.chips)
      ),
  }));

/** A member's Pace score with its hover, described through the page's host as a `Badge` is. */
function PaceCell({ title, value }: { title: string | null; value: string }) {
  // Outside every host the title alone describes the cell: a node beside it would join the cell's text.
  const hosted = useHostedDescription(title ?? undefined);
  return (
    <td className="num" title={title ?? undefined} aria-describedby={hosted}>
      {value === '-' ? <span className="muted">-</span> : value}
    </td>
  );
}

/**
 * The open decision row's card: loads the record on mount and draws the six parts of the spec's card. The record is
 * read again when the row's answer moves, so a card opened on a running request completes when an auto reload poll
 * brings it.
 */
export function DecisionCard({ now, onWiden, row }: { now: number; onWiden: (link: LogLink) => void; row: DecisionSummary }) {
  const answer = JSON.stringify([row.status, row.attemptCount, row.cache]);
  const loaded = useAsync(() => api.getDecision(row.requestId, row.seq), [row.requestId, row.seq, answer]);
  if (failedFirst(loaded)) {
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
  const [entriesShown, setEntriesShown] = useState(false);
  const at = Date.parse(row.at);
  const settings = settingsRows(record);
  // Narrow to wide, as the Requests drawer orders its own.
  const widenings: { label: string; link: LogLink }[] = [
    { label: 'this minute', link: clockMinute(at, now) },
    ...(row.sessionId !== null ? [{ label: 'this session', link: { session: row.sessionId, ...aroundSubject(at, now) } }] : []),
    ...(row.accountId !== null ? [{ label: 'this account', link: { accounts: [row.accountId], ...aroundRequest(at, null, now) } }] : []),
  ];
  return (
    <div className="decision-card">
      <div className="decision-card__verdict">
        <Badge tone={DECISION_KIND_TONE[row.kind]}>{DECISION_KIND_LABEL[row.kind]}</Badge>
        <span className="decision-card__sentence">
          {verdictSentence(record, row).map((c, i) => (
            <span key={i}>{chipValue(c)}</span>
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
          <h4 className="decision-card__title">Candidates, by rank</h4>
          <div className="table-wrap">
            <table className="table decision-card__candidates">
              <thead>
                <tr>
                  <th>Account</th>
                  <th>Tier</th>
                  <th>5h</th>
                  <th>7d</th>
                  <th>Fable</th>
                  <th>Projection</th>
                  <th>Runway</th>
                  <th>In flight</th>
                  <th>{paceHeader(record)}</th>
                  <th>Prefix</th>
                  <th>Verdict</th>
                </tr>
              </thead>
              <tbody>
                {rankCandidates(record.candidates).map((c) => {
                  const cells = candidateCells(c, record, now);
                  return (
                    <tr key={c.accountId} className={cells.verdict.chosen ? 'is-chosen' : undefined}>
                      <td>
                        <Link to={entityHref('account', c.accountId)}>{c.accountId}</Link>
                        <span className="sub">{cells.plan}</span>
                      </td>
                      <td className="num">{cells.tier}</td>
                      {cells.windows.length === 1 ? (
                        <td colSpan={3} className="num">
                          {cells.windows[0][0]}
                          {cells.windows[0].slice(1).map((line, j) => (
                            <span key={j} className="sub">
                              {line}
                            </span>
                          ))}
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
                      <td className="num decision-card__window" title={cells.projectionTitle ?? undefined}>
                        {cells.projection.map((line, j) => (
                          <span key={j} className={line === '-' ? 'muted' : undefined}>
                            {line}
                          </span>
                        ))}
                      </td>
                      <td className="num">{cells.runway === '-' ? <span className="muted">-</span> : cells.runway}</td>
                      <td className="num">{cells.inFlight}</td>
                      <PaceCell title={cells.paceTitle} value={cells.pace} />
                      <td className="decision-card__prefix">{cells.prefix.hash ? <EntryChips entry={cells.prefix.hash} now={now} /> : <span className="muted">-</span>}</td>
                      <td className="decision-card__judged">
                        <Badge tone={cells.verdict.badgeTone}>{cells.verdict.badge}</Badge>
                        {cells.verdict.chip && (
                          <span className="decision-card__why">
                            {/* A destination's fit carries the verdict's tone, which a badge draws and a chip has no member for. */}
                            {cells.verdict.tone === null ? (
                              <ChipView chip={cells.verdict.chip} />
                            ) : (
                              <Badge tone={cells.verdict.tone} title={cells.verdict.chip.title}>
                                {cells.verdict.chip.value}
                              </Badge>
                            )}
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
        {/* A refusal entered no setting, so its card draws no settings fold. */}
        {record.outcome.kind !== 'refused' && (
          <details className="decision-card__fold">
            <summary>
              Settings in force
              {record.settings !== null && (
                <>
                  {' '}
                  <span className="decision-card__since muted">since {formatFact(record.settings.poolUpdatedAt, 'time', now)}</span>
                </>
              )}
            </summary>
            {settings === null ? <p className="muted">The key and its pool had left the registry</p> : <KeyValue className="decision-card__settings" rows={keyValueRows(settings)} />}
          </details>
        )}
      </div>

      <section>
        <h4 className="decision-card__title">Outcome</h4>
        <KeyValue className="decision-card__outcome" rows={keyValueRows(outcomeRows(record, now, row), entriesShown, setEntriesShown)} />
      </section>

      <footer className="decision-card__foot">
        <span className="muted">Widen to</span>
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
