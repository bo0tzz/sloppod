import { expect, test, type Page } from '@playwright/test';
import type { Proposal } from '../src/lib/api.ts';

const FINAL = /Done\. The tests pass/;

/** Starts a session from the empty chat. Prompts that mention "sloppod" get that repository. */
async function createSession(page: Page, prompt: string) {
  await page.goto('/');
  await page.getByRole('link', { name: 'New session' }).click();
  await expect(page).toHaveURL(/\/new$/);
  await expect(page.getByTestId('session-title')).toHaveText('New session');
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill(prompt);
  await box.press('Enter');
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

  const prompt = 'Add backoff to the reconnect logic in sloppod';
  await createSession(page, prompt);

  await expect(page.getByTestId('session-title')).toHaveText(prompt);
  await expect(page.getByTestId('session-subtitle')).toHaveText('bo0tzz/sloppod');
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
  await createSession(page, 'Answer my permission request in sloppod');

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
  const id = await createSession(page, 'Two viewers on sloppod');
  const other = await context.newPage();
  await other.goto(`/sessions/${id}`);

  await expect(permissionCard(page)).toBeVisible();
  await expect(permissionCard(other)).toBeVisible();
  await permissionCard(other).getByRole('button', { name: 'Allow', exact: true }).click();

  await expect(permissionCard(page)).toBeHidden();
  await expect(agentMessages(page).filter({ hasText: FINAL })).toBeVisible();
});

test('steers a queued message into the running turn', async ({ page }) => {
  await createSession(page, 'Steer me in sloppod');
  await expect(permissionCard(page)).toBeVisible();

  // Sending during a turn queues the message; Steer moves it into the turn.
  const steer = 'Use 250 ms as the base delay';
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill(steer);
  await page.getByRole('button', { name: 'Send' }).click();
  await expect(box).toHaveValue('');
  const queued = page.getByTestId('queued-message').filter({ hasText: steer });
  await expect(queued).toBeVisible();
  await expect(userMessages(page).filter({ hasText: steer })).toHaveCount(0);
  await queued.getByRole('button', { name: 'Steer' }).click();
  await expect(page.getByTestId('queued-messages')).toBeHidden();
  await expect(userMessages(page).filter({ hasText: steer })).toHaveCount(1);
  await expect(userMessages(page).filter({ hasText: steer })).toHaveAttribute('data-steering', 'true');

  await permissionCard(page).getByRole('button', { name: 'Allow', exact: true }).click();
  await expect(agentMessages(page).filter({ hasText: `Got your message mid-turn: “${steer}”` })).toBeVisible();
  await expect(agentMessages(page).filter({ hasText: FINAL })).toBeVisible();

  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');
});

test('runs a queued message as the next turn', async ({ page }) => {
  await createSession(page, 'Queue behind me in sloppod');
  await expect(permissionCard(page)).toBeVisible();

  const next = 'Then ask me something';
  await page.getByRole('textbox', { name: 'Prompt' }).fill(next);
  await page.getByRole('button', { name: 'Send' }).click();
  await expect(page.getByTestId('queued-message').filter({ hasText: next })).toBeVisible();

  // Survives a reload while it waits.
  await page.reload();
  await expect(page.getByTestId('queued-message').filter({ hasText: next })).toBeVisible();

  await permissionCard(page).getByRole('button', { name: 'Allow', exact: true }).click();
  await expect(agentMessages(page).filter({ hasText: FINAL })).toBeVisible();
  await expect(page.getByTestId('queued-messages')).toBeHidden();
  await expect(userMessages(page).filter({ hasText: next })).not.toHaveAttribute('data-steering');
  await expect(page.getByTestId('question-card')).toBeVisible();
});

test('typing goes to the message box unless something else is focused or selected', async ({ page }) => {
  await createSession(page, 'Type at me in sloppod');
  await finishTurn(page);
  const box = page.getByRole('textbox', { name: 'Prompt' });
  const message = agentMessages(page).filter({ hasText: FINAL });

  await message.click();
  await expect(box).not.toBeFocused();
  await page.keyboard.type('hi there');
  await expect(box).toBeFocused();
  await expect(box).toHaveValue('hi there');

  await box.fill('');
  await message.dblclick();
  await page.keyboard.type('x');
  await expect(box).toHaveValue('');

  await page.getByRole('button', { name: 'Proposals' }).focus();
  await page.keyboard.type('y');
  await expect(box).toHaveValue('');
});

test('stops a running turn', async ({ page }) => {
  await createSession(page, 'Stop me in sloppod');
  await expect(permissionCard(page)).toBeVisible();
  await page.getByRole('button', { name: 'Stop' }).click();
  await expect(permissionCard(page)).toBeHidden();
  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');
  await expect(page.getByTestId('tool-call').filter({ hasText: 'Edit ui/src/lib/acp/connection.ts' })).toHaveAttribute(
    'data-status',
    'failed',
  );
});

test('Esc stops a running turn, unless it closes a dialog first', async ({ page }) => {
  await createSession(page, 'Stop me with Esc in sloppod');
  await expect(permissionCard(page)).toBeVisible();

  await page.getByRole('button', { name: 'Proposals' }).click();
  const dialog = page.getByRole('dialog');
  await expect(dialog).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(dialog).toBeHidden();
  await expect(permissionCard(page)).toBeVisible();

  await page.getByRole('textbox', { name: 'Prompt' }).focus();
  await page.keyboard.press('Escape');
  await expect(permissionCard(page)).toBeHidden();
  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');
  await expect(page.getByRole('button', { name: 'Stop' })).toBeHidden();
});

test('approves a proposal with an edited title', async ({ page }) => {
  const id = await createSession(page, 'Propose a PR for sloppod');
  await finishTurn(page);

  const card = page.getByTestId('proposal-card');
  await expect(card).toHaveAttribute('data-status', 'pending');
  await card.getByRole('button', { name: 'Review' }).click();

  const detail = page.getByTestId('proposal-detail');
  await expect(detail.getByTestId('proposal-title')).toHaveText('Reconnect the ACP socket with exponential backoff');
  await expect(detail).toContainText('Back off exponentially when reconnecting');
  await expect(detail.getByTestId('diff')).toHaveCount(2);
  await expect(detail.getByTestId('diff').first()).toContainText('MAX_BACKOFF_MS');

  // The agent asked for auto-merge; the reviewer turns it off.
  await expect(detail.getByTestId('proposal-auto-merge')).toBeVisible();
  await page.getByRole('checkbox', { name: 'Merge when checks pass' }).click();
  await page.getByRole('button', { name: 'Edit' }).click();
  await page.getByRole('textbox', { name: 'Title' }).fill('Back off when reconnecting the ACP socket');
  await page.getByRole('button', { name: 'Approve with edits' }).click();

  await expect(detail).toBeHidden();
  await expect(page.getByTestId('proposal-card')).toBeHidden();
  await expect(page.getByTestId('system-notice')).toContainText(/github\.com\/bo0tzz\/sloppod\/pull\/\d+/);
  const proposals: Proposal[] = await (await page.request.get(`/api/v1/proposals?session=${id}`)).json();
  const pr = proposals.find((p) => p.kind === 'github.pr.create');
  expect(pr?.payload.title).toBe('Back off when reconnecting the ACP socket');
  expect(pr?.payload.auto_merge).toBe(false);
});

