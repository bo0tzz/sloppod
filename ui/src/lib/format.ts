const units: [Intl.RelativeTimeFormatUnit, number][] = [
  ['year', 365 * 24 * 3600],
  ['month', 30 * 24 * 3600],
  ['week', 7 * 24 * 3600],
  ['day', 24 * 3600],
  ['hour', 3600],
  ['minute', 60],
];

const relative = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' });

export function timeAgo(iso: string, now = Date.now()): string {
  const seconds = (Date.parse(iso) - now) / 1000;
  for (const [unit, size] of units) {
    if (Math.abs(seconds) >= size) {
      return relative.format(Math.round(seconds / size), unit);
    }
  }
  return 'just now';
}

export function absoluteTime(iso: string): string {
  return new Date(iso).toLocaleString();
}

/** A time of day, with the date too when it isn't today. */
export function clockTime(iso: string, now = Date.now()): string {
  const date = new Date(iso);
  const time = date.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
  return date.toDateString() === new Date(now).toDateString()
    ? time
    : `${date.toLocaleDateString(undefined, { weekday: 'short', day: 'numeric', month: 'short' })} ${time}`;
}

/** How long until `iso`, to the minute: "in 1 h 23 min", or "any moment now" once it's less than a minute. */
export function countdown(iso: string, now = Date.now()): string {
  const minutes = Math.ceil((Date.parse(iso) - now) / 60_000);
  if (minutes < 1) return 'any moment now';
  const hours = Math.floor(minutes / 60);
  const rest = minutes % 60;
  return `in ${hours > 0 ? `${hours} h ` : ''}${hours > 0 && rest === 0 ? '' : `${rest} min`}`.trimEnd();
}
