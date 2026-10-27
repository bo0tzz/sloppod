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
  await expect(layers.last().getByTestId('diff')).toContainText('Reconnecting in {seconds} s');

  await page.getByRole('button', { name: 'Approve', exact: true }).click();
  await expect(page.getByTestId('system-notice')).toContainText('as stack #7');
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

test('a standing approval approves matching proposals as they are made', async ({ page }) => {
  await page.goto('/settings');
  const section = page.getByTestId('standing-approvals');
  // The defaults: pull requests on any repository, until revoked, merged when checks pass.
  await section.getByRole('button', { name: 'Add' }).click();
  await expect(section.getByTestId('standing-approval')).toContainText('Pull requests on any repository');

  const id = await createSession(page, 'Propose a PR for sloppod');
  await finishTurn(page);
  await expect(page.getByTestId('system-notice')).toContainText(/github\.com\/bo0tzz\/sloppod\/pull\/\d+/);
  const proposals: Proposal[] = await (await page.request.get(`/api/v1/proposals?session=${id}`)).json();
  expect(proposals[0].decided_by).toMatch(/^standing-approval:/);
  expect(proposals[0].payload.auto_merge).toBe(true);

  // Revoked, so later proposals wait for review again.
  await page.goto('/settings');
  await section.getByRole('button', { name: 'Revoke' }).click();
  await expect(section.getByTestId('standing-approval')).toHaveCount(0);
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

test('lists the session grants from the session menu', async ({ page }) => {
  await createSession(page, 'Fix the build in bo0tzz/sloppod');
  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Grants' }).click();
  const grants = page.getByTestId('grants');
  await expect(grants.getByText("Claude's API, with the owner's Claude account.")).toBeVisible();
  await expect(grants.getByText(/^bo0tzz\/sloppod: read, and push the session branch `sloppod\//)).toBeVisible();
});
