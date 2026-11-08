import { api } from './api.ts';

/** The browser's timezone, e.g. `Europe/Amsterdam`. */
export const browserTimezone = () => Intl.DateTimeFormat().resolvedOptions().timeZone;

/**
 * The timezones to choose from: every one the browser knows, plus UTC, which browsers leave out of that list, and
 * `current`, e.g. an alias saved earlier that the browser's list doesn't have.
 */
export function timezones(current?: string | null): string[] {
  const zones = new Set(Intl.supportedValuesOf('timeZone'));
  zones.add('UTC');
  if (current) zones.add(current);
  return [...zones].sort();
}

let adopted = false;

/** Gives a user without a timezone the browser's, once a page load, so agents are told the user's time. */
export async function adoptBrowserTimezone() {
  if (adopted) return;
  adopted = true;
  try {
    const settings = await api.getSettings();
    if (settings.timezone === null) await api.updateSettings({ timezone: browserTimezone() });
  } catch {
    adopted = false;
  }
}

let known: string | undefined;

/** The user's timezone from Settings, the browser's for a user without one (#253); read once a page load. */
export async function userTimezone(): Promise<string> {
  if (known !== undefined) return known;
  try {
    known = (await api.getSettings()).timezone ?? browserTimezone();
  } catch {
    // Not remembered: a moment without the runtime shouldn't pin the browser's over the user's.
    return browserTimezone();
  }
  return known;
}

/** A date on a zone's calendar. */
export type CalendarDate = { year: number; month: number; day: number };

/** What a zone's clock reads at `at`. */
function zonedFields(zone: string, at: Date) {
  const parts = new Intl.DateTimeFormat('en-US', {
    timeZone: zone,
    hour12: false,
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  }).formatToParts(at);
  const value = (type: string) => Number(parts.find((part) => part.type === type)?.value);
  // Some engines read midnight as hour 24.
  return { year: value('year'), month: value('month'), day: value('day'), hour: value('hour') % 24, minute: value('minute'), second: value('second') };
}

/** How far ahead of UTC a zone's clock is at `at`. */
function offsetAt(zone: string, at: Date): number {
  const { year, month, day, hour, minute, second } = zonedFields(zone, at);
  return Date.UTC(year, month - 1, day, hour, minute, second) - (at.getTime() - at.getMilliseconds());
}

/** The date a zone's clock is on at `at`. */
export function zonedDate(zone: string, at: Date): CalendarDate {
  const { year, month, day } = zonedFields(zone, at);
  return { year, month, day };
}

/** `days` on from a date. */
export function addDays(date: CalendarDate, days: number): CalendarDate {
  const at = new Date(Date.UTC(date.year, date.month - 1, date.day + days));
  return { year: at.getUTCFullYear(), month: at.getUTCMonth() + 1, day: at.getUTCDate() };
}

/** A date's weekday: 0 Sunday … 6 Saturday. */
export function weekday(date: CalendarDate): number {
  return new Date(Date.UTC(date.year, date.month - 1, date.day)).getUTCDay();
}

/**
 * The instant at which a zone's clock reads a wall-clock time there. A time a daylight-saving change skips or repeats
 * resolves the way the browser resolves the zone's own: two rounds of converting settle the offset, which differs
 * either side of the change.
 */
export function zonedTime(zone: string, date: CalendarDate, hour: number, minute = 0): Date {
  const wall = Date.UTC(date.year, date.month - 1, date.day, hour, minute);
  let at = wall;
  for (let round = 0; round < 2; round++) at = wall - offsetAt(zone, new Date(at));
  return new Date(at);
}
