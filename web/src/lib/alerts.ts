import type { AskUserQuestionDetail } from '@/api/types';

/**
 * The line an `ask_user_question` event draws under its session. A null question has three causes the route
 * tells apart, and each reads as itself: the viewer role, the captured body's deadline, or a body never captured.
 * `now` is the page's clock in Unix milliseconds, the unit the route stores the deadline in.
 */
export function questionLine(asked: AskUserQuestionDetail, now: number): { muted: boolean; text: string } {
  if (asked.question) return { muted: false, text: `asked: ${asked.question}` };
  if (asked.question_redacted) return { muted: true, text: 'question is for administrators' };
  const expiresAt = asked.question_expires_at ?? null;
  if (expiresAt !== null && expiresAt <= now) return { muted: true, text: 'question expired with its captured body' };
  return { muted: true, text: 'question not captured' };
}
