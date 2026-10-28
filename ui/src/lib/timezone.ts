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
