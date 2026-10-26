import DOMPurify from 'dompurify';
import { Marked } from 'marked';

const prose = new Marked({ gfm: true, breaks: false });
// People type single newlines and expect them kept.
const typed = new Marked({ gfm: true, breaks: true });

DOMPurify.addHook('afterSanitizeAttributes', (node) => {
  if (node.tagName === 'A') {
    node.setAttribute('target', '_blank');
    node.setAttribute('rel', 'noopener noreferrer');
  }
});

export function renderMarkdown(text: string, { breaks = false } = {}): string {
  const html = (breaks ? typed : prose).parse(text, { async: false });
  return DOMPurify.sanitize(html);
}
