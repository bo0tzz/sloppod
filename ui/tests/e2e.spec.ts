import { expect, test, type Page } from '@playwright/test';

const FINAL = /Done\. The tests pass/;

async function createSession(page: Page, prompt: string) {
  await page.goto('/');
  await page.getByRole('link', { name: 'New session' }).click();
  await expect(page).toHaveURL(/\/new$/);
  await page.getByRole('combobox').click();
  await page.getByRole('option', { name: 'bo0tzz/sloppod' }).click();
  await page.getByRole('textbox', { name: 'Prompt' }).fill(prompt);
  await page.getByRole('button', { name: 'Start session' }).click();
  await expect(page).toHaveURL(/\/sessions\/[0-9a-f-]+$/);
  return page.url().split('/').pop()!;
}

const permissionCard = (page: Page) => page.getByTestId('permission-request');
const phase = (page: Page) => page.getByTestId('session-screen').getByTestId('phase');
const agentMessages = (page: Page) => page.getByTestId('message-agent');
const userMessages = (page: Page) => page.getByTestId('message-user');

async function finishTurn(page: Page) {
  await permissionCard(page).getByRole('button', { name: 'Allow', exact: true }).click();
  await expect(agentMessages(page).filter({ hasText: FINAL })).toBeVisible();
  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');
}

test('creates a session and streams the agent output', async ({ page }) => {
  const chunkFrames: string[] = [];
  page.on('websocket', (socket) =>
    socket.on('framereceived', ({ payload }) => {
      if (typeof payload === 'string' && payload.includes('"agent_message_chunk"')) chunkFrames.push(payload);
    }),
  );

  const prompt = 'Add backoff to the reconnect logic';
  await createSession(page, prompt);

  await expect(page.getByTestId('session-title')).toHaveText(prompt);
  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(1);

  // The first agent message arrives as several chunks and renders as one message.
  const first = agentMessages(page).first();
  await expect(first).toHaveText("I'll start by reading the connection code to see how reconnects work today.");
  expect(chunkFrames.length).toBeGreaterThan(3);
  await expect(agentMessages(page).filter({ hasText: "I'll start" })).toHaveCount(1);

  await expect(page.getByTestId('thought').first()).toBeVisible();
  await expect(page.getByTestId('plan')).toBeVisible();
  await expect(page.getByTestId('tool-call').filter({ hasText: 'Read ui/src/lib/acp/connection.ts' })).toHaveAttribute(
    'data-status',
    'completed',
  );

  // The session shows up in the sidebar with its live phase.
  await expect(page.getByTestId('session-row').filter({ hasText: prompt })).toBeVisible();
});

test('answers a permission request', async ({ page }) => {
  await createSession(page, 'Answer my permission request');

  const card = permissionCard(page);
  await expect(card).toBeVisible();
  await expect(card).toContainText('Edit ui/src/lib/acp/connection.ts');
  await expect(card.getByTestId('diff')).toContainText('MAX_BACKOFF_MS');
  await expect(phase(page)).toHaveAttribute('data-phase', 'waiting');

  await card.getByRole('button', { name: 'Allow', exact: true }).click();
  await expect(card).toBeHidden();
  await expect(page.getByTestId('tool-call').filter({ hasText: 'Edit ui/src/lib/acp/connection.ts' })).toHaveAttribute(
    'data-status',
    'completed',
  );

  // The terminal-style output of the test run shows once expanded.
  const testRun = page.getByTestId('tool-call').filter({ hasText: 'npm test -- reconnect' });
  await expect(testRun).toHaveAttribute('data-status', 'completed');
  await testRun.locator('summary').click();
  await expect(testRun.getByTestId('tool-output')).toContainText('3 passed');

  await expect(agentMessages(page).filter({ hasText: FINAL })).toBeVisible();
});

test('a permission answered elsewhere disappears', async ({ page, context }) => {
  const id = await createSession(page, 'Two viewers');
  const other = await context.newPage();
  await other.goto(`/sessions/${id}`);

  await expect(permissionCard(page)).toBeVisible();
  await expect(permissionCard(other)).toBeVisible();
  await permissionCard(other).getByRole('button', { name: 'Allow', exact: true }).click();

  await expect(permissionCard(page)).toBeHidden();
  await expect(agentMessages(page).filter({ hasText: FINAL })).toBeVisible();
});

test('steers the agent during a turn', async ({ page }) => {
  await createSession(page, 'Steer me');
  await expect(permissionCard(page)).toBeVisible();

  const steer = 'Use 250 ms as the base delay';
  const box = page.getByRole('textbox', { name: 'Steering message' });
  await box.fill(steer);
  await page.getByRole('button', { name: 'Steer' }).click();
  await expect(box).toHaveValue('');
  await expect(userMessages(page).filter({ hasText: steer })).toHaveCount(1);

  await permissionCard(page).getByRole('button', { name: 'Allow', exact: true }).click();
  await expect(agentMessages(page).filter({ hasText: `Got your message mid-turn: “${steer}”` })).toBeVisible();
  await expect(agentMessages(page).filter({ hasText: FINAL })).toBeVisible();

  // Once the turn is over the same box sends a normal prompt again.
  await expect(page.getByRole('button', { name: 'Send' })).toBeVisible();
});

