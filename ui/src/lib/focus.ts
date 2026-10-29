/**
 * For what asks the user something while a turn waits on it, a question card or its folded summary: it takes focus
 * when it appears, so a stray Esc goes to it rather than stopping the turn, which would throw the questions away
 * (#365). Esc on it does nothing, since the composer's binding leaves an Esc already handled alone; Skip or the Stop
 * button do what's meant. Someone part way through writing in a field elsewhere (the composer, say) keeps their focus.
 */
export function claimsFocus(node: HTMLElement) {
  const active = document.activeElement;
  const typing =
    (active instanceof HTMLInputElement || active instanceof HTMLTextAreaElement) && active.value.trim() !== '';
  if (!typing) node.focus({ preventScroll: true });
  const keepEscape = (event: KeyboardEvent) => {
    if (event.key === 'Escape') event.preventDefault();
  };
  node.addEventListener('keydown', keepEscape);
  return { destroy: () => node.removeEventListener('keydown', keepEscape) };
}
