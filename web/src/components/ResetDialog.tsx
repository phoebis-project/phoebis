import { Fragment, useEffect, useRef, useState, type ReactNode } from 'react';
import { createPortal } from 'react-dom';
import { api } from '@/api';
import type { Account, LimitResetsRead, ResetGrant, ResetUse, WindowId } from '@/api/types';
import { useSession } from '@/app/session';
import { Button, Dialog, KeyValue, Refusal, Warning } from '@/components/ui/primitives';
import { fmtDate, fmtEndsIn, fmtRelative, noun } from '@/lib/format';
import { useLatest } from '@/lib/hooks';
import { windowOf } from '@/lib/quota';
import { clearsWords, resetAnswer, resetClearsNothing, resetEstimate, resetHolds, resetRowNote, resetRows, type ResetAnswer } from '@/lib/resets';
import { periodHoursOf } from '@/lib/windows';

/**
 * The first page, with the row Back returned from, or the confirmation of one grant, `direct` where it opened there.
 */
type Page = { at: 'pick'; from: string | null } | { at: 'confirm'; direct: boolean; grant: string };

/**
 * No claim, one on its way, or the answer to the last one: the grant it claimed, the row the page held as the answer
 * came and the member the answer carried.
 */
type Claim = null | 'out' | { answer: ResetAnswer; grant: string; row: Account; use: ResetUse | null };

/** A read opens on the confirmation where its block lists exactly one grant and that grant is usable. */
function firstPage(read: LimitResetsRead): Page {
  const grants = read.limitResets?.grants ?? [];
  return grants.length === 1 && grants[0].state === 'usable' ? { at: 'confirm', direct: true, grant: grants[0].id } : { at: 'pick', from: null };
}

/**
 * An account's limit resets on two pages: the first lists them to pick one from, and the confirmation names what the
 * picked reset clears and sends its claim. A claim cannot be read back, so while one is out every close waits for its
 * answer, a use closes the dialog under a toast, and a refusal or an open outcome keeps it on the confirmation under
 * the answer's sentence.
 */
