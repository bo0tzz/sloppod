/**
 * The browser's `localStorage`, for things that are nice to keep across a reload: reads and writes never throw, so a
 * full, denied or missing store only means nothing is kept.
 */

export function readStored(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

/** Stores `value` under `key`, or removes it for `null`; says whether it could. */
export function writeStored(key: string, value: string | null): boolean {
  try {
    if (value === null) localStorage.removeItem(key);
    else localStorage.setItem(key, value);
    return true;
  } catch {
    // Not kept, which only matters after a reload.
    return false;
  }
}
