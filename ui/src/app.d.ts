declare global {
  namespace App {}

  /** The build this bundle was built from, injected by Vite from `SLOPPOD_BUILD` (see `vite.config.ts`); '' if unset. */
  const __SLOPPOD_BUILD__: string;
}

export {};
