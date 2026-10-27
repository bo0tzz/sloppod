import { MediaQuery } from 'svelte/reactivity';

/**
 * The session sidebar. It's there from `md` up: open beside the page on wide screens, and folded away on narrower
 * ones, where it opens over the page. A toggle lasts until the screen crosses that width.
 */
class Sidebar {
  readonly #available = new MediaQuery('(min-width: 768px)');
  readonly #wide = new MediaQuery('(min-width: 1024px)');
  #toggled = $state<{ wide: boolean; open: boolean } | null>(null);

  get available() {
    return this.#available.current;
  }

  get wide() {
    return this.#wide.current;
  }

  get open() {
    return this.#toggled && this.#toggled.wide === this.wide ? this.#toggled.open : this.wide;
  }

  set open(open: boolean) {
    this.#toggled = { wide: this.wide, open };
  }

  toggle() {
    this.open = !this.open;
  }
}

export const sidebar = new Sidebar();
