import { expect, test, type Page } from '@playwright/test';

// Not assertions about pixels: these capture the main screens for a human to look at.

const dir = '.screenshots';
const viewports = {
  desktop: { viewport: { width: 1280, height: 800 }, isMobile: false, hasTouch: false, deviceScaleFactor: 1 },
  mobile: { viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true, deviceScaleFactor: 2 },
} as const;

type Session = { id: string; title: string };

async function seededSession(page: Page, title: RegExp): Promise<Session> {
  const sessions: Session[] = await (await page.request.get('/api/v1/sessions')).json();
  const session = sessions.find((candidate) => title.test(candidate.title));
  if (!session) throw new Error(`no seeded session matching ${title}`);
  return session;
}

for (const [form, device] of Object.entries(viewports)) {
  for (const colorScheme of ['light', 'dark'] as const) {
    test.describe(`${form} ${colorScheme}`, () => {
      test.use({ ...device, colorScheme });
      const shot = (page: Page, name: string) => page.screenshot({ path: `${dir}/${name}-${form}-${colorScheme}.png` });

      test('session list and new session form', async ({ page }) => {
        await page.goto('/');
        await expect(page.getByTestId('session-row').first()).toBeVisible();
        await shot(page, 'sessions');

        await page.goto('/new');
        await page.getByRole('textbox', { name: 'Prompt' }).fill('Add backoff to the ACP reconnect logic in sloppod, and a test for it.');
        await shot(page, 'new-session');

        await page.goto('/settings');
        await expect(page.getByTestId('credential').first()).toBeVisible();
        await page.getByTestId('credentials-github-app').getByRole('button', { name: 'Add GitHub App' }).click();
        await shot(page, 'settings');
      });

      test('queued messages while the agent starts', async ({ page }) => {
        const response = await page.request.post('/api/v1/sessions', {
          data: { prompt: 'slow start: what can you tell me about this runtime?' },
        });
        const session: Session = await response.json();
        await page.goto(`/sessions/${session.id}`);
        await expect(page.getByTestId('activity')).toBeVisible();
        await page.getByRole('textbox', { name: 'Prompt' }).fill('Keep it short');
        await shot(page, 'starting');
      });

      test('finished session with a proposal', async ({ page }) => {
        const session = await seededSession(page, /^Reconnects hammer/);
        await page.goto(`/sessions/${session.id}`);
        await expect(page.getByTestId('proposal-card')).toBeVisible();
        const edit = page.getByTestId('tool-call').filter({ hasText: 'Edit ' });
        await edit.locator('summary').click();
        await expect(edit.getByTestId('diff')).toBeVisible();
        const tests = page.getByTestId('tool-call').filter({ hasText: 'npm test' });
        await tests.locator('summary').click();
        await expect(tests.getByTestId('tool-output')).toBeVisible();
        await shot(page, 'session');

        await page.getByTestId('proposal-card').getByRole('button', { name: 'Review' }).click();
        await expect(page.getByTestId('proposal-detail').getByTestId('diff').first()).toBeVisible();
        await shot(page, 'proposal');
      });

      test('live turn waiting on a permission', async ({ page }) => {
        const response = await page.request.post('/api/v1/sessions', {
          data: { prompt: 'Make the reconnect delay in sloppod back off exponentially' },
        });
        const session: Session = await response.json();
        await page.goto(`/sessions/${session.id}`);
        await expect(page.getByTestId('permission-request')).toBeVisible();
        await page.getByRole('textbox', { name: 'Prompt' }).fill('Cap it at ten seconds');
        await shot(page, 'permission');
      });

      test('repository access request and the notice after approving it', async ({ page }) => {
        const response = await page.request.post('/api/v1/sessions', {
          data: { prompt: 'Please request a repository for this' },
        });
        const session: Session = await response.json();
        await page.goto(`/sessions/${session.id}`);
        const card = page.getByTestId('proposal-card');
        await expect(card.getByTestId('proposal-reason')).toBeVisible();
        await shot(page, 'repo-grant');
        await card.getByRole('button', { name: 'Approve' }).click();
        await expect(page.getByTestId('system-notice')).toBeVisible();
        await expect(page.getByTestId('session-screen').getByTestId('phase')).toHaveAttribute('data-phase', 'idle');
        await shot(page, 'notice');
      });

      test('questions from the agent', async ({ page }) => {
        const response = await page.request.post('/api/v1/sessions', { data: { prompt: 'Ask me how to do it' } });
        const session: Session = await response.json();
        await page.goto(`/sessions/${session.id}`);
        const card = page.getByTestId('question-card');
        await card.getByRole('radio', { name: /Exponential/ }).check();
        await card.getByRole('checkbox', { name: /End-to-end/ }).check();
        await expect(card.getByTestId('option-preview')).toBeVisible();
        await card.locator('header').scrollIntoViewIfNeeded();
        await shot(page, 'question');
      });

      test('stopped session with a compaction', async ({ page }) => {
        const session = await seededSession(page, /^Summarise/);
        await page.goto(`/sessions/${session.id}`);
        await expect(page.getByTestId('compaction')).toBeVisible();
        await page.getByTestId('compaction').locator('summary').click();
        await shot(page, 'compaction');
      });
    });
  }
}
