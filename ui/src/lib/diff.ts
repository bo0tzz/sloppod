import { parsePatch, structuredPatch, type StructuredPatch } from 'diff';

export type DiffLine =
  | { type: 'hunk'; text: string }
  | { type: 'add' | 'del' | 'ctx'; text: string; oldNo?: number; newNo?: number }
  | { type: 'meta'; text: string };

export type FileDiff = {
  path: string;
  oldPath?: string;
  status: 'added' | 'deleted' | 'modified' | 'renamed';
  additions: number;
  deletions: number;
  lines: DiffLine[];
};

function stripPrefix(name: string | undefined): string | undefined {
  if (!name || name === '/dev/null') return undefined;
  return name.replace(/^[ab]\//, '');
}

function toFileDiff(patch: StructuredPatch): FileDiff {
  const lines: DiffLine[] = [];
  let additions = 0;
  let deletions = 0;
  for (const hunk of patch.hunks) {
    lines.push({
      type: 'hunk',
      text: `@@ -${hunk.oldStart},${hunk.oldLines} +${hunk.newStart},${hunk.newLines} @@`,
    });
    let oldNo = hunk.oldStart;
    let newNo = hunk.newStart;
    for (const line of hunk.lines) {
      const marker = line[0];
      const text = line.slice(1);
      if (marker === '+') {
        lines.push({ type: 'add', text, newNo: newNo++ });
        additions++;
      } else if (marker === '-') {
        lines.push({ type: 'del', text, oldNo: oldNo++ });
        deletions++;
      } else if (marker === '\\') {
        lines.push({ type: 'meta', text: line });
      } else {
        lines.push({ type: 'ctx', text, oldNo: oldNo++, newNo: newNo++ });
      }
    }
  }

  const oldPath = stripPrefix(patch.oldFileName);
  const newPath = stripPrefix(patch.newFileName);
  const status = !oldPath ? 'added' : !newPath ? 'deleted' : oldPath !== newPath ? 'renamed' : 'modified';
  return {
    path: newPath ?? oldPath ?? '(unknown)',
    oldPath: status === 'renamed' ? oldPath : undefined,
    status,
    additions,
    deletions,
    lines,
  };
}

const withNewline = (text: string) => (text === '' || text.endsWith('\n') ? text : `${text}\n`);

/**
 * Diff of an ACP `diff` tool call content block. `oldText` is null for a new file. The texts are
 * often snippets rather than whole files, so a missing final newline means nothing and isn't shown.
 */
export function diffTexts(path: string, oldText: string | null | undefined, newText: string): FileDiff {
  const patch = structuredPatch(
    oldText == null ? '/dev/null' : path,
    path,
    withNewline(oldText ?? ''),
    withNewline(newText),
    undefined,
    undefined,
    { context: 3 },
  );
  return { ...toFileDiff(patch), path };
}

/** Parses a unified diff, or returns null if it's malformed so the caller can show it raw. */
/** Lines added and removed across `files`. */
export function diffStats(files: FileDiff[]): { additions: number; deletions: number } {
  return {
    additions: files.reduce((sum, file) => sum + file.additions, 0),
    deletions: files.reduce((sum, file) => sum + file.deletions, 0),
  };
}

export function parseUnifiedDiff(text: string): FileDiff[] | null {
  if (!text.trim()) return [];
  try {
    return parsePatch(text).map(toFileDiff);
  } catch {
    return null;
  }
}
