import type { RepoOption } from './components/RepoPicker.svelte';

/**
 * What a token's repositories come to in a picker (#352): the ones it can push to first, then the rest, each in name
 * order, the rest marked because a session on one can't push its branch. A credential added before the runtime kept
 * that list has no `writable`, and its repositories are offered as they come, unmarked.
 */
export function offeredRepositories(repositories: string[], writable?: string[]): RepoOption[] {
  if (!writable) return repositories.map((repo) => ({ value: repo }));
  const pushable = new Set(writable);
  const byName = (a: string, b: string) => a.toLowerCase().localeCompare(b.toLowerCase());
  const canPush = repositories.filter((repo) => pushable.has(repo)).sort(byName);
  const readOnly = repositories.filter((repo) => !pushable.has(repo)).sort(byName);
  return [
    ...canPush.map((repo) => ({ value: repo })),
    ...readOnly.map((repo) => ({ value: repo, label: `${repo} (read-only)` })),
  ];
}
