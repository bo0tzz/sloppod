import type { Highlight } from './api.ts';
import { addDays, weekday, zonedDate, zonedTime } from './timezone.ts';

const HOUR = 3600_000;

/** A highlight the user is still to be shown, which they put off until later (#467). */
export function isSnoozed(highlight: Highlight, now = Date.now()): boolean {
  return (
    !highlight.seen_at &&
    !highlight.retracted_at &&
    highlight.snoozed_until !== undefined &&
    Date.parse(highlight.snoozed_until) > now
  );
}

export type SnoozeChoice = { title: string; until: Date };

/**
 * What each snooze choice means, in the user's timezone (#467): an hour on; this evening at 18:00, tomorrow evening
 * once that has gone; tomorrow at 09:00; and next Monday at 09:00, a week on when today is a Monday. Every one is
 * ahead of `now`, so nothing comes straight back.
 */
export function snoozeChoices(zone: string, now = new Date()): SnoozeChoice[] {
  const today = zonedDate(zone, now);
  const evening = zonedTime(zone, today, 18);
  const nextMonday = addDays(today, ((8 - (weekday(today) || 7)) % 7) || 7);
  return [
    { title: 'In an hour', until: new Date(now.getTime() + HOUR) },
    {
      title: 'This evening (18:00)',
      until: evening.getTime() > now.getTime() ? evening : zonedTime(zone, addDays(today, 1), 18),
    },
    { title: 'Tomorrow morning (09:00)', until: zonedTime(zone, addDays(today, 1), 9) },
    { title: 'Next week (Monday 09:00)', until: zonedTime(zone, nextMonday, 9) },
  ];
}