test('stops a running turn', async ({ page }) => {
  await createSession(page, 'Stop me');
  await expect(permissionCard(page)).toBeVisible();
  await page.getByRole('button', { name: 'Stop' }).click();
  await expect(permissionCard(page)).toBeHidden();
  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');
  await expect(page.getByTestId('tool-call').filter({ hasText: 'Edit ui/src/lib/acp/connection.ts' })).toHaveAttribute(
    'data-status',
    'failed',
  );
});

test('approves a proposal with an edited title', async ({ page }) => {
  await createSession(page, 'Propose a PR');
  await finishTurn(page);

  const card = page.getByTestId('proposal-card');
  await expect(card).toHaveAttribute('data-status', 'pending');
  await card.getByRole('button', { name: 'Review' }).click();

  const detail = page.getByTestId('proposal-detail');
  await expect(detail.getByTestId('proposal-title')).toHaveText('Reconnect the ACP socket with exponential backoff');
  await expect(detail).toContainText('Back off exponentially when reconnecting');
  await expect(detail.getByTestId('diff')).toHaveCount(2);
  await expect(detail.getByTestId('diff').first()).toContainText('MAX_BACKOFF_MS');

  await page.getByRole('button', { name: 'Edit' }).click();
  await page.getByRole('textbox', { name: 'Title' }).fill('Back off when reconnecting the ACP socket');
  await page.getByRole('button', { name: 'Approve with edits' }).click();

  await expect(detail.getByTestId('proposal-status')).toHaveText('Opened');
  await expect(detail.getByTestId('proposal-result')).toHaveAttribute('href', /github\.com\/bo0tzz\/sloppod\/pull\/\d+/);
  await expect(detail.getByTestId('proposal-title')).toHaveText('Back off when reconnecting the ACP socket');

  await page.getByRole('button', { name: 'Close' }).click();
  await expect(page.getByTestId('proposal-card')).toBeHidden();
});

test('rejects a proposal with a reason', async ({ page }) => {
  await createSession(page, 'Propose something I will reject');
  await finishTurn(page);

  await page.getByTestId('proposal-card').getByRole('button', { name: 'Review' }).click();
  await page.getByRole('button', { name: 'Reject…' }).click();
  await page.getByRole('textbox', { name: 'Reason (optional)' }).fill('Not now');
  await page.getByRole('button', { name: 'Reject', exact: true }).click();

  const detail = page.getByTestId('proposal-detail');
  await expect(detail.getByTestId('proposal-status')).toHaveText('Rejected');
  await expect(detail).toContainText('Not now');
});

async function expectTranscriptOnce(page: Page, prompt: string) {
  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(1);
  await expect(agentMessages(page).filter({ hasText: FINAL })).toHaveCount(1);
  await expect(agentMessages(page).filter({ hasText: "I'll start by reading" })).toHaveCount(1);
  await expect(page.getByTestId('tool-call')).toHaveCount(5);
  await expect(page.getByTestId('plan')).toHaveCount(1);
}

test('replays the history once after a page reload', async ({ page }) => {
  const prompt = 'Reload me';
  await createSession(page, prompt);
  await finishTurn(page);
  await expectTranscriptOnce(page, prompt);

  await page.reload();
  await expectTranscriptOnce(page, prompt);
  await expect(page.getByTestId('proposal-card')).toHaveCount(1);
});

test('reconnects after the socket drops without duplicating history', async ({ page }) => {
  const prompt = 'Reconnect me';
  await createSession(page, prompt);
  await finishTurn(page);
  await expectTranscriptOnce(page, prompt);

  const response = await page.request.post('/_mock/drop');
  expect((await response.json()).dropped).toBeGreaterThan(0);
  await expect(page.getByRole('button', { name: 'Send' })).toBeVisible();
  await expect(page.getByTestId('reconnecting')).toBeHidden();
  await expectTranscriptOnce(page, prompt);

  // The new connection is live: a second prompt runs a new turn.
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('And once more');
  await box.press('Enter');
  await expect(userMessages(page).filter({ hasText: 'And once more' })).toHaveCount(1);
  await expect(permissionCard(page)).toBeVisible();
});

test('switches the permission mode', async ({ page }) => {
  await createSession(page, 'Switch modes');
  await expect(permissionCard(page)).toBeVisible();
  await permissionCard(page).getByRole('button', { name: 'Always allow edits' }).click();
  await expect(page.getByTestId('mode-select')).toContainText('Accept edits');
  await expect(agentMessages(page).filter({ hasText: FINAL })).toBeVisible();

  await page.getByTestId('mode-select').getByRole('combobox').click();
  await page.getByRole('option', { name: 'Default' }).click();
  await expect(page.getByTestId('mode-select')).toContainText('Default');
  await page.reload();
  await expect(page.getByTestId('mode-select')).toContainText('Default');
});
