/**
 * Where focus goes around dialogs (#367): into a dialog when it opens, and back to what opened it when it closes.
 *
 * What opened a dialog is the last element focused outside any dialog or menu: the button behind it, or, for one
 * opened from a menu, the menu's button. It's tracked as focus moves, since the menu item clicked is gone by the time
 * the dialog closes.
 */
let lastOutside: HTMLElement | null = null;

if (typeof document !== 'undefined') {
  document.addEventListener('focusin', (event) => {
    const target = event.target;
    if (target instanceof HTMLElement && !target.closest('[role="dialog"], [role="alertdialog"], [role="menu"]')) {
      lastOutside = target;
    }
  });
}

/** What a dialog opening now should give focus back to when it closes. */
export function opener(): HTMLElement | null {
  return lastOutside;
}

/** Gives focus back to `element`, if it's still there. */
export function returnFocus(element: HTMLElement | null) {
  // After the dialog's own focus handling has let go.
  setTimeout(() => {
    if (element?.isConnected) element.focus();
  });
}

const FIELDS = 'input:not([type="hidden"]):not([disabled]), textarea:not([disabled]), select:not([disabled])';

/**
 * Focuses what a dialog's user most likely wants first: what's marked `data-autofocus`, else its first field, else
 * the dialog itself, never the ✕ in its header.
 */
export function focusInto(dialog: HTMLElement) {
  const target =
    dialog.querySelector<HTMLElement>('[data-autofocus]') ??
    dialog.querySelector<HTMLElement>(FIELDS);
  if (target) {
    target.focus();
    return;
  }
  if (!dialog.hasAttribute('tabindex')) dialog.setAttribute('tabindex', '-1');
  dialog.focus();
}