test('approves a stack of pull requests', async ({ page }) => {
  await createSession(page, 'Propose a stack for sloppod');
  const card = page.getByTestId('proposal-card');
  await expect(card).toContainText('2 pull requests, from “Back off when reconnecting”');
  await card.getByRole('button', { name: 'Review' }).click();

  const layers = page.getByTestId('stack-layer');
  await expect(layers).toHaveCount(2);
  await expect(layers.first()).toContainText('1. Back off when reconnecting');
  await expect(layers.last()).toContainText('2. Show the reconnect countdown');
  await expect(layers.last()).toContainText(/-ui → sloppod\/[0-9a-f]+-backoff/);
  // Layers start folded, so a long stack stays readable.
  await expect(layers.last().getByTestId('diff')).toBeHidden();
  await layers.last().getByText('2. Show the reconnect countdown').click();
  await expect(layers.last().getByTestId('diff')).toContainText('Reconnecting in {seconds} s');
  await layers.last().getByRole('heading', { name: /^Changes/ }).click();
  await expect(layers.last().getByTestId('diff')).toBeHidden();

  await layers.first().getByText('1. Back off when reconnecting').click();
  await page.getByRole('checkbox', { name: 'Merge when checks pass' }).click();
  await page.getByRole('button', { name: 'Approve', exact: true }).click();
  await expect(page.getByTestId('system-notice')).toContainText('as stack #7. The stack merges once its required checks pass.');
});

test('shows the review a proposal went through on request', async ({ page }) => {
  const id = await createSession(page, 'Propose a reviewed PR for sloppod');
  const put = await page.request.put(`/api/v1/sessions/${id}/review`, { data: { review: 'required' } });
  expect(put.status()).toBe(200);
  await finishTurn(page);
  await page.getByTestId('proposal-card').getByRole('button', { name: 'Review' }).click();

  const history = page.getByTestId('review-history');
  await expect(page.getByTestId('review-round')).toHaveCount(0);
  await history.getByText('Review by Codex').click();
  const rounds = history.getByTestId('review-round');
  await expect(rounds).toHaveCount(2);
  await expect(rounds.first()).toContainText('Changes requested');
  await expect(rounds.first().getByTestId('review-finding')).toHaveCount(2);
  await expect(rounds.first()).toContainText('ui/src/lib/acp/connection.ts:12');
  await expect(rounds.first().getByTestId('review-finding').first()).toContainText(/^1\.1\s*p1/);
  await expect(rounds.first()).toContainText('The backoff never resets');
  await expect(rounds.first()).toContainText('Fix: Reset the attempt counter');
  await expect(rounds.last()).toContainText('Resolved since the last round: 1.1, 1.2');
  await expect(rounds.last()).toContainText('Passed');
  await expect(rounds.last()).toContainText('Reset it after a successful session/load.');
  await expect(history.getByRole('link', { name: 'Open the review session' })).toHaveAttribute('href', /^\/sessions\/[0-9a-f-]+$/);

  // A review session opens like any other, but the session list doesn't take it in.
  await history.getByRole('link', { name: 'Open the review session' }).click();
  await expect(page.getByRole('heading', { level: 1 })).toContainText('Review:');
  await expect(page.getByRole('navigation', { name: 'Sessions' }).getByText('Review:')).toHaveCount(0);
});

