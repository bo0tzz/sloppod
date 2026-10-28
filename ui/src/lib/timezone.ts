import { api } from './api.ts';

/** The browser's timezone, e.g. `Europe/Amsterdam`. */
export const browserTimezone = () => Intl.DateTimeFormat().resolvedOptions().timeZone;

/** Every timezone the browser knows, for choosing one. */
export const timezones = () => Intl.supportedValuesOf('timeZone');

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
