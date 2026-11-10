/**
 * What an "Add …" form needs when it opens (#370): the page scrolled to it, its first field focused, and Esc leaving
 * it. A form that opens below the fold — on a phone, say — otherwise looks like the tap did nothing, and an Esc that
 * does nothing leaves the user hunting for Cancel.
 */
export function opensForAdding(node: HTMLElement, onEscape: () => void) {
  node.scrollIntoView({ block: 'start' });
  node.querySelector<HTMLElement>('input:not([type="hidden"]), textarea, select')?.focus({ preventScroll: true });
  const keydown = (event: KeyboardEvent) => {
    if (event.key !== 'Escape' || event.defaultPrevented) return;
    event.preventDefault();
    onEscape();
  };
  node.addEventListener('keydown', keydown);
  return { destroy: () => node.removeEventListener('keydown', keydown) };
}