export function ResetDialog({
  account,
  chip,
  now,
  onAnswered,
  onClose,
  opening,
}: {
  /** The page's row of the account, a fresh object after each list read. */
  account: Account;
  /** A window's chip as the card reads it now. */
  chip: (window: WindowId) => ReactNode;
  now: number;
  /** After every answer of a claim, so the page reads its list again. */
  onAnswered: () => void;
  onClose: () => void;
  /** What the click's read answered, a failed read folded into `error` over the stored block. Null while closed. */
  opening: LimitResetsRead | null;
}): JSX.Element {
  const { notify } = useSession();
  const [page, setPage] = useState<Page>({ at: 'pick', from: null });
  const [claim, setClaim] = useState<Claim>(null);
  // Each read opens the dialog anew, on the page its block calls for and with no claim.
  const [openedOn, setOpenedOn] = useState<LimitResetsRead | null>(null);
  if (opening !== openedOn) {
    setOpenedOn(opening);
    if (opening !== null) {
      setPage(firstPage(opening));
      setClaim(null);
    }
  }
  const latest = useLatest({ account, now });
  const list = useRef<HTMLUListElement>(null);
  const cancelButton = useRef<HTMLButtonElement>(null);
  const open = opening !== null;
  const out = claim === 'out';
  const answered = claim === null || claim === 'out' ? null : claim;
  // The answer's own member stands until a list read lands a fresh row.
  const after = answered === null ? null : account !== answered.row ? account.limitResets : (answered.use?.limitResets ?? null);
  const block = after ?? opening?.limitResets ?? null;
  const label = account.label;

  // A claim cannot be read back, so every stoppable close waits for its answer.
  const close = () => {
    if (!out) onClose();
  };
  const send = async (grant: ResetGrant) => {
    setClaim('out');
    // The member the confirmation is drawn from, so the router claims for the account and the counts it shows.
    const result = await api.useLimitReset(account.id, grant.id, block?.revision).then(
      (use) => ({ use }),
      (thrown: unknown) => ({ thrown }),
    );
    const answer = resetAnswer(result, account.label, grant, latest.current.now);
    onAnswered();
    if (answer.kind === 'used') {
      setClaim(null);
      notify('Reset used');
      onClose();
      return;
    }
    setClaim({ answer, grant: grant.id, row: latest.current.account, use: 'use' in result ? result.use : null });
  };

  // A child's effects run first, so this lands after the dialog's own showModal, as ConfirmDialog's focus does.
  useEffect(() => {
    if (!open) return;
    const rows = [...(list.current?.querySelectorAll<HTMLButtonElement>('button[data-grant]') ?? [])].filter((b) => !b.disabled);
    // A page change unmounts the focused button, so each page names its own start.
    const start = page.at === 'confirm' ? undefined : (rows.find((b) => b.dataset.grant === page.from) ?? rows[0]);
    (start ?? cancelButton.current)?.focus();
  }, [open, page]);

  const cancel = (
    <Button ref={cancelButton} variant="ghost" disabled={out} onClick={close}>
      Cancel
    </Button>
  );
  const stale =
    opening !== null && opening.error !== null && block !== null ? (
      <p className="reset-dialog__stale muted">
        Read {fmtRelative(block.readAt, now)}. A fresh read failed: {opening.error}
      </p>
    ) : null;
  const confirming = page.at === 'confirm' ? (block?.grants.find((g) => g.id === page.grant) ?? null) : null;

  let title: string;
  let pages: ReactNode;
  let foot: ReactNode = cancel;
  let refusal: ReactNode = null;
  let clearsNothing = false;
  if (confirming !== null && page.at === 'confirm') {
    title = `Use reset on ${label}`;
    const shown = answered !== null && answered.grant === confirming.id ? answered.answer : null;
    // Before an answer the grant was usable where the dialog offered it, and after one the member read after the claim
    // decides.
    const target = answered === null ? confirming : (after?.grants.find((g) => g.id === confirming.id) ?? null);
    const usable = answered === null || target?.state === 'usable';
    const why = usable ? undefined : target === null ? resetHolds(label, null) : (resetRowNote(target, after?.cooldownUntil ?? null, now) ?? undefined);
    const estimate = resetEstimate(confirming, block?.fableWeekShare ?? null, windowOf(account, '7d'), account.fableWindow, now);
    refusal = shown !== null && (
      <Refusal>
        {shown.sentence}
        {shown.kind === 'unconfirmed' && ` ${resetHolds(label, target)}`}
      </Refusal>
    );
    clearsNothing = resetClearsNothing(confirming.clears.map((id) => windowOf(account, id)), now);
    pages = (
      <>
        <p>{confirming.label}</p>
        <div className="reset-dialog__windows">
          {confirming.clears.map((id) => (
            <Fragment key={id}>
              {chip(id)}
              <span>{periodHoursOf(id) === 5 ? 'starts anew' : 'clears to 100% left'}</span>
            </Fragment>
          ))}
          {estimate !== null && (
            <>
              {chip('7d')}
              <span>
                also gives back its Fable share
                {estimate.figure !== null && (
                  <>
                    , about {estimate.figure} left after, <span className="muted">estimated</span>
                  </>
                )}
              </span>
            </>
          )}
        </div>
        <KeyValue
          className="reset-dialog__kv"
          rows={[
            { label: 'Left after', value: `${Math.max(0, confirming.resetsLeft - 1)} of ${confirming.resetsTotal}` },
            { label: 'Use by', value: `${fmtDate(confirming.endsAt)}, ${fmtEndsIn(confirming.endsAt, now).replace(/^ends /, '')}` },
          ]}
        />
        <p className="secondary">A used reset cannot be returned.</p>
      </>
    );
    foot = (
      <>
        {!page.direct && (
          <Button variant="ghost" className="reset-dialog__back" disabled={out} onClick={() => setPage({ at: 'pick', from: confirming.id })}>
            Back
          </Button>
        )}
        {cancel}
        <Button variant="primary" disabled={out || !usable} title={why} onClick={() => void send(confirming)}>
          {out ? 'Using…' : 'Use reset'}
        </Button>
      </>
    );
  } else {
    title = `Resets on ${label}`;
    pages = block !== null && (
      <ul ref={list} className="plain-list reset-dialog__rows">
        {resetRows(block).map((g) => {
          const note = resetRowNote(g, block.cooldownUntil, now);
          const words = clearsWords(g.clears);
          return (
            <li key={g.id} className="reset-dialog__row">
              <div className="reset-dialog__facts">
                <span className="reset-dialog__label">{g.label}</span>
                <span className="reset-dialog__line">
                  {words !== null && <span>Clears {words}</span>}
                  <span>
                    {g.resetsLeft} of {g.resetsTotal} left
                  </span>
                  <span>use by {fmtDate(g.endsAt)}</span>
                </span>
                {note !== null && <span className="reset-dialog__note">{note}</span>}
              </div>
              <Button size="sm" data-grant={g.id} disabled={note !== null} onClick={() => setPage({ at: 'confirm', direct: false, grant: g.id })}>
                Use reset
              </Button>
            </li>
          );
        })}
      </ul>
    );
  }

  // A dialog inside the card would inherit the foot's type size and join the card's text, so it lives under the body.
  return createPortal(
    <Dialog open={open} onClose={close} title={title} width={440} className="reset-dialog" footer={foot}>
      {stale}
      {refusal}
      {/* Mounted on the pick page too, so the region stands before a reset that clears nothing fills it. */}
      <Warning when={clearsNothing}>{confirming !== null && `${clearsWords(confirming.clears)} ${noun(confirming.clears.length, 'reads', 'read')} 100% left now, so this reset has nothing to clear yet.`}</Warning>
      {pages}
    </Dialog>,
    document.body,
  );
}
