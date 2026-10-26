import { expect, test, type Page } from '@playwright/test';

// Read-only checks against a real runtime's existing session. Never approves anything.

const SESSION = process.env.SLOPPOD_SESSION ?? '2677e6a5-833c-466c-8a94-41eee6a260e8';
const dir = '.screenshots/real';

const shot = (page: Page, name: string) => page.screenshot({ path: `${dir}/${name}.png` });

async function openSession(page: Page) {
  await page.goto(`/sessions/${SESSION}`);
  await expect(page.getByTestId('transcript')).toBeVisible();
  await expect(page.getByTestId('message-user').first()).toBeVisible();
}

test('lists the session', async ({ page }) => {
  await page.goto('/');
  const row = page.locator(`a[href="/sessions/${SESSION}"]`);
  await expect(row).toBeVisible();
  await shot(page, 'sessions-desktop-light');
});

for (const [name, options] of [
  ['desktop-light', { viewport: { width: 1280, height: 800 }, colorScheme: 'light' }],
  ['desktop-dark', { viewport: { width: 1280, height: 800 }, colorScheme: 'dark' }],
  ['mobile-light', { viewport: { width: 390, height: 844 }, colorScheme: 'light', isMobile: true, hasTouch: true }],
  ['mobile-dark', { viewport: { width: 390, height: 844 }, colorScheme: 'dark', isMobile: true, hasTouch: true }],
] as const) {
  test.describe(name, () => {
    test.use(options);

    test('replays the history', async ({ page }) => {
      const frames: string[] = [];
      page.on('websocket', (socket) => socket.on('framereceived', ({ payload }) => frames.push(String(payload))));
      await openSession(page);

      const updates = frames.filter((frame) => frame.includes('"session/update"')).map((frame) => JSON.parse(frame).params.update);
      const userIds = new Set<string>();
      let anonymous = 0;
      let previous = '';
      for (const update of updates) {
        if (update.sessionUpdate === 'user_message_chunk') {
          if (update.messageId) userIds.add(update.messageId);
          else if (previous !== 'user_message_chunk') anonymous++;
        }
        if (update.sessionUpdate !== 'usage_update' && update.sessionUpdate !== 'available_commands_update') previous = update.sessionUpdate;
      }
      const tools = new Set(updates.filter((u) => u.sessionUpdate === 'tool_call').map((u) => u.toolCallId));
      const steers = updates.filter((u) => u.sessionUpdate === 'user_message_chunk' && u._meta?.sloppod?.steering);

      await expect(page.getByTestId('message-user')).toHaveCount(userIds.size + anonymous);
      await expect(page.getByTestId('tool-call')).toHaveCount(tools.size);
      await expect(page.locator('[data-testid="message-user"][data-steering]')).toHaveCount(steers.length);
      await shot(page, `session-${name}`);

      await page.getByTestId('message-user').first().scrollIntoViewIfNeeded();
      await page.getByTestId('tool-call').first().locator('summary').click();
      await shot(page, `session-top-${name}`);

      const propose = page.getByTestId('tool-call').filter({ hasText: 'propose pull request' });
      await propose.locator('summary').click();
      await page.locator('[data-testid="message-user"][data-steering]').first().scrollIntoViewIfNeeded();
      await shot(page, `session-steers-${name}`);

      await page.getByRole('button', { name: 'Proposals' }).click();
      const card = page.getByTestId('proposal-card').first();
      await expect(card).toBeVisible();
      await shot(page, `proposals-${name}`);
      await card.getByRole('button', { name: /View|Review/ }).click();
      await expect(page.getByTestId('proposal-detail').getByTestId('proposal-status')).toBeVisible();
      await page.waitForTimeout(500);
      await shot(page, `proposal-${name}`);
    });
  });
}