test('an open proposal follows a decision made elsewhere', async ({ page }) => {
  const id = await createSession(page, 'Propose something for sloppod I will approve elsewhere');
  await finishTurn(page);
  await page.getByTestId('proposal-card').getByRole('button', { name: 'Review' }).click();
  await expect(page.getByRole('button', { name: 'Approve', exact: true })).toBeVisible();

  const proposals: { id: string }[] = await (await page.request.get(`/api/v1/proposals?session=${id}`)).json();
  const approved = await page.request.post(`/api/v1/proposals/${proposals[0].id}/approve`, { data: {} });
  expect(approved.status()).toBe(200);

  await expect(page.getByRole('button', { name: 'Approve', exact: true })).toBeHidden();
  await expect(page.getByTestId('proposal-result')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Done' })).toBeVisible();
});

test('retries loading a session that failed to load', async ({ page }) => {
  const sessions: { id: string; title: string }[] = await (await page.request.get('/api/v1/sessions')).json();
  const session = sessions.find((candidate) => candidate.title === 'Park and wake')!;
  let failures = 1;
  await page.route(`**/api/v1/sessions/${session.id}`, (route) =>
    failures-- > 0 ? route.fulfill({ status: 500, json: { error: 'database unavailable' } }) : route.fallback(),
  );
  await page.goto(`/sessions/${session.id}`);
  await expect(page.getByTestId('load-error')).toContainText('database unavailable');
  await expect(page.getByTestId('load-error')).toBeHidden();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Park and wake');
});

test('rejects a proposal with a reason', async ({ page }) => {
  await createSession(page, 'Propose something for sloppod I will reject');
  await finishTurn(page);

  await page.getByTestId('proposal-card').getByRole('button', { name: 'Review' }).click();
  await page.getByRole('button', { name: 'Reject…' }).click();
  await page.getByRole('textbox', { name: 'Reason (optional)' }).fill('Not now');
  await page.getByRole('button', { name: 'Reject', exact: true }).click();

  await expect(page.getByTestId('proposal-detail')).toBeHidden();
  await expect(page.getByTestId('system-notice')).toContainText('Your proposal was rejected: Not now');
});

test('files issues, leaving one out and editing another', async ({ page }) => {
  const id = await createSession(page, 'File issues for sloppod');
  const card = page.getByTestId('proposal-card');
  await expect(card).toHaveAttribute('data-kind', 'github.issue.create');
  await card.getByRole('button', { name: 'Review' }).click();

  const drafts = page.getByTestId('issue-draft');
  await expect(drafts).toHaveCount(3);
  await expect(drafts.nth(1)).toContainText('enforcement');
  await drafts.nth(2).getByRole('checkbox', { name: 'File this issue' }).click();
  await page.getByRole('button', { name: 'Edit' }).click();
  await drafts.nth(0).getByRole('textbox', { name: 'Issue title' }).fill('Steer at the next tool boundary');
  await page.getByRole('button', { name: 'File 2 issues' }).click();

  await expect(page.getByTestId('proposal-detail')).toBeHidden();
  await expect(page.getByTestId('system-notice')).toContainText(/issues\/\d+, https/);
  const proposals: Proposal[] = await (await page.request.get(`/api/v1/proposals?session=${id}`)).json();
  const filed = proposals.find((p) => p.kind === 'github.issue.create')!;
  expect(filed.status).toBe('executed');
  expect((filed.payload.issues as { title: string }[]).map((issue) => issue.title)).toEqual([
    'Steer at the next tool boundary',
    'Fail fast on blocked egress',
  ]);
});

async function expectTranscriptOnce(page: Page, prompt: string) {
  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(1);
  await expect(agentMessages(page).filter({ hasText: FINAL })).toHaveCount(1);
  await expect(agentMessages(page).filter({ hasText: "I'll start by reading" })).toHaveCount(1);
  await expect(page.getByTestId('tool-call')).toHaveCount(5);
  await expect(page.getByTestId('plan')).toHaveCount(1);
}

test('replays the history once after a page reload', async ({ page }) => {
  const prompt = 'Reload me in sloppod';
  await createSession(page, prompt);
  await finishTurn(page);
  await expectTranscriptOnce(page, prompt);

  await page.reload();
  await expectTranscriptOnce(page, prompt);
  await expect(page.getByTestId('proposal-card')).toHaveCount(1);
});

test('reconnects after the socket drops without duplicating history', async ({ page }) => {
  const prompt = 'Reconnect me in sloppod';
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

test('the sidebar shows a redeployed runtime once the connection is back', async ({ page }) => {
  await createSession(page, 'Reconnect me in sloppod');
  await finishTurn(page);
  const build = page.getByTestId('runtime-build');
  await expect(build).toHaveText('sloppod 0.0.1-main.1');

  try {
    await page.request.post('/_mock/build?version=0.0.1-main.2&build=v0.0.2');
    await page.request.post('/_mock/drop');
    await expect(build).toHaveText('sloppod 0.0.1-main.2');
    await expect(build).toHaveAttribute('href', 'https://github.com/bo0tzz/sloppod/releases/tag/v0.0.2');
  } finally {
    await page.request.post('/_mock/build');
  }
});

test('a message written while reconnecting is sent once the connection is back', async ({ page }) => {
  await createSession(page, 'Reconnect me in sloppod');
  await finishTurn(page);

  await page.request.post('/_mock/drop?for=3000');
  await expect(page.getByTestId('composer-hint')).toHaveText("Reconnecting… messages send once it's back.");
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('Written offline');
  await box.press('Enter');
  await expect(box).toHaveValue('');
  await expect(page.getByTestId('unsent-message')).toContainText('Written offline');

  await expect(userMessages(page).filter({ hasText: 'Written offline' })).toHaveCount(1, { timeout: 15_000 });
  await expect(page.getByTestId('unsent-messages')).toBeHidden();
});

test('a message written while the reconnect finishes loading is sent too', async ({ page }) => {
  await createSession(page, 'Reconnect me in sloppod');
  await finishTurn(page);

  // The socket comes back at once, but reading the session takes 3 s: the session loads and the view stays
  // reconnecting meanwhile.
  await page.request.post('/_mock/drop?slow=3000');
  await expect(page.getByTestId('composer-hint')).toHaveText("Reconnecting… messages send once it's back.");
  await page.waitForTimeout(500);
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('Written while loading');
  await box.press('Enter');
  await expect(page.getByTestId('unsent-message')).toContainText('Written while loading');

  await expect(userMessages(page).filter({ hasText: 'Written while loading' })).toHaveCount(1, { timeout: 15_000 });
  await expect(page.getByTestId('unsent-messages')).toBeHidden();
});

test('switches the model of a session', async ({ page, context }) => {
  const sent: string[] = [];
  page.on('websocket', (socket) => socket.on('framesent', ({ payload }) => sent.push(String(payload))));
  const id = await createSession(page, 'Switch models in sloppod');
  const select = page.getByTestId('model-select');
  await expect(select).toContainText('Default · Sonnet');

  const other = await context.newPage();
  await other.goto(`/sessions/${id}`);
  await expect(other.getByTestId('model-select')).toContainText('Default · Sonnet');

  await select.getByRole('combobox').click();
  // The default option says which model it currently is.
  await expect(page.getByRole('option', { name: 'Default · Sonnet' })).toBeVisible();
  await page.getByRole('option', { name: 'Opus' }).click();
  await expect(select).toContainText('Opus');
  expect(sent.some((frame) => frame.includes('"session/set_config_option"') && frame.includes('"opus"'))).toBe(true);
  expect(sent.some((frame) => frame.includes('"session/set_mode"'))).toBe(false);

  // Another viewer follows along, and the choice survives a reload.
  await expect(other.getByTestId('model-select')).toContainText('Opus');
  await page.reload();
  await expect(page.getByTestId('model-select')).toContainText('Opus');
});

test('saves the default model for new sessions', async ({ page }) => {
  const defaults = page.getByTestId('session-defaults');
  async function choose(name: string) {
    await page.goto('/settings');
    await defaults.getByRole('combobox', { name: 'Model' }).click();
    await page.getByRole('option', { name, exact: true }).click();
    await defaults.getByRole('button', { name: 'Save' }).click();
    await expect(defaults.getByRole('button', { name: 'Save' })).toBeDisabled();
  }

  await choose('Haiku');
  await page.reload();
  await expect(defaults.getByRole('combobox', { name: 'Model' })).toContainText('Haiku');

  await createSession(page, 'Use my default model in sloppod');
  await expect(page.getByTestId('model-select')).toContainText('Haiku');

  await choose('Agent default');
  await page.reload();
  await expect(defaults.getByRole('combobox', { name: 'Model' })).toContainText('Agent default');
});

test('saves a model id the agent does not list', async ({ page }) => {
  const defaults = page.getByTestId('session-defaults');
  await page.goto('/settings');
  await defaults.getByRole('combobox', { name: 'Model' }).click();
  await page.getByRole('option', { name: 'Other…', exact: true }).click();
  await defaults.getByRole('textbox', { name: 'Model id' }).fill('claude-mythos-5-1');
  await defaults.getByRole('button', { name: 'Save' }).click();
  await expect(defaults.getByRole('button', { name: 'Save' })).toBeDisabled();

  await page.reload();
  await expect(defaults.getByRole('combobox', { name: 'Model' })).toContainText('Other…');
  await expect(defaults.getByRole('textbox', { name: 'Model id' })).toHaveValue('claude-mythos-5-1');
  const settings: { model: string } = await (await page.request.get('/api/v1/settings')).json();
  expect(settings.model).toBe('claude-mythos-5-1');
  await page.request.put('/api/v1/settings', { data: { model: null } });
});

test('requires review by default, or per session', async ({ page }) => {
  const defaults = page.getByTestId('session-defaults');
  const policy = defaults.getByRole('combobox', { name: 'Review by Codex' });
  async function choose(name: string) {
    await page.goto('/settings');
    await policy.click();
    await page.getByRole('option', { name, exact: true }).click();
    await defaults.getByRole('button', { name: 'Save' }).click();
    await expect(defaults.getByRole('button', { name: 'Save' })).toBeDisabled();
  }

  await choose('Always, for pull requests');
  await page.reload();
  await expect(policy).toContainText('Always, for pull requests');
  const id = await createSession(page, 'Get my changes reviewed in sloppod');
  const review = async () => (await (await page.request.get(`/api/v1/sessions/${id}`)).json()).review;
  expect(await review()).toBe('required');

  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Stop requiring review' }).click();
  await expect.poll(review).toBe('off');
  await page.getByRole('button', { name: 'Session actions' }).click();
  await expect(page.getByRole('menuitem', { name: 'Require review by Codex' })).toBeVisible();
  await page.keyboard.press('Escape');

  await choose('Only when the agent asks');
});

test('queues messages while the agent starts', async ({ page }) => {
  const first = 'slow start: add backoff in sloppod';
  await createSession(page, first);

  // The first message is recorded straight away, before the agent is up.
  await expect(userMessages(page).filter({ hasText: first })).toHaveCount(1);
  await expect(phase(page)).toHaveAttribute('data-phase', 'starting');
  await expect(page.getByTestId('activity')).toHaveText('Starting the agent…');
  await expect(page.getByTestId('composer-hint')).toHaveText('The agent is starting; messages are queued.');

  const second = 'Also add a test for it';
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill(second);
  await page.getByRole('button', { name: 'Send' }).click();
  await expect(userMessages(page).filter({ hasText: second })).toHaveCount(1);
  await expect(box).toHaveValue('');

  // Both run as one turn once the agent is up.
  await expect(permissionCard(page)).toBeVisible({ timeout: 20_000 });
  await finishTurn(page);
  await expect(page.getByTestId('composer-hint')).toBeHidden();
});

test('follows new output while the reader is at the bottom, across a reconnect', async ({ page }) => {
  await page.setViewportSize({ width: 1000, height: 500 });
  await createSession(page, 'Give me a long answer in sloppod');
  const end = page.getByText('The end.');
  await expect(end).toBeInViewport({ timeout: 20_000 });

  await page.request.post('/_mock/drop');
  await expect(page.getByTestId('reconnecting')).toBeHidden();
  await expect(end).toBeInViewport();

  // Scrolling up stops the following; new output then leaves the view where it is.
  const scroller = page.getByTestId('transcript-scroller');
  const scrollTop = () => scroller.evaluate((element) => element.scrollTop);
  await end.hover();
  await page.mouse.wheel(0, -2000);
  await expect(end).not.toBeInViewport();
  const kept = await scrollTop();
  await page.request.post('/_mock/say?text=One%20more%20thing.');
  await expect(page.getByText('One more thing.')).toBeAttached();
  expect(await scrollTop()).toBe(kept);

  // So does dragging the scrollbar, however long the drag is held before it moves.
  await scroller.evaluate((element) => element.scrollTo({ top: element.scrollHeight }));
  await page.request.post('/_mock/say?text=Back%20at%20the%20bottom.');
  await expect(page.getByText('Back at the bottom.')).toBeInViewport();
  // A scrollbar drag scrolls with the pointer held and no wheel, touch or key.
  await page.getByText('Back at the bottom.').hover();
  await page.mouse.down();
  await page.waitForTimeout(1500);
  await scroller.evaluate((element) => element.scrollTo({ top: 0 }));
  const dragged = await scrollTop();
  await page.request.post('/_mock/say?text=Still%20held.');
  await expect(page.getByText('Still held.')).toBeAttached();
  expect(await scrollTop()).toBe(dragged);
  await page.mouse.up();
  await expect(page.getByText('Still held.')).not.toBeInViewport();
});

test('says what the pod is doing while the agent starts, and why a start failed', async ({ page }) => {
  await createSession(page, 'failing start: fix the build in bo0tzz/sloppod');
  const activity = page.getByTestId('activity');
  await expect(activity).toHaveText('Cloning https://github.com/bo0tzz/sloppod.git…');

  // The failure is in the transcript, and the session says it's trying again.
  const failure = page.getByText("The session's pod couldn't start the agent");
  await expect(failure).toBeVisible();
  await expect(page.getByText('preparing the pod: git clone exited with status 128')).toBeVisible();
  await expect(activity).toHaveText('Starting the agent again…');

  // The next start works; the failure stays in the transcript.
  await expect(permissionCard(page)).toBeVisible({ timeout: 20_000 });
  await finishTurn(page);
  await expect(failure).toBeVisible();
});

test('a long session opens at its latest messages, and older ones load on the way up', async ({ page }) => {
  const prompt = 'Write a long history in sloppod';
  await createSession(page, prompt);
  await expect(page.getByText('The end.')).toBeVisible({ timeout: 20_000 });

  await page.reload();
  await expect(page.getByText('The end.')).toBeVisible();
  const earlier = page.getByRole('button', { name: 'Load earlier messages' });
  await expect(earlier).toBeAttached();
  // The first message is in the page before; scrolling up to the top brings it in.
  await page.getByText('The end.').hover();
  for (let i = 0; i < 20 && (await earlier.count()) > 0; i++) await page.mouse.wheel(0, -5000);
  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(1);
  await expect(earlier).not.toBeAttached();
  await expect(page.getByText('The end.')).toBeVisible();
});

test('a message sent again after a reload runs once, even when it is older than the latest page', async ({ page }) => {
  await createSession(page, 'Write a long history in sloppod');
  await expect(page.getByText('The end.')).toBeVisible({ timeout: 20_000 });
  // Learn the id the page sends its next message under.
  let messageId = '';
  page.on('websocket', (socket) =>
    socket.on('framesent', ({ payload }) => {
      const message = JSON.parse(String(payload));
      if (message.method === 'session/prompt') messageId = message.params._meta.sloppod.messageId;
    }),
  );
  await page.reload();
  await expect(page.getByText('The end.')).toBeVisible();
  const again = 'Write a long history in sloppod, again';
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill(again);
  await box.press('Enter');
  await expect(page.getByText('The end.')).toHaveCount(2, { timeout: 20_000 });
  expect(messageId).not.toBe('');

  // As if the page reloaded before the runtime confirmed it: by then it's older than the latest page.
  const id = page.url().split('/').pop()!;
  await page.evaluate(
    ([session, message, text]) =>
      localStorage.setItem(`sloppod.unconfirmed.${session}`, JSON.stringify([{ id: message, text }])),
    [id, messageId, again],
  );
  await page.reload();
  await expect(page.getByText('The end.')).toBeVisible();
  await page.waitForTimeout(500);
  // Not run again: the latest page still ends with its answer, and nothing new was sent to the agent.
  await expect(userMessages(page).filter({ hasText: again })).toHaveCount(0);
  await expect(page.getByTestId('activity')).toBeHidden();
});

test('runs a session without a repository', async ({ page }) => {
  const prompt = 'Explain what the runtime does';
  await createSession(page, prompt);
  await expect(page.getByTestId('session-subtitle')).toHaveText('No repository');
  await permissionCard(page).getByRole('button', { name: 'Allow', exact: true }).click();
  await expect(agentMessages(page).filter({ hasText: 'This session has no repository' })).toBeVisible();
  await expect(page.getByTestId('proposal-card')).toHaveCount(0);

  const row = page.getByTestId('session-row').filter({ hasText: prompt });
  await expect(row).toBeVisible();
  await expect(row).not.toContainText('bo0tzz/');
});

test('approves a repository access request', async ({ page }) => {
  await createSession(page, 'Please request a repository for this');
  await expect(page.getByTestId('session-subtitle')).toHaveText('No repository');

  const card = page.getByTestId('proposal-card');
  await expect(card).toHaveAttribute('data-kind', 'sloppod.repo.grant');
  await expect(card).toContainText('Write access to immich-app/static-pages');
  // Decided on the card itself; there's nothing more to review.
  await expect(card.getByRole('button', { name: 'Review' })).toHaveCount(0);
  await expect(card.getByTestId('proposal-reason')).toContainText('this session has none yet');
  await card.getByRole('button', { name: 'Approve' }).click();
  await expect(card).toBeHidden();
  await expect(page.getByRole('dialog')).toHaveCount(0);

  // The session now has the repository, and sloppod's notice shows as a notice, not a user message.
  await expect(page.getByTestId('session-subtitle')).toHaveText('immich-app/static-pages');
  await expect(page.getByTestId('system-notice')).toContainText('You now have write access to immich-app/static-pages');
  await expect(userMessages(page)).toHaveCount(1);
  await expect(agentMessages(page).filter({ hasText: 'I can push to immich-app/static-pages now' })).toBeVisible();
});

test('adds a repository from the session menu', async ({ page }) => {
  await createSession(page, 'Just chat for now');
  await expect(page.getByTestId('session-subtitle')).toHaveText('No repository');

  const picker = page.getByTestId('add-repo');
  const openPicker = async () => {
    await page.getByRole('button', { name: 'Session actions' }).click();
    await page.getByRole('menuitem', { name: 'Add repository…' }).click();
    await picker.getByRole('combobox').click();
  };
  await openPicker();
  await page.getByRole('option', { name: 'bo0tzz/memini' }).click();
  await page.getByRole('button', { name: 'Add', exact: true }).click();
  await expect(page.getByTestId('session-subtitle')).toHaveText('bo0tzz/memini');

  // Repositories the session already has aren't offered again.
  await openPicker();
  await expect(page.getByRole('option', { name: 'bo0tzz/sloppod' })).toBeVisible();
  await expect(page.getByRole('option', { name: 'bo0tzz/memini' })).toHaveCount(0);
});

test('shows a proposal of an unknown kind generically', async ({ page }) => {
  const sessions: { id: string; title: string }[] = await (await page.request.get('/api/v1/sessions')).json();
  const session = sessions.find((candidate) => candidate.title === 'Park and wake')!;
  await page.goto(`/sessions/${session.id}`);

  const card = page.getByTestId('proposal-card');
  await expect(card).toHaveAttribute('data-kind', 'kube.apply');
  await card.getByRole('button', { name: 'Review' }).click();
  await expect(page.getByTestId('proposal-payload')).toContainText('"cluster": "home"');
  await page.getByRole('button', { name: 'Reject…' }).click();
  await page.getByRole('button', { name: 'Reject', exact: true }).click();
  await expect(page.getByTestId('proposal-detail')).toBeHidden();
  await expect(page.getByTestId('system-notice')).toContainText('Your proposal was rejected');
});

const questionCard = (page: Page) => page.getByTestId('question-card');
const questionField = (page: Page, key: string) => questionCard(page).locator(`[data-key="${key}"]`);

test('answers questions from the agent, in every tab', async ({ page, context }) => {
  const sent: string[] = [];
  page.on('websocket', (socket) => socket.on('framesent', ({ payload }) => sent.push(String(payload))));
  const id = await createSession(page, 'Please ask me how to do it');
  const other = await context.newPage();
  await other.goto(`/sessions/${id}`);

  await expect(questionCard(page)).toContainText('Please answer the following questions.');
  await expect(questionCard(other)).toBeVisible();
  await expect(phase(page)).toHaveAttribute('data-phase', 'waiting');
  await expect(page.getByTestId('activity')).toHaveText('Waiting for your answer');

  const backoff = questionField(page, 'question_0');
  await expect(backoff).toContainText('Backoff');
  await expect(backoff).toContainText('Which backoff strategy should reconnects use?');
  await expect(backoff).toContainText('Double the delay after each failure');
  await backoff.getByRole('radio', { name: /Exponential/ }).check();
  await expect(backoff.getByTestId('option-preview')).toContainText('Math.min(500 * 2 ** attempt');

  const tests = questionField(page, 'question_1');
  await tests.getByRole('checkbox', { name: /Unit/ }).check();
  await tests.getByRole('checkbox', { name: /End-to-end/ }).check();
  await questionField(page, 'question_1_custom').getByRole('textbox').fill('Load tests');
  await questionCard(page).getByRole('button', { name: 'Answer' }).click();

  await expect(questionCard(page)).toBeHidden();
  await expect(questionCard(other)).toBeHidden();
  await expect(agentMessages(page).filter({ hasText: 'Backoff: Exponential.' })).toContainText(
    'Tests: Unit, End-to-end, Load tests.',
  );
  const answer = sent.find((frame) => frame.includes('"action":"accept"'));
  expect(JSON.parse(answer!).result).toEqual({
    action: 'accept',
    content: { question_0: 'Exponential', question_1: ['Unit', 'End-to-end'], question_1_custom: 'Load tests' },
  });
});

test('Enter in the "Other" field submits the answer', async ({ page }) => {
  await createSession(page, 'Ask me before you start');
  const other = questionField(page, 'question_1_custom').getByRole('textbox');
  await other.fill('Load tests');
  await other.press('Enter');
  await expect(questionCard(page)).toBeHidden();
  await expect(agentMessages(page).filter({ hasText: 'Tests: Load tests.' })).toBeVisible();
});

test('skips questions from the agent', async ({ page }) => {
  await createSession(page, 'Ask me first');
  await questionCard(page).getByRole('button', { name: 'Skip' }).click();
  await expect(questionCard(page)).toBeHidden();
  await expect(agentMessages(page).filter({ hasText: 'You skipped the questions' })).toBeVisible();
  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');
});

test('stopping the turn cancels its question', async ({ page }) => {
  await createSession(page, 'Ask me first');
  await expect(questionCard(page)).toBeVisible();
  await page.getByRole('button', { name: 'Stop' }).click();
  await expect(questionCard(page)).toBeHidden();
  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');
});

test('a question asked with ask_user outlives the turn and a stop', async ({ page }) => {
  await createSession(page, 'Use ask_user about the backoff');
  await expect(questionCard(page)).toBeVisible();
  await expect(phase(page)).toHaveAttribute('data-phase', 'working');
  await page.getByRole('button', { name: 'Stop' }).click();
  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');
  await expect(questionCard(page)).toBeVisible();

  await questionField(page, 'question_0_custom').getByRole('textbox').fill('Exponential, capped');
  await questionCard(page).getByRole('button', { name: 'Answer' }).click();
  await expect(questionCard(page)).toBeHidden();
  await expect(page.getByText('sloppod: the user answered your question:', { exact: false })).toBeVisible();
  await expect(agentMessages(page).filter({ hasText: 'Thanks, going with that. Backoff: Exponential, capped.' })).toBeVisible();
});

test('shows an open question again after a reload', async ({ page }) => {
  await createSession(page, 'Ask me something, then reload');
  await expect(questionCard(page)).toBeVisible();
  await page.reload();
  await expect(questionCard(page)).toBeVisible();
  await questionField(page, 'question_0_custom').getByRole('textbox').fill('Whatever you think is best');
  await questionCard(page).getByRole('button', { name: 'Answer' }).click();
  await expect(agentMessages(page).filter({ hasText: 'Backoff: Whatever you think is best.' })).toBeVisible();
});

// Last, because it changes the mock's credentials; it puts them back at the end.
test('manages credentials and nudges when they are missing', async ({ page }) => {
  const section = (kind: string) => page.getByTestId(`credentials-${kind}`);
  const row = (kind: string, name: string) => section(kind).getByTestId('credential').filter({ hasText: name });
  const confirmDelete = async (kind: string, name: string) => {
    await row(kind, name).getByRole('button', { name: `Delete ${name}` }).click();
    await page.getByRole('button', { name: 'Delete', exact: true }).click();
    await expect(row(kind, name)).toHaveCount(0);
  };
  async function add(kind: string, noun: string, fields: Record<string, string>) {
    await section(kind).getByRole('button', { name: `Add ${noun}` }).click();
    for (const [label, value] of Object.entries(fields)) {
      await section(kind).getByLabel(label, { exact: true }).fill(value);
    }
    await section(kind).getByRole('button', { name: 'Save' }).click();
    await expect(row(kind, fields.Name)).toBeVisible();
  }

  await page.goto('/settings');
  await expect(page.getByTestId('session-subtitle')).toHaveText('Signed in as bo0tzz');
  await expect(page.getByTestId('current-user')).toHaveText('Signed in as bo0tzz');
  const build = page.getByTestId('runtime-build');
  await expect(build).toHaveText('sloppod 0.0.1-main.1');
  await expect(build).toHaveAttribute('title', 'Built from 11396d515413b2bec5e7c052b06d5df78cc68052');
  await expect(build).toHaveAttribute('href', 'https://github.com/bo0tzz/sloppod/commit/11396d515413b2bec5e7c052b06d5df78cc68052');
  await expect(row('claude', 'Personal').getByTestId('default-badge')).toBeVisible();
  await expect(row('github-app', 'sloppod-dev')).toContainText('App ID: 1234567');

  // A new kind's first credential becomes its default; secrets are never shown.
  await add('memini', 'memini server', { Name: 'Home', URL: 'https://memini.example.com', 'Admin key': 'top-secret-key' });
  await expect(row('memini', 'Home')).toContainText('URL: https://memini.example.com');
  await expect(row('memini', 'Home').getByTestId('default-badge')).toBeVisible();
  await expect(page.getByText('top-secret-key')).toHaveCount(0);

  await add('claude', 'Claude account', { Name: 'Work', Token: 'sk-ant-oat-work' });
  await expect(row('claude', 'Work').getByTestId('default-badge')).toHaveCount(0);
  await row('claude', 'Work').getByRole('button', { name: 'Make default' }).click();
  await expect(row('claude', 'Work').getByTestId('default-badge')).toBeVisible();
  await expect(row('claude', 'Personal').getByTestId('default-badge')).toHaveCount(0);

  await confirmDelete('memini', 'Home');
  await confirmDelete('claude', 'Personal');
  await confirmDelete('claude', 'Work');
  await confirmDelete('github-app', 'sloppod-dev');
  await expect(section('claude')).toContainText('No Claude account yet');

  await page.goto('/');
  await expect(page.getByTestId('nudge-claude')).toBeVisible();
  await expect(page.getByTestId('nudge-repos')).toBeVisible();

  // Without a Claude account the runtime refuses to start a session; the message stays.
  await page.goto('/new');
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('This should not start');
  await box.press('Enter');
  await expect(page.getByTestId('composer-error')).toContainText('No Claude account is configured');
  await expect(box).toHaveValue('This should not start');
  await page.getByTestId('composer-error').getByRole('link', { name: 'Add one in settings' }).click();
  await expect(page).toHaveURL(/\/settings$/);

  await add('claude', 'Claude account', { Name: 'Personal', Token: 'sk-ant-oat-personal' });
  await add('github-app', 'GitHub App', {
    Name: 'sloppod-dev',
    'App ID': '1234567',
    'Private key': '-----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY-----',
  });
  await page.goto('/');
  await expect(page.getByTestId('session-row').first()).toBeVisible();
  await expect(page.getByTestId('nudge-claude')).toHaveCount(0);
  await expect(page.getByTestId('nudge-repos')).toHaveCount(0);
});

test('a session whose pull requests are approved automatically', async ({ page }) => {
  const id = await createSession(page, 'Propose a PR for sloppod');
  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Approve its pull requests automatically' }).click();
  await page.getByRole('button', { name: 'Session actions' }).click();
  await expect(page.getByRole('menuitem', { name: 'Stop approving its pull requests' })).toBeVisible();
  await page.keyboard.press('Escape');
  await finishTurn(page);
  await expect(page.getByTestId('system-notice')).toContainText(/github\.com\/bo0tzz\/sloppod\/pull\/\d+/);
  const proposals: Proposal[] = await (await page.request.get(`/api/v1/proposals?session=${id}`)).json();
  expect(proposals[0].decided_by).toMatch(/^standing-approval:/);
  expect(proposals[0].payload.auto_merge).toBe(true);

  // Another session isn't covered while this one's approval is live.
  await createSession(page, 'Propose another PR for sloppod');
  await finishTurn(page);
  await expect(page.getByTestId('proposal-card')).toHaveAttribute('data-status', 'pending');

  // Settings lists it by session, and revokes it.
  await page.goto('/settings');
  const section = page.getByTestId('standing-approvals');
  await expect(section.getByTestId('standing-approval')).toContainText('Pull requests from Propose a PR for sloppod');
  await section.getByRole('button', { name: 'Revoke' }).click();
  await expect(section.getByTestId('standing-approval')).toHaveCount(0);
});

test('a stale Stop never turns automatic approval on', async ({ page }) => {
  const id = await createSession(page, 'Propose a PR for sloppod, stale menu');
  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Approve its pull requests automatically' }).click();
  await page.getByRole('button', { name: 'Session actions' }).click();
  await expect(page.getByRole('menuitem', { name: 'Stop approving its pull requests' })).toBeVisible();
  // Another tab revokes it while this menu still says Stop.
  const approvals: { id: string; session_id?: string }[] = await (await page.request.get('/api/v1/standing-approvals')).json();
  for (const approval of approvals.filter((a) => a.session_id === id)) {
    await page.request.delete(`/api/v1/standing-approvals/${approval.id}`);
  }
  await page.getByRole('menuitem', { name: 'Stop approving its pull requests' }).click();
  await page.getByRole('button', { name: 'Session actions' }).click();
  await expect(page.getByRole('menuitem', { name: 'Approve its pull requests automatically' })).toBeVisible();
  const after: { session_id?: string }[] = await (await page.request.get('/api/v1/standing-approvals')).json();
  expect(after.filter((a) => a.session_id === id)).toHaveLength(0);
});

test('stopping automatic approval revokes every approval of the session', async ({ page }) => {
  const id = await createSession(page, 'Propose a PR for sloppod twice approved');
  for (const target of [undefined, 'bo0tzz/sloppod']) {
    const created = await page.request.post('/api/v1/standing-approvals', {
      data: { kind: 'github.pr.create', session_id: id, target, edits: { auto_merge: true } },
    });
    expect(created.ok()).toBe(true);
  }
  await page.reload();
  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Stop approving its pull requests' }).click();
  await expect
    .poll(async () => {
      const approvals: { session_id?: string }[] = await (await page.request.get('/api/v1/standing-approvals')).json();
      return approvals.filter((a) => a.session_id === id).length;
    })
    .toBe(0);
  await page.getByRole('button', { name: 'Session actions' }).click();
  await expect(page.getByRole('menuitem', { name: 'Approve its pull requests automatically' })).toBeVisible();
});

test('a stale copy of a proposal does not undo its approval', async ({ page }) => {
  const id = await createSession(page, 'Propose a PR for sloppod');
  await finishTurn(page);
  const pr = page.getByTestId('proposal-card').and(page.locator('[data-kind="github.pr.create"]'));
  await expect(pr).toHaveAttribute('data-status', 'pending');

  // From now on, listing proposals returns what it did before the approval, like a refetch that was slower than
  // the approval. A marker proposal shows when that list has been applied.
  const stale: Proposal[] = await (await page.request.get(`/api/v1/proposals?session=${id}`)).json();
  const marker: Proposal = {
    id: 'stale-list-marker',
    session_id: id,
    kind: 'kube.apply',
    status: 'pending',
    payload: {},
    created_at: new Date().toISOString(),
  };
  await page.route(
    (url) => url.pathname === '/api/v1/proposals',
    (route) => route.fulfill({ json: [...stale, marker] }),
  );

  await pr.getByRole('button', { name: 'Review' }).click();
  await page.getByRole('button', { name: 'Approve', exact: true }).click();
  await expect(pr).toBeHidden();

  // A reconnect refetches proposals.
  await page.request.post('/_mock/drop');
  await expect(page.getByTestId('proposal-card').and(page.locator('[data-kind="kube.apply"]'))).toBeVisible();
  await expect(pr).toBeHidden();
});

test('wide screens show what waits on the user in a side panel', async ({ page }) => {
  await page.setViewportSize({ width: 1920, height: 1000 });
  await createSession(page, 'Propose a PR for sloppod');
  const panel = page.getByTestId('side-panel');
  await expect(panel.getByTestId('permission-request')).toBeVisible();
  await finishTurn(page);
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('File issues for sloppod');
  await box.press('Enter');

  await expect(panel.getByTestId('proposal-card')).toHaveCount(2);
  await expect(page.getByTestId('proposals-bar')).toHaveCount(0);

  // Without the room for both, several proposals fold into the bar above the composer.
  await page.setViewportSize({ width: 1280, height: 1000 });
  await expect(panel).toHaveCount(0);
  await expect(page.getByTestId('proposals-bar')).toContainText('2 proposals to review');
});

test('the sidebar folds away on narrow screens', async ({ page }) => {
  const sidebar = page.getByRole('navigation', { name: 'Sessions' });
  await page.setViewportSize({ width: 900, height: 800 });
  await createSession(page, 'Fold the sidebar away in sloppod');
  await expect(sidebar).toHaveCount(0);

  await page.getByRole('button', { name: 'Show sessions' }).click();
  await expect(sidebar.getByTestId('session-row').filter({ hasText: 'Fold the sidebar away' })).toBeVisible();
  // Picking a session folds it away again.
  await sidebar.locator('[data-testid="session-row"]:not([aria-current])').first().click();
  await expect(sidebar).toHaveCount(0);

  await page.getByRole('button', { name: 'Show sessions' }).click();
  await sidebar.getByRole('button', { name: 'Hide sessions' }).click();
  await expect(sidebar).toHaveCount(0);

  // On a wide screen it's open beside the page.
  await page.setViewportSize({ width: 1280, height: 800 });
  await expect(sidebar).toBeVisible();
});

test('lists sub-sessions under their parent', async ({ page }) => {
  await page.setViewportSize({ width: 1400, height: 900 });
  await createSession(page, 'Fan out the work in sloppod');
  await finishTurn(page);
  const parent = page.url().split('/').pop();
  const response = await page.request.post(`/_mock/spawn?parent=${parent}&task=Fix%20the%20docs`);
  expect(response.ok()).toBe(true);

  await page.reload();
  const list = page.getByTestId('session-list');
  const parentRow = list.locator('li').filter({ has: page.getByTestId('sub-sessions') });
  await expect(parentRow.getByTestId('session-row').first()).toContainText('Fan out the work');
  await expect(parentRow.getByTestId('sub-sessions').getByTestId('session-row')).toContainText('Fix the docs');
  // Not listed a second time at the top level.
  await expect(list.getByTestId('session-row').filter({ hasText: 'Fix the docs' })).toHaveCount(1);
});

test('deleting a session deletes its sub-sessions, also below one already deleted', async ({ page }) => {
  await createSession(page, 'Root of a tree in sloppod');
  const a = page.url().split('/').pop();
  const spawn = async (parent: string | undefined, task: string) =>
    (await (await page.request.post(`/_mock/spawn?parent=${parent}&task=${task}`)).json()).id as string;
  const b = await spawn(a, 'Middle');
  const c = await spawn(b, 'Leaf');
  expect((await page.request.delete(`/api/v1/sessions/${b}`)).ok()).toBe(true);
  expect((await page.request.post(`/api/v1/sessions/${c}/restore`)).ok()).toBe(true);
  expect((await page.request.delete(`/api/v1/sessions/${a}`)).ok()).toBe(true);
  const deleted = (await (await page.request.get('/api/v1/sessions/deleted')).json()) as { id: string }[];
  expect(deleted.map((s) => s.id)).toEqual(expect.arrayContaining([a, b, c]));
});

test('a message from another session shows folded under its summary', async ({ page }) => {
  await createSession(page, 'Wait for a report');
  await finishTurn(page);
  const summary = `Sub-session "Docs" reports it's done`;
  const text = `sloppod: your sub-session "Docs" reports that it's done: Rewrote the deploy guide.`;
  await page.request.post(
    `/_mock/notice?text=${encodeURIComponent(text)}&summary=${encodeURIComponent(summary)}`,
  );
  const notice = page.getByTestId('system-notice').filter({ hasText: summary });
  await expect(notice).toBeVisible();
  await expect(notice.getByText('Rewrote the deploy guide.')).toBeHidden();
  await notice.getByText(summary).click();
  await expect(notice.getByText('Rewrote the deploy guide.')).toBeVisible();
});

test('parks a session and wakes it by sending a message', async ({ page }) => {
  await page.setViewportSize({ width: 1400, height: 900 });
  await createSession(page, 'Park me in sloppod');
  await finishTurn(page);

  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Park' }).click();
  await expect(phase(page)).toHaveAttribute('data-phase', 'parked');
  const parked = page.getByTestId('parked-sessions');
  await expect(parked.getByTestId('session-row').filter({ hasText: 'Park me' })).toBeVisible();
  await expect(page.getByTestId('session-list').getByTestId('session-row').filter({ hasText: 'Park me' })).toHaveCount(0);

  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('Carry on');
  await box.press('Enter');
  await expect(phase(page)).not.toHaveAttribute('data-phase', 'parked');
  await expect(parked).toHaveCount(0);
});

test('wakes a parked session from the session menu', async ({ page }) => {
  await createSession(page, 'Park and wake me in sloppod');
  await finishTurn(page);
  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Park' }).click();
  await expect(phase(page)).toHaveAttribute('data-phase', 'parked');
  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Wake' }).click();
  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');
});

test('a list fetched before a delete, arriving late, leaves the session deleted', async ({ page }) => {
  await createSession(page, 'Remove this while a list is on its way');
  await finishTurn(page);
  const rows = page.getByTestId('session-row').filter({ hasText: 'Remove this while' });

  // Hold the next list, fetched while the session still exists, until after a newer one has arrived.
  let release: (() => void) | undefined;
  const released = new Promise<void>((resolve) => (release = resolve));
  let holding = false;
  await page.route('**/api/v1/sessions', async (route) => {
    if (holding || route.request().method() !== 'GET') return route.fallback();
    holding = true;
    const stale = await (await route.fetch()).json();
    await released;
    await route.fulfill({ json: stale });
  });
  await page.evaluate(() => document.dispatchEvent(new Event('visibilitychange')));
  await expect.poll(() => holding).toBe(true);

  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Delete session' }).click();
  await page.getByRole('button', { name: 'Delete', exact: true }).click();
  await expect(page).toHaveURL(/\/$/);
  const newer = page.waitForResponse((response) => response.url().endsWith('/api/v1/sessions') && response.status() === 200);
  await page.evaluate(() => document.dispatchEvent(new Event('visibilitychange')));
  await newer;
  await expect(rows).toHaveCount(0);

  const late = page.waitForResponse((response) => response.url().endsWith('/api/v1/sessions'));
  release!();
  await late;
  // Checked once, not retried: the next poll would hide a session the late list put back.
  await page.waitForTimeout(300);
  expect(await rows.count()).toBe(0);
});

test('a deleted session can be restored from Settings', async ({ page }) => {
  const id = await createSession(page, 'Delete me in sloppod, then restore me');
  await finishTurn(page);
  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Delete session' }).click();
  await expect(page.getByText('you can restore it from Settings')).toBeVisible();
  await page.getByRole('button', { name: 'Delete', exact: true }).click();
  await expect(page).toHaveURL(/\/$/);
  await expect(page.getByTestId('session-row').filter({ hasText: 'Delete me' })).toHaveCount(0);

  // Deleted, it stays parked until it's restored.
  expect((await page.request.post(`/api/v1/sessions/${id}/wake`)).status()).toBe(409);

  await page.goto('/settings');
  const deleted = page.getByTestId('deleted-sessions').getByTestId('deleted-session');
  await expect(deleted.filter({ hasText: 'Delete me' })).toBeVisible();
  await deleted.filter({ hasText: 'Delete me' }).getByRole('button', { name: 'Restore' }).click();
  await expect(deleted.filter({ hasText: 'Delete me' })).toHaveCount(0);

  // Back among the sessions, parked.
  const session: { phase: string; deleted_at?: string } = await (await page.request.get(`/api/v1/sessions/${id}`)).json();
  expect(session.phase).toBe('parked');
  expect(session.deleted_at).toBeUndefined();
});

test('a message cut off by a dropped connection is sent again, once', async ({ page }) => {
  await createSession(page, 'Keep my messages in sloppod');
  await finishTurn(page);
  await page.request.post('/_mock/drop-next-prompt?when=before');
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('Did this arrive?');
  await box.press('Enter');

  await expect(userMessages(page).filter({ hasText: 'Did this arrive?' })).toHaveCount(1);
  await expect(page.getByTestId('reconnecting')).toBeHidden();
  await expect(box).toHaveValue('');
  // Still once while the turn it started runs.
  await expect(phase(page)).not.toHaveAttribute('data-phase', 'idle');
  await page.waitForTimeout(500);
  await expect(userMessages(page).filter({ hasText: 'Did this arrive?' })).toHaveCount(1);
});

test('a message that arrived before the connection dropped is not put back', async ({ page }) => {
  await createSession(page, 'Do not repeat my messages in sloppod');
  await finishTurn(page);
  await page.request.post('/_mock/drop-next-prompt?when=after');
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('This one arrived');
  await box.press('Enter');

  await expect(page.getByTestId('reconnecting')).toBeVisible();
  await expect(page.getByTestId('reconnecting')).toBeHidden();
  await expect(userMessages(page).filter({ hasText: 'This one arrived' })).toHaveCount(1);
  await expect(box).toHaveValue('');
});

test('a message taken behind a turn is not sent again after its queue notice was lost', async ({ page }) => {
  await createSession(page, 'Queue behind a turn in sloppod');
  // The turn waits on a permission request, so it's still running.
  await expect(permissionCard(page)).toBeVisible();
  await page.request.post('/_mock/drop-next-prompt?when=after');
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('Behind the turn');
  await box.press('Enter');

  await expect(page.getByTestId('reconnecting')).toBeHidden();
  const queued = page.getByTestId('queued-message').filter({ hasText: 'Behind the turn' });
  await expect(queued).toHaveCount(1);
  await page.waitForTimeout(500);
  await expect(queued).toHaveCount(1);
});

test('a message that fails to arrive is kept apart from the draft', async ({ page }) => {
  await createSession(page, 'Lose a message in sloppod');
  await finishTurn(page);
  await page.request.post('/_mock/drop-next-prompt?when=before');
  await page.request.post('/_mock/fail-next-prompt');
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('Lost one');
  await box.press('Enter');
  await box.fill('My draft');

  const undelivered = page.getByTestId('undelivered-message');
  await expect(undelivered).toContainText('Lost one');
  await expect(box).toHaveValue('My draft');
  await undelivered.getByRole('button', { name: 'Put back' }).click();
  await expect(box).toHaveValue('My draft\n\nLost one');
  await expect(undelivered).toHaveCount(0);
});

test('lists the session grants from the session menu', async ({ page }) => {
  await createSession(page, 'Fix the build in bo0tzz/sloppod');
  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Grants' }).click();
  const grants = page.getByTestId('grants');
  await expect(grants.getByText("Claude's API, with the owner's Claude account.")).toBeVisible();
  await expect(grants.getByText(/^bo0tzz\/sloppod: read, and push the session branch `sloppod\//)).toBeVisible();
});

test('reviews and approves updates to existing issues', async ({ page }) => {
  await createSession(page, 'Tidy the issues in sloppod');
  const card = page.getByTestId('proposal-card');
  await expect(card).toHaveAttribute('data-kind', 'github.issue.update');
  await card.getByRole('button', { name: 'Review' }).click();
  const updates = page.getByTestId('issue-update');
  await expect(updates).toHaveCount(3);
  await expect(updates.nth(0)).toContainText('#100');
  await expect(updates.nth(0)).toContainText('close as completed');
  await expect(updates.nth(0)).toContainText('Remove labels: priority');
  await expect(updates.nth(1)).toContainText('close as a duplicate');
  await expect(updates.nth(2)).toContainText('New title: Startup failures show in the UI');

  // Drop the duplicate close, and reword the first comment.
  await updates.nth(1).getByRole('checkbox', { name: 'Make this update' }).click();
  await page.getByRole('button', { name: 'Edit' }).click();
  await updates.nth(0).getByRole('textbox', { name: 'Comment' }).fill('Fixed by #148, closing.');
  await page.getByRole('button', { name: 'Make 2 updates' }).click();
  await expect(page.getByTestId('system-notice').last()).toContainText('Your issue updates were approved and made');
  const proposals: { kind: string; payload: { updates: { number: number; comment?: string }[] } }[] = await (
    await page.request.get('/api/v1/proposals')
  ).json();
  const made = proposals.filter((p) => p.kind === 'github.issue.update').at(-1)!;
  expect(made.payload.updates.map((u) => u.number)).toEqual([100, 59]);
  expect(made.payload.updates[0].comment).toBe('Fixed by #148, closing.');
});

test('a draft survives a page reload', async ({ page }) => {
  await createSession(page, 'Keep my draft in sloppod');
  await finishTurn(page);
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('Half a thought');
  await page.reload();
  await expect(box).toHaveValue('Half a thought');
  await box.fill('');
  await page.reload();
  await expect(box).toHaveValue('');
});

test('a message a reload cut off is sent once the session loads', async ({ page }) => {
  const id = await createSession(page, 'Send what I left unconfirmed in sloppod');
  await finishTurn(page);
  // As if the page reloaded after sending, before the runtime answered.
  await page.evaluate(
    ([session, messageId]) =>
      localStorage.setItem(
        `sloppod.unconfirmed.${session}`,
        JSON.stringify([{ id: messageId, text: 'Sent just before the reload' }]),
      ),
    [id, crypto.randomUUID()],
  );
  await page.reload();
  await expect(userMessages(page).filter({ hasText: 'Sent just before the reload' })).toHaveCount(1);
  await finishTurn(page);
  // Settled: not sent again after the next reload.
  await page.reload();
  await expect(userMessages(page).filter({ hasText: 'Sent just before the reload' })).toHaveCount(1);
  expect(await page.evaluate((session) => localStorage.getItem(`sloppod.unconfirmed.${session}`), id)).toBeNull();
});

test('a browser whose storage is full still sends', async ({ page }) => {
  await page.addInitScript(() => {
    Storage.prototype.setItem = () => {
      throw new DOMException('full', 'QuotaExceededError');
    };
  });
  await createSession(page, 'Work without storage in sloppod');
  await finishTurn(page);
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('Sent anyway');
  await box.press('Enter');
  await expect(userMessages(page).filter({ hasText: 'Sent anyway' })).toHaveCount(1);
  await finishTurn(page);
});
