import { readFile } from 'node:fs/promises';
import { expect, test, type Page, type WebSocket as PageSocket, type WebSocketRoute } from '@playwright/test';
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

/** Opens every fold of tool calls in the transcript, to see the calls in them. */
async function expandFolds(page: Page) {
  for (const summary of await page.locator('[data-testid="tool-fold"]:not([open]) > summary').all()) await summary.click();
}

/** How many tool calls the transcript holds, folded or not. */
async function toolCallsInTranscript(page: Page) {
  const folds = await page.getByTestId('tool-fold-count').allTextContents();
  return folds.reduce((total, text) => total + Number.parseInt(text, 10), 0) + (await page.getByTestId('tool-call').count());
}

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
  // What it says, without the time its toolbar has.
  await expect(first.locator('.markdown').first()).toHaveText("I'll start by reading the connection code to see how reconnects work today.");
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

test("copies an agent message's Markdown", async ({ page, context }) => {
  await context.grantPermissions(['clipboard-read', 'clipboard-write']);
  await createSession(page, 'Add backoff to the reconnect logic in sloppod');
  await finishTurn(page);

  const message = agentMessages(page).filter({ hasText: FINAL });
  const copy = message.getByRole('button', { name: 'Copy message' });
  const actions = message.getByTestId('message-actions');
  await page.mouse.move(0, 0);
  await expect(actions).toHaveCSS('opacity', '0');
  await message.hover();
  await expect(actions).toHaveCSS('opacity', '1');

  await copy.click();
  await expect(message.getByRole('button', { name: 'Copied' })).toBeVisible();
  const copied = await page.evaluate(() => navigator.clipboard.readText());
  expect(copied).toMatch(/^Done\. The tests pass and I've proposed a pull request, \*\*.+\*\*, from `sloppod\/.+`\./);
  await expect(message.getByRole('button', { name: 'Copy message' })).toBeVisible();

  await expect(userMessages(page).getByRole('button', { name: 'Copy message' })).toHaveCount(0);

  // A denied clipboard says so on the button.
  await page.evaluate(() => {
    navigator.clipboard.writeText = () => Promise.reject(new DOMException('Denied', 'NotAllowedError'));
  });
  await message.getByRole('button', { name: 'Copy message' }).click();
  await expect(message.getByRole('button', { name: "Couldn't copy" })).toBeVisible();
  await expect(message.getByRole('status')).toHaveText("Couldn't copy the message");
});

test("downloads a debug bundle for an agent message's turn", async ({ page }) => {
  await createSession(page, 'Add backoff to the reconnect logic in sloppod');
  await finishTurn(page);

  const message = agentMessages(page).filter({ hasText: FINAL });
  await message.hover();
  const download = page.waitForEvent('download');
  await message.getByRole('link', { name: 'Download debug bundle' }).click();
  const file = await download;
  expect(file.suggestedFilename()).toMatch(/^sloppod-debug-.+\.json$/);
  const bundle = JSON.parse(await readFile(await file.path(), 'utf8'));
  // The turn is the one the user's message started.
  const first = bundle.turns.at(-1).events[0];
  expect(first.update.sessionUpdate).toBe('user_message_chunk');
  expect(first.update.content.text).toBe('Add backoff to the reconnect logic in sloppod');
  expect(bundle.turn).toBe(first.update.messageId);
  expect(bundle.turns.at(-1).model_calls.length).toBeGreaterThan(0);

  await expect(userMessages(page).getByRole('link', { name: 'Download debug bundle' })).toHaveCount(0);
});

test("a message shows when it was made on hover, in the user's timezone", async ({ page }) => {
  await createSession(page, 'Add backoff to the reconnect logic in sloppod');
  await finishTurn(page);

  // The user's own message has no toolbar: the time shows in its corner on hover.
  const mine = userMessages(page).first();
  const mineTime = mine.getByTestId('message-time');
  await page.mouse.move(0, 0);
  await expect(mineTime).toHaveCSS('opacity', '0');
  // The bubble is at the right of a full-width row, so it's the bubble that's hovered.
  await mine.locator('> div').hover();
  await expect(mineTime).toHaveCSS('opacity', '1');
  await expect(mineTime).toHaveText(/^\d{2}:\d{2}$/);
  // The full date and time are in its tooltip.
  await expect(mineTime).toHaveAttribute('title', new RegExp(`${new Date().getFullYear()}`));

  // The agent's go in its toolbar, with copy and the debug bundle.
  const theirs = agentMessages(page).filter({ hasText: FINAL });
  await page.mouse.move(0, 0);
  await theirs.hover();
  await expect(theirs.getByTestId('message-actions').getByTestId('message-time')).toHaveText(/^\d{2}:\d{2}$/);

  // In the timezone the user has set: the same moment, as Tokyo has it.
  const settings = await (await page.request.get('/api/v1/settings')).json();
  await page.request.put('/api/v1/settings', { data: { timezone: 'Asia/Tokyo' } });
  try {
    await page.reload();
    const at = await userMessages(page).first().getByTestId('message-time').getAttribute('datetime');
    const tokyo = new Intl.DateTimeFormat('en-GB', { hour: '2-digit', minute: '2-digit', hourCycle: 'h23', timeZone: 'Asia/Tokyo' }).format(new Date(at!));
    await expect(userMessages(page).first().getByTestId('message-time')).toHaveText(tokyo);
  } finally {
    await page.request.put('/api/v1/settings', { data: { timezone: settings.timezone } });
  }
});

test("a message from before today says which day, and one from before the last week the date", async ({ page }) => {
  await createSession(page, 'Give me a long answer in sloppod');
  await expect(agentMessages(page).filter({ hasText: 'The end.' })).toBeVisible({ timeout: 20_000 });
  const chunk = (text: string) => ({ sessionUpdate: 'agent_message_chunk', messageId: text, content: { type: 'text', text } });
  const ago = (days: number) => new Date(Date.now() - days * 86_400_000).toISOString();
  await page.request.post(`/_mock/update?at=${ago(2)}`, { data: chunk('Two days ago.') });
  await page.request.post(`/_mock/update?at=${ago(10)}`, { data: chunk('Ten days ago.') });
  await page.request.post(`/_mock/update?at=${new Date().toISOString()}`, { data: chunk('Just now.') });

  const time = (text: string) => agentMessages(page).filter({ hasText: text }).getByTestId('message-time');
  await agentMessages(page).filter({ hasText: 'Two days ago.' }).hover();
  await expect(time('Two days ago.')).toHaveText(/^[A-Z][a-z]{2} \d{2}:\d{2}$/);
  await agentMessages(page).filter({ hasText: 'Ten days ago.' }).hover();
  await expect(time('Ten days ago.')).toHaveText(/^\d{1,2} [A-Z][a-z]{2} \d{2}:\d{2}$/);
  await agentMessages(page).filter({ hasText: 'Just now.' }).hover();
  await expect(time('Just now.')).toHaveText(/^\d{2}:\d{2}$/);
  // After a reload the times come from the log.
  await page.reload();
  await expect(time('Ten days ago.')).toHaveText(/^\d{1,2} [A-Z][a-z]{2} \d{2}:\d{2}$/);
});

test('consecutive tool calls fold into one block between two messages, and expand to every call', async ({ page }) => {
  await createSession(page, 'Use several tools in a row');
  await expect(agentMessages(page).filter({ hasText: 'That is all of it.' })).toBeVisible();
  const fold = page.getByTestId('tool-fold');
  await expect(fold).toHaveCount(1);
  await expect(fold.getByTestId('tool-fold-count')).toHaveText('5 tool calls');
  // Folded, the calls aren't drawn at all.
  await expect(page.getByTestId('tool-call')).toHaveCount(0);
  const order = await page
    .getByTestId('transcript')
    .locator('[data-testid="message-agent"], [data-testid="tool-fold"]')
    .evaluateAll((nodes) => nodes.map((node) => node.getAttribute('data-testid')));
  expect(order).toEqual(['message-agent', 'tool-fold', 'message-agent']);

  await fold.locator(':scope > summary').click();
  await expect(page.getByTestId('tool-call')).toHaveCount(5);
  await expect(page.getByTestId('tool-call').first()).toContainText('Read src/one.ts');
  await expect(page.getByTestId('tool-call').last()).toContainText('Read src/five.ts');
  await fold.locator(':scope > summary').click();
  await expect(page.getByTestId('tool-call')).toHaveCount(0);
});

test('the thoughts between tool calls fold with them, the ones before and after stay out', async ({ page }) => {
  await createSession(page, 'Think between tools');
  await expect(agentMessages(page).filter({ hasText: 'Here is the answer.' })).toBeVisible();
  const fold = page.getByTestId('tool-fold');
  await expect(fold).toHaveCount(1);
  // Three calls, however many thoughts come between them.
  await expect(fold.getByTestId('tool-fold-count')).toHaveText('3 tool calls');
  // A thought before the first call and one after the last stay in the open.
  const order = await page
    .getByTestId('transcript')
    .locator('[data-testid="thought"], [data-testid="tool-fold"], [data-testid="message-agent"]')
    .evaluateAll((nodes) => nodes.map((node) => node.getAttribute('data-testid')));
  expect(order).toEqual(['thought', 'tool-fold', 'thought', 'message-agent']);

  // Opened, the fold holds the three calls and the two thoughts between them.
  await fold.locator(':scope > summary').click();
  await expect(page.getByTestId('tool-call')).toHaveCount(3);
  await expect(page.getByTestId('thought')).toHaveCount(4);
});

test('a highlight and a failed call among tool calls stay outside the folds', async ({ page }) => {
  await createSession(page, 'Use several tools in a row, and a highlight');
  await expect(agentMessages(page).filter({ hasText: 'That is all of it.' })).toBeVisible();
  // Three calls, then the highlight and a failed call, which show as they are, then two more.
  const order = await page
    .getByTestId('transcript')
    .locator('[data-testid="message-agent"], [data-testid="tool-fold"], [data-testid="highlight-card"], [data-testid="tool-call"]')
    .evaluateAll((nodes) => nodes.map((node) => node.getAttribute('data-testid')));
  expect(order).toEqual(['message-agent', 'tool-fold', 'highlight-card', 'tool-call', 'tool-fold', 'message-agent']);
  await expect(page.getByTestId('highlight-card')).toContainText('The answer is in src/three.ts');
  await expect(page.getByTestId('tool-call')).toHaveAttribute('data-status', 'failed');
  await expect(page.getByTestId('tool-fold-count')).toHaveText(['3 tool calls', '2 tool calls']);
});

test('a fold shows the call that is running', async ({ page }) => {
  await createSession(page, 'Use several tools in a row, slowly');
  const fold = page.getByTestId('tool-fold');
  await expect(fold.getByTestId('tool-fold-running')).toHaveText('Read src/slow.ts');
  await expect(fold).toHaveAttribute('data-running', 'true');
  await expect(agentMessages(page).filter({ hasText: 'That is all of it.' })).toBeVisible();
  await expect(fold).toHaveAttribute('data-running', 'false');
  await expect(fold.getByTestId('tool-fold-running')).toHaveCount(0);
});

test('a debug bundle finds the turn of a message loaded without the message that started it', async ({ page }) => {
  const prompt = 'Write a long history in sloppod';
  await createSession(page, prompt);
  await expect(page.getByText('The end.')).toBeVisible({ timeout: 20_000 });
  const firstEvent = async () => {
    const words = agentMessages(page).filter({ hasText: 'word word' }).first();
    await words.hover();
    const download = page.waitForEvent('download');
    await words.getByRole('link', { name: 'Download debug bundle' }).click();
    const bundle = JSON.parse(await readFile(await (await download).path(), 'utf8'));
    return bundle.turns.at(-1).events[0];
  };

  // The loaded history begins inside the latest turn.
  await page.reload();
  await expect(page.getByText('The end.')).toBeVisible();
  await expect(userMessages(page)).toHaveCount(0);
  expect((await firstEvent()).update.content.text).toBe(prompt);

  // And then inside the turn before the first message loaded.
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('Add backoff to the reconnect logic in sloppod');
  await box.press('Enter');
  await finishTurn(page);
  await page.reload();
  await expect(agentMessages(page).filter({ hasText: FINAL })).toBeVisible();
  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(0);
  expect((await firstEvent()).update.content.text).toBe(prompt);
});

test('with a pointer, the message actions take no room under a message', async ({ page }) => {
  await createSession(page, 'Add backoff to the reconnect logic in sloppod');
  await finishTurn(page);
  await page.mouse.move(0, 0);
  // A message ends where its text ends: the actions overlay its last line instead of adding a row under it.
  for (const message of await agentMessages(page).all()) {
    const gap = await message.evaluate((node) => {
      const blocks = node.querySelectorAll('p, ul, ol, pre');
      return node.getBoundingClientRect().bottom - blocks[blocks.length - 1].getBoundingClientRect().bottom;
    });
    expect(gap).toBeLessThanOrEqual(1);
  }
  // Keyboard users get them too: focus reveals them.
  const message = agentMessages(page).filter({ hasText: FINAL });
  await message.getByRole('button', { name: 'Copy message' }).focus();
  await expect(message.getByTestId('message-actions')).toHaveCSS('opacity', '1');
});

test('touch screens always show the copy button', async ({ page, browser }) => {
  await createSession(page, 'Add backoff to the reconnect logic in sloppod');
  await finishTurn(page);

  const touch = await browser.newContext({ hasTouch: true, isMobile: true, viewport: { width: 412, height: 915 } });
  const phone = await touch.newPage();
  await phone.goto(page.url());
  await expect(agentMessages(phone).filter({ hasText: FINAL }).getByTestId('message-actions')).toHaveCSS('opacity', '1');
  await touch.close();
});

test('answers a permission request', async ({ page }) => {
  await createSession(page, 'Answer my permission request in sloppod');

  const card = permissionCard(page);
  await expect(card).toBeVisible();
  await expect(card).toContainText('Edit ui/src/lib/acp/connection.ts');
  await expect(card.getByTestId('diff')).toContainText('MAX_BACKOFF_MS');
  await expect(phase(page)).toHaveAttribute('data-phase', 'waiting');
  // The call asking for permission is out in the open, not in a fold.
  await expect(page.getByTestId('tool-call').filter({ hasText: 'Edit ui/src/lib/acp/connection.ts' })).toBeVisible();

  await card.getByRole('button', { name: 'Allow', exact: true }).click();
  await expect(card).toBeHidden();
  await expect(agentMessages(page).filter({ hasText: FINAL })).toBeVisible();

  // The calls of a run are folded together; open them to see each.
  await expandFolds(page);
  await expect(page.getByTestId('tool-call').filter({ hasText: 'Edit ui/src/lib/acp/connection.ts' })).toHaveAttribute(
    'data-status',
    'completed',
  );

  // The terminal-style output of the test run shows once expanded.
  const testRun = page.getByTestId('tool-call').filter({ hasText: 'npm test -- reconnect' });
  await expect(testRun).toHaveAttribute('data-status', 'completed');
  await testRun.locator('summary').click();
  await expect(testRun.getByTestId('tool-output')).toContainText('3 passed');
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

/** A 1×1 PNG. */
const PIXEL = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==';

/** Hands the composer an image the way the browser would for a paste or a drop. */
async function handImage(page: Page, kind: 'paste' | 'drop', name: string) {
  await page.evaluate(
    ([kind, name, pixel]) => {
      const bytes = Uint8Array.from(atob(pixel), (c) => c.charCodeAt(0));
      const data = new DataTransfer();
      data.items.add(new File([bytes], name, { type: 'image/png' }));
      const box = document.querySelector('textarea[aria-label="Prompt"]')!;
      const event =
        kind === 'paste'
          ? new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true })
          : new DragEvent('drop', { dataTransfer: data, bubbles: true, cancelable: true });
      box.dispatchEvent(event);
    },
    [kind, name, PIXEL] as const,
  );
}

test('sends images picked, pasted or dropped into the message box', async ({ page }) => {
  await createSession(page, 'Look at my screenshots in sloppod');
  await finishTurn(page);
  const attachments = page.getByTestId('attachment');

  await page.getByTestId('image-picker').setInputFiles({ name: 'a.png', mimeType: 'image/png', buffer: Buffer.from(PIXEL, 'base64') });
  await expect(attachments).toHaveCount(1);
  await handImage(page, 'paste', 'b.png');
  await expect(attachments).toHaveCount(2);
  await handImage(page, 'drop', 'c.png');
  await expect(attachments).toHaveCount(3);
  await page.getByRole('button', { name: 'Remove image 2' }).click();
  await expect(attachments).toHaveCount(2);

  // An image alone can be sent; with text, both go.
  await page.getByRole('textbox', { name: 'Prompt' }).fill('What do these show?');
  await page.getByRole('button', { name: 'Send' }).click();
  await expect(attachments).toHaveCount(0);
  const sent = userMessages(page).filter({ hasText: 'What do these show?' });
  await expect(sent.locator('img')).toHaveCount(2);
  await expect(sent.locator('img').first()).toHaveAttribute('src', `data:image/png;base64,${PIXEL}`);

  // And they're there after a reload, from the session's history.
  await page.reload();
  await expect(userMessages(page).filter({ hasText: 'What do these show?' }).locator('img')).toHaveCount(2);
});

test('an image removed while another is being prepared stays removed', async ({ page }) => {
  // Holds preparing an image until the test lets it go.
  await page.addInitScript(() => {
    const real = window.createImageBitmap.bind(window);
    const w = window as unknown as { holdImages: boolean; releaseImages: () => void };
    let release: () => void = () => {};
    let held = new Promise<void>((resolve) => (release = resolve));
    w.holdImages = false;
    w.releaseImages = () => release();
    window.createImageBitmap = (async (...args: Parameters<typeof createImageBitmap>) => {
      if (w.holdImages) await held;
      return real(...args);
    }) as typeof createImageBitmap;
  });
  await createSession(page, 'Look at my screenshots in sloppod');
  await finishTurn(page);
  const attachments = page.getByTestId('attachment');
  await page.getByTestId('image-picker').setInputFiles({ name: 'a.png', mimeType: 'image/png', buffer: Buffer.from(PIXEL, 'base64') });
  await expect(attachments).toHaveCount(1);

  await page.evaluate(() => ((window as unknown as { holdImages: boolean }).holdImages = true));
  await handImage(page, 'paste', 'b.png');
  await expect(page.getByText('Adding…')).toBeVisible();
  await page.getByRole('button', { name: 'Remove image 1' }).click();
  await expect(attachments).toHaveCount(0);
  await page.evaluate(() => (window as unknown as { releaseImages: () => void }).releaseImages());
  await expect(page.getByText('Adding…')).toBeHidden();
  await expect(attachments).toHaveCount(1);
});

test('a file that is not an image is not attached', async ({ page }) => {
  await createSession(page, 'Look at my files in sloppod');
  await finishTurn(page);
  await page.getByTestId('image-picker').setInputFiles({ name: 'notes.txt', mimeType: 'text/plain', buffer: Buffer.from('hi') });
  await expect(page.getByText("notes.txt isn't an image")).toBeVisible();
  await expect(page.getByTestId('attachment')).toHaveCount(0);
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

test('takes queued messages back to edit or remove', async ({ page }) => {
  await createSession(page, 'Edit my queue in sloppod');
  await expect(permissionCard(page)).toBeVisible();

  const box = page.getByRole('textbox', { name: 'Prompt' });
  for (const message of ['Drop this one', 'Fix this one']) {
    await box.fill(message);
    await page.getByRole('button', { name: 'Send' }).click();
    await expect(page.getByTestId('queued-message').filter({ hasText: message })).toBeVisible();
  }

  // Up arrow in the empty box takes the last one back for editing.
  await box.press('ArrowUp');
  await expect(box).toHaveValue('Fix this one');
  await expect(page.getByTestId('queued-message')).toHaveCount(1);
  await page.getByTestId('queued-message').getByRole('button', { name: 'Remove' }).click();
  await expect(page.getByTestId('queued-messages')).toBeHidden();

  await box.fill('Fixed this one');
  await page.getByRole('button', { name: 'Send' }).click();
  await permissionCard(page).getByRole('button', { name: 'Allow', exact: true }).click();
  await expect(userMessages(page).filter({ hasText: 'Fixed this one' })).toHaveCount(1);
  await expect(userMessages(page).filter({ hasText: /Drop this one|Fix this one$/ })).toHaveCount(0);
});

test('a message taken back is not sent again when the connection drops before the answer', async ({ page }) => {
  await createSession(page, 'Edit my queue in sloppod');
  await expect(permissionCard(page)).toBeVisible();
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('Take me back');
  await page.getByRole('button', { name: 'Send' }).click();
  const queued = page.getByTestId('queued-message').filter({ hasText: 'Take me back' });
  await expect(queued).toBeVisible();

  await page.request.post('/_mock/drop-next-remove');
  await queued.getByRole('button', { name: 'Edit' }).click();
  await expect(box).toHaveValue('Take me back');
  await expect(page.getByTestId('reconnecting')).toBeHidden();
  await expect(page.getByTestId('queued-messages')).toBeHidden();

  await permissionCard(page).getByRole('button', { name: 'Allow', exact: true }).click();
  await expect(agentMessages(page).filter({ hasText: FINAL })).toBeVisible();
  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');
  await expect(userMessages(page).filter({ hasText: 'Take me back' })).toHaveCount(0);
});

test('a queued message with more than text can be removed but not edited', async ({ page }) => {
  const sessionId = await createSession(page, 'Edit my queue in sloppod');
  await expect(permissionCard(page)).toBeVisible();
  await page.request.post(`/_mock/queue-image?session=${sessionId}`);
  const queued = page.getByTestId('queued-message');
  await expect(queued).toHaveCount(1);
  await expect(queued.getByRole('button', { name: 'Edit' })).toHaveCount(0);
  await page.getByRole('textbox', { name: 'Prompt' }).press('ArrowUp');
  await expect(queued).toHaveCount(1);
  await queued.getByRole('button', { name: 'Remove' }).click();
  await expect(page.getByTestId('queued-messages')).toBeHidden();
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


test('a stopped turn resumed while reconnecting carries on once', async ({ page }) => {
  await createSession(page, 'Stop me in sloppod');
  await expect(permissionCard(page)).toBeVisible();
  await page.getByRole('button', { name: 'Stop' }).click();
  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');

  await page.request.post('/_mock/drop?for=8000');
  await expect(page.getByTestId('composer-hint')).toHaveText("Reconnecting… messages send once it's back.");
  const resume = page.getByTestId('resume-turn');
  await resume.click();
  await expect(page.getByTestId('unsent-message')).toContainText('Carry on where you left off.');
  // Waiting to be sent counts as sent: there's no second Resume to click meanwhile.
  await expect(resume).toHaveCount(0, { timeout: 1000 });
  // After an 8 s outage the reconnect's backoff can take a while.
  await expect(userMessages(page).filter({ hasText: 'Carry on where you left off.' })).toHaveCount(1, { timeout: 30_000 });
  await expect(resume).toHaveCount(0);
});

test('a stopped turn is resumed with one click', async ({ page }) => {
  await createSession(page, 'Stop me in sloppod');
  await expect(permissionCard(page)).toBeVisible();
  await page.getByRole('button', { name: 'Stop' }).click();
  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');

  // The stop is marked, with a way on.
  const resume = page.getByTestId('resume-turn');
  await expect(page.getByTestId('notice').filter({ hasText: 'Stopped' })).toBeVisible();
  await expect(resume).toBeVisible();
  await resume.click();
  await expect(userMessages(page).filter({ hasText: 'Carry on where you left off.' })).toBeVisible();
  // Once anything was sent after it, the marker has done its job.
  await expect(resume).toHaveCount(0);
  await expect(page.getByTestId('notice').filter({ hasText: 'Stopped' })).toBeVisible();
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

test('a proposal made while the session view is loading shows without a reload', async ({ page }) => {
  await createSession(page, 'Give me a long answer');
  await expect(agentMessages(page).filter({ hasText: 'The end.' })).toBeVisible();
  await expect(page.getByTestId('proposal-card')).toBeHidden();

  // The proposal is made after the reloaded page's REST fetch and before its socket subscribes (#423).
  await page.request.post('/_mock/propose-on-load');
  await page.reload();
  await expect(page.getByTestId('proposal-card')).toBeVisible();
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

test('Esc in the review modal undoes one step, and unsaved edits are asked about', async ({ page }) => {
  await createSession(page, 'Propose a PR for sloppod');
  await finishTurn(page);
  const card = page.getByTestId('proposal-card');
  const detail = page.getByTestId('proposal-detail');
  const confirm = page.getByRole('dialog').filter({ hasText: 'Discard your edits?' });
  await card.getByRole('button', { name: 'Review' }).click();
  await expect(detail.getByTestId('proposal-title')).toBeVisible();

  // Reject… focuses the reason; Esc goes back to the decision buttons, not out of the modal.
  await page.getByRole('button', { name: 'Reject…' }).click();
  const reason = page.getByRole('textbox', { name: 'Reason (optional)' });
  await expect(reason).toBeFocused();
  await page.keyboard.press('Escape');
  await expect(reason).toBeHidden();
  await expect(page.getByRole('button', { name: 'Reject…' })).toBeFocused();
  await expect(detail).toBeVisible();

  // With edits, Esc asks before leaving editing; keeping them stays put.
  await page.getByRole('button', { name: 'Edit' }).click();
  const title = page.getByRole('textbox', { name: 'Title' });
  await title.fill('Back off when reconnecting');
  await page.keyboard.press('Escape');
  await expect(confirm).toBeVisible();
  await confirm.getByRole('button', { name: 'Cancel' }).click();
  await expect(title).toHaveValue('Back off when reconnecting');

  // ✕ asks too.
  await page.getByRole('dialog').filter({ has: detail }).getByRole('button', { name: 'Close' }).first().click();
  await expect(confirm).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(confirm).toBeHidden();
  await expect(title).toHaveValue('Back off when reconnecting');

  // Discarding leaves editing; then Esc closes the modal.
  await page.keyboard.press('Escape');
  await confirm.getByRole('button', { name: 'Discard edits' }).click();
  await expect(title).toBeHidden();
  await expect(detail.getByTestId('proposal-title')).toHaveText('Reconnect the ACP socket with exponential backoff');
  await page.keyboard.press('Escape');
  await expect(detail).toBeHidden();

  // Without edits, editing is left and the modal closed without asking.
  await card.getByRole('button', { name: 'Review' }).click();
  await page.getByRole('button', { name: 'Edit' }).click();
  await page.keyboard.press('Escape');
  await expect(page.getByRole('textbox', { name: 'Title' })).toBeHidden();
  await page.keyboard.press('Escape');
  await expect(detail).toBeHidden();
  await expect(confirm).toHaveCount(0);
});

test('a proposal made with the user\'s own GitHub token says so', async ({ page }) => {
  // As the runtime reports a proposal on a repository the session acts on with the user's token (ADR 29).
  await page.route('**/api/v1/proposals?*', async (route) => {
    const response = await route.fetch();
    const proposals: Proposal[] = await response.json();
    await route.fulfill({ response, json: proposals.map((p) => ({ ...p, payload: { ...p.payload, as: 'bo0tzz' } })) });
  });
  await createSession(page, 'Propose a PR for sloppod');
  await finishTurn(page);
  await page.reload();

  const card = page.getByTestId('proposal-card');
  await expect(card.getByTestId('proposal-acts-as')).toHaveText('as bo0tzz');
  await card.getByRole('button', { name: 'Review' }).click();
  await expect(page.getByTestId('proposal-detail').getByTestId('proposal-acts-as')).toHaveText('Acts as bo0tzz');
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

test("a proposal shows at once, and only its changes wait for the detail", async ({ page }) => {
  await createSession(page, 'Add backoff to the reconnect logic in sloppod');
  await finishTurn(page);
  await page.request.post('/_mock/slow-proposals?ms=2500');
  try {
    await page.getByTestId('proposal-card').getByRole('button', { name: 'Review' }).click();
    const dialog = page.getByRole('dialog');
    // The title and description from the list, straight away, with no spinner over them.
    await expect(dialog.getByTestId('proposal-title')).toHaveText('Reconnect the ACP socket with exponential backoff', {
      timeout: 1000,
    });
    await expect(dialog.getByTestId('proposal-status')).toBeVisible({ timeout: 1000 });
    // Only where the changes go says they're coming, and approving waits for them.
    await expect(dialog.getByTestId('proposal-changes-loading')).toBeVisible();
    await expect(dialog.getByRole('button', { name: 'Approve' })).toBeDisabled();
    await expect(dialog.getByTestId('changes')).toBeVisible({ timeout: 10_000 });
    await expect(dialog.getByTestId('proposal-changes-loading')).toHaveCount(0);
    await expect(dialog.getByRole('button', { name: 'Approve' })).toBeEnabled();
  } finally {
    await page.request.post('/_mock/slow-proposals?ms=0');
  }
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
  // Every load fails until the test lets them through: the session loads more than once (#423), and the error has to
  // stay up for as long as the test looks at it.
  let failing = true;
  await page.route(`**/api/v1/sessions/${session.id}`, (route) =>
    failing ? route.fulfill({ status: 500, json: { error: 'database unavailable' } }) : route.fallback(),
  );
  await page.goto(`/sessions/${session.id}`);
  await expect(page.getByTestId('load-error')).toContainText('database unavailable');
  failing = false;
  await expect(page.getByTestId('load-error')).toBeHidden();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Park and wake');
});

test('a load that fails after a newer one succeeded shows no error', async ({ page }) => {
  const sessions: { id: string; title: string }[] = await (await page.request.get('/api/v1/sessions')).json();
  const session = sessions.find((candidate) => candidate.title === 'Park and wake')!;
  // The first load hangs until the session has loaded by other means (the refresh after `session/load`), then fails.
  let calls = 0;
  let release!: () => void;
  const held = new Promise<void>((resolve) => (release = resolve));
  let failed!: () => void;
  const answered = new Promise<void>((resolve) => (failed = resolve));
  await page.route(`**/api/v1/sessions/${session.id}`, async (route) => {
    if (calls++ > 0) return route.fallback();
    await held;
    await route.fulfill({ status: 500, json: { error: 'database unavailable' } });
    failed();
  });
  await page.goto(`/sessions/${session.id}`);
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Park and wake');
  await expect.poll(() => calls).toBeGreaterThan(1);
  release();
  await answered;
  // Long enough for the failure to have been handled; unhandled, its error would show for about a second.
  await page.evaluate(() => new Promise((resolve) => setTimeout(resolve, 300)));
  // Not retried: the error does go away by itself, after its retry.
  expect(await page.getByTestId('load-error').count()).toBe(0);
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

test('the right sidebar stays up beside the chat, and folds away', async ({ page }) => {
  await page.setViewportSize({ width: 1920, height: 1000 });
  const id = await createSession(page, 'Give me a long answer in sloppod');
  const panel = page.getByTestId('side-panel');
  // Open with nothing waiting, so the chat doesn't move when something does.
  await expect(panel).toBeVisible();
  await expect(panel.getByRole('heading', { name: 'Nothing waiting on you' })).toBeVisible();
  await expect(panel.getByTestId('work-done-empty')).toBeVisible();
  const chat = page.getByTestId('transcript-scroller');
  const width = (await chat.boundingBox())!.width;
  await page.request.post(`/_mock/highlight?session=${id}&text=Look%20here`);
  await expect(panel.getByTestId('highlight-item')).toBeVisible();
  expect((await chat.boundingBox())!.width).toBe(width);

  await panel.getByRole('button', { name: 'Close the sidebar' }).click();
  await expect(panel).toBeHidden();
  // What waits shows under the chat meanwhile.
  await expect(page.getByTestId('waiting-panel')).toBeVisible();
  const toggle = page.getByRole('button', { name: 'Show details' });
  await expect(toggle).toHaveAttribute('aria-expanded', 'false');
  await toggle.click();
  await expect(panel).toBeVisible();
  await expect(page.getByTestId('waiting-panel')).toBeHidden();
  await expect(page.getByRole('button', { name: 'Hide details' })).toHaveAttribute('aria-expanded', 'true');
});

test('on a narrow screen the sidebar is folded away, and opens over the chat', async ({ page }) => {
  await page.setViewportSize({ width: 600, height: 900 });
  await createSession(page, 'Give me a long answer in sloppod');
  const panel = page.getByTestId('side-panel');
  await expect(panel).toBeHidden();
  await page.getByRole('button', { name: 'Show details' }).click();
  await expect(panel).toBeVisible();
  await expect(panel.getByTestId('work-done-empty')).toBeVisible();
  await panel.getByRole('button', { name: 'Close the sidebar' }).click();
  await expect(panel).toBeHidden();
});

test('lists the pull requests and issues a session acted on, with where a pull request stands', async ({ page }) => {
  await page.setViewportSize({ width: 1920, height: 1000 });
  const id = await createSession(page, 'Propose a PR for sloppod');
  await finishTurn(page);
  const panel = page.getByTestId('side-panel');
  await expect(panel.getByTestId('work-done-empty')).toBeVisible();
  await panel.getByTestId('proposal-card').getByRole('button', { name: 'Review' }).click();
  await page.getByRole('button', { name: 'Approve', exact: true }).click();

  const item = panel.getByTestId('work-done-item');
  await expect(item).toHaveCount(1);
  await expect(item.getByTestId('work-done-title')).toHaveText('Reconnect the ACP socket with exponential backoff');
  await expect(item.getByTestId('work-done-did')).toHaveText('opened');
  await expect(item.getByRole('link')).toHaveAttribute('href', /github\.com\/bo0tzz\/sloppod\/pull\/\d+$/);
  await expect(item.getByTestId('work-done-state')).toBeHidden();

  // What the runtime has seen of the pull request is there after a reload.
  const proposals: Proposal[] = await (await page.request.get(`/api/v1/proposals?session=${id}`)).json();
  const { number } = proposals.find((p) => p.kind === 'github.pr.create')!.result as { number: number };
  await page.request.post(`/_mock/pull?repo=bo0tzz/sloppod&number=${number}&state=open&checks=failure&failing=check,lint`);
  await page.reload();
  await expect(item.getByTestId('work-done-state')).toHaveText('Open');
  await expect(item.getByTestId('work-done-checks')).toHaveText('checks failing: check, lint');
});

test('an older answer about the work done does not replace a newer one', async ({ page }) => {
  await page.setViewportSize({ width: 1920, height: 1000 });
  const id = await createSession(page, 'Propose a PR for sloppod');
  await finishTurn(page);
  const proposals: Proposal[] = await (await page.request.get(`/api/v1/proposals?session=${id}`)).json();
  const pending = proposals.find((p) => p.kind === 'github.pr.create')!;

  // The load's first request is answered with the history as it was, but only once the proposal has been carried out.
  let requests = 0;
  let release!: () => void;
  const held = new Promise<void>((resolve) => (release = resolve));
  await page.route('**/api/v1/sessions/*/work-done', async (route) => {
    const request = ++requests;
    const response = await route.fetch();
    if (request === 1) await held;
    await route.fulfill({ response });
  });
  await page.reload();
  // The load fetches twice (before and after `session/load`); the first answer is the one that's held.
  await expect.poll(() => requests).toBeGreaterThanOrEqual(2);
  await page.request.post(`/api/v1/proposals/${pending.id}/approve`, { data: {} });
  const panel = page.getByTestId('side-panel');
  await expect(panel.getByTestId('work-done-item')).toHaveCount(1);

  release();
  await page.waitForTimeout(500);
  await expect(panel.getByTestId('work-done-item')).toHaveCount(1);
});

test('files issues, which the work done lists with what was done to them', async ({ page }) => {
  await page.setViewportSize({ width: 1920, height: 1000 });
  await createSession(page, 'File issues for sloppod');
  const panel = page.getByTestId('side-panel');
  await panel.getByTestId('proposal-card').getByRole('button', { name: 'Review' }).click();
  await page.getByRole('button', { name: /^File \d+ issues$/ }).click();
  const items = panel.getByTestId('work-done-item');
  await expect(items).toHaveCount(3);
  await expect(items.first().getByTestId('work-done-did')).toHaveText('filed');
  await expect(items.first().getByRole('link')).toHaveAttribute('href', /github\.com\/bo0tzz\/sloppod\/issues\/\d+$/);
});

async function expectTranscriptOnce(page: Page, prompt: string) {
  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(1);
  await expect(agentMessages(page).filter({ hasText: FINAL })).toHaveCount(1);
  await expect(agentMessages(page).filter({ hasText: "I'll start by reading" })).toHaveCount(1);
  await expect.poll(() => toolCallsInTranscript(page)).toBe(5);
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

test('a dropped connection leaves the state unknown, with a way to retry at once', async ({ page }) => {
  await page.setViewportSize({ width: 1400, height: 900 });
  const sockets: PageSocket[] = [];
  page.on('websocket', (socket) => sockets.push(socket));
  const prompt = 'Say something, the connection will drop';
  await createSession(page, prompt);
  await finishTurn(page);
  const row = page.getByRole('navigation', { name: 'Sessions' }).getByTestId('session-row').filter({ hasText: prompt });
  await expect(phase(page)).toHaveText('Idle');
  await expect(row.getByTestId('phase')).toHaveText('Idle');

  // Down for a while: neither the header nor the sidebar says Idle meanwhile.
  const attemptsBefore = sockets.length;
  await page.request.post('/_mock/drop?for=8000');
  const banner = page.getByTestId('reconnecting');
  await expect(banner).toContainText('Connection lost, reconnecting');
  await expect(phase(page)).toHaveText('Unknown');
  await expect(row.getByTestId('phase')).toHaveText('Unknown');
  await expect(banner.getByRole('button', { name: 'Retry now' })).toBeVisible();

  // The automatic attempts back off (0.5, 1, 2 s…): once three have been refused the next is more than two seconds
  // off. Retrying starts one at once, instead of waiting that out.
  await expect.poll(() => sockets.length).toBeGreaterThanOrEqual(attemptsBefore + 3);
  const attempts = sockets.length;
  await banner.getByRole('button', { name: 'Retry now' }).click();
  await expect.poll(() => sockets.length, { timeout: 1000 }).toBeGreaterThan(attempts);
  // Once the runtime takes a connection, the states are back.
  await expect(banner).toBeHidden({ timeout: 20_000 });
  await expect(phase(page)).toHaveText('Idle');
  await expect(row.getByTestId('phase')).toHaveText('Idle');
});

test('Retry now also tries a session whose load failed on a connection that is up', async ({ page }) => {
  await page.setViewportSize({ width: 1400, height: 900 });
  const prompt = 'Say something, its load will fail once reconnected';
  await createSession(page, prompt);
  await finishTurn(page);

  const loads: number[] = [];
  page.on('websocket', (socket) =>
    socket.on('framesent', ({ payload }) => {
      if (JSON.parse(String(payload)).method === 'session/load') loads.push(Date.now());
    }),
  );
  // The reconnect comes back up, but the runtime can't read the session: the load fails, and retries back off (1 s, 2 s…).
  await page.request.post('/_mock/fail-loads?for=60000');
  await page.request.post('/_mock/drop');
  const banner = page.getByTestId('reconnecting');
  await expect(banner).toBeVisible();
  await expect.poll(() => loads.length).toBeGreaterThanOrEqual(2);

  // The runtime can read it again, and the next automatic try is about two seconds off: Retry now doesn't wait for it.
  await page.request.post('/_mock/fail-loads?for=0');
  const before = loads.length;
  await banner.getByRole('button', { name: 'Retry now' }).click();
  await expect.poll(() => loads.length, { timeout: 800 }).toBeGreaterThan(before);
  await expect(banner).toBeHidden({ timeout: 3000 });
  await expect(phase(page)).toHaveText('Idle');
});

test('a stopped session says that a message does not start it again', async ({ page }) => {
  const sessions: { id: string; title: string }[] = await (await page.request.get('/api/v1/sessions')).json();
  const stopped = sessions.find((candidate) => candidate.title === 'Summarise the namespace resolution rules')!;
  await page.goto(`/sessions/${stopped.id}`);
  await expect(phase(page)).toHaveAttribute('data-phase', 'stopped');
  await expect(page.getByTestId('composer-hint')).toContainText('does not start it again');
  await expect(page.getByTestId('composer-hint')).toContainText('Start a new session');
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

test("an endpoint added in settings offers its models wherever a model is chosen", async ({ page }) => {
  await page.goto('/settings');
  const endpoints = page.getByTestId('credentials-endpoint');
  await endpoints.getByRole('button', { name: 'Add endpoint' }).click();
  await endpoints.getByLabel('Name', { exact: true }).fill('litellm');
  await endpoints.getByLabel('Base URL').fill('http://litellm.ai.svc.cluster.local:4000/');
  await endpoints.getByLabel('Models').fill('qwen3-coder, glm-5');
  await endpoints.getByLabel('Key', { exact: true }).fill('sk-litellm');
  await endpoints.getByRole('button', { name: 'Save' }).click();
  await expect(endpoints.getByTestId('credential')).toContainText('litellm');
  try {
    // The default model and the one sub-sessions run on can be one of its models.
    const defaults = page.getByTestId('session-defaults');
    await page.reload();
    await defaults.getByRole('combobox', { name: 'Model', exact: true }).click();
    await expect(page.getByRole('option', { name: 'glm-5 (litellm)' })).toBeVisible();
    await page.keyboard.press('Escape');
    await defaults.getByRole('combobox', { name: 'Sub-sessions' }).click();
    await expect(page.getByRole('option', { name: 'qwen3-coder (litellm)' })).toBeVisible();
    await page.keyboard.press('Escape');

    // A running session can be moved onto one, and back; a move applies at the next message.
    const sent: string[] = [];
    page.on('websocket', (socket) => socket.on('framesent', ({ payload }) => sent.push(String(payload))));
    await createSession(page, 'Try another model in sloppod');
    const select = page.getByTestId('model-select');
    await select.getByRole('combobox').click();
    await page.getByRole('option', { name: 'qwen3-coder (litellm)' }).click();
    await expect(select).toContainText('qwen3-coder (litellm)');
    await expect(page.getByTestId('model-pending')).toHaveText('Applies to your next message');
    expect(sent.some((frame) => frame.includes('"session/set_config_option"') && frame.includes('endpoint:'))).toBe(true);
    await page.reload();
    await expect(page.getByTestId('model-select')).toContainText('qwen3-coder (litellm)');
    await expect(page.getByTestId('model-pending')).toBeVisible();
    await page.getByRole('textbox', { name: 'Prompt' }).fill('Now on qwen');
    await page.keyboard.press('Enter');
    await expect(page.getByTestId('model-pending')).toHaveCount(0);
    await expect(page.getByTestId('model-select')).toContainText('qwen3-coder (litellm)');

    // On it, the choices are Claude's models and the endpoints', not Claude's aliases with the endpoint's model behind.
    await page.getByTestId('model-select').getByRole('combobox').click();
    await expect(page.getByRole('option', { name: 'qwen3-coder (opus)' })).toHaveCount(0);
    await expect(page.getByRole('option', { name: 'glm-5 (litellm)' })).toBeVisible();
    await page.getByRole('option', { name: 'Opus' }).click();
    await expect(page.getByTestId('model-select')).toContainText('Opus');
    await expect(page.getByTestId('model-pending')).toBeVisible();
  } finally {
    const listed: { id: string; kind: string }[] = await (await page.request.get('/api/v1/credentials')).json();
    for (const { id } of listed.filter((c) => c.kind === 'endpoint')) await page.request.delete(`/api/v1/credentials/${id}`);
  }
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

test('follows new output while the reader is at the bottom, across a reconnect', { tag: '@scroll' }, async ({ page }) => {
  showChatLog(page);
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
  // Up this far in a long session, the page before comes in by itself, and what it puts above the reader moves the
  // scroll for a few frames as it settles: both are waited for, so what's measured below is the new output's doing.
  await expect(page.getByTestId('history-start')).toBeVisible();
  const kept = await scroller.evaluate(
    (element) =>
      new Promise<number>((resolve) => {
        let last = element.scrollTop;
        let still = 0;
        const frame = () => {
          still = element.scrollTop === last ? still + 1 : 0;
          last = element.scrollTop;
          if (still >= 6) return resolve(last);
          requestAnimationFrame(frame);
        };
        requestAnimationFrame(frame);
      }),
  );
  await page.request.post('/_mock/say?text=One%20more%20thing.');
  await expect(page.getByText('One more thing.')).toBeAttached();
  expect(await scrollTop()).toBe(kept);

  // So does dragging the scrollbar, however long the drag is held before it moves.
  await scroller.evaluate((element) => element.scrollTo({ top: element.scrollHeight }));
  // The view has noticed it's at the bottom before more output arrives.
  await expect(page.getByRole('button', { name: 'Scroll to the latest message' })).toBeHidden();
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

/** Waits for the runtime to have the session's read position beyond `seq`, as the UI reports it a moment after reading. */
async function readPosition(page: Page, id: string, above = 0) {
  let seq = 0;
  await expect
    .poll(
      async () => {
        const session = await (await page.request.get(`/api/v1/sessions/${id}`)).json();
        seq = session.read_seq ?? 0;
        return seq;
      },
      { timeout: 15_000 },
    )
    .toBeGreaterThan(above);
  return seq;
}

const LONG_SAY = Array.from({ length: 12 }, (_, line) => `line ${line}`).join('%0A%0A');

test('marks where the reader left off, and opens there, on this device or another', { tag: '@scroll' }, async ({ page, context }) => {
  await page.setViewportSize({ width: 1000, height: 500 });
  const id = await createSession(page, 'Give me a long answer in sloppod');
  await expect(page.getByText('The end.')).toBeInViewport({ timeout: 20_000 });
  const divider = page.getByTestId('unread-divider');
  // Everything is read at the bottom: no bar, and the position is kept.
  await expect(divider).toHaveCount(0);
  const read = await readPosition(page, id);

  // Scrolled up, new output arrives under the bar.
  await page.getByText('The end.').hover();
  await page.mouse.wheel(0, -2000);
  await expect(page.getByText('The end.')).not.toBeInViewport();
  await page.request.post(`/_mock/say?text=Unread%20first.%0A%0A${LONG_SAY}`);
  await page.request.post(`/_mock/say?text=Unread%20last.%0A%0A${LONG_SAY}`);
  await expect(page.getByText('Unread last.')).toBeAttached();
  await expect(divider).toHaveCount(1);
  const dividerTop = (await divider.boundingBox())!.y;
  const firstTop = (await page.getByText('Unread first.').boundingBox())!.y;
  expect(dividerTop).toBeLessThan(firstTop);
  // Nothing past what was read has been reported.
  expect(await readPosition(page, id)).toBe(read);

  // A reload starts at the bar, with what comes before it in view too, not at the bottom.
  await page.reload();
  await expect(divider).toBeInViewport();
  await expect(page.getByText('Unread first.')).toBeInViewport();
  await expect(page.getByText('Unread last.')).not.toBeInViewport();
  await expect(page.getByText('The end.')).toBeInViewport();

  // So does another device: the position is the user's, not the browser's.
  const phone = await context.newPage();
  await phone.setViewportSize({ width: 500, height: 500 });
  await phone.goto(page.url());
  await expect(phone.getByTestId('unread-divider')).toBeInViewport();
  await expect(phone.getByText('Unread last.')).not.toBeInViewport();

  // Reading to the end takes the bar away, and moves the position on for both.
  await page.getByText('Unread first.').hover();
  // A wheel event scrolls a page at most in some browsers (Firefox), so it takes as many as it takes to reach the very
  // end: the bar goes within a few lines of it, but the last message is only read once it's all in view.
  const scroller = page.getByTestId('transcript-scroller');
  await expect(async () => {
    await page.mouse.wheel(0, 5000);
    const gap = await scroller.evaluate((element) => element.scrollHeight - element.scrollTop - element.clientHeight);
    expect(gap).toBeLessThan(2);
  }).toPass();
  await expect(divider).toHaveCount(0);
  const moved = await readPosition(page, id, read);
  await phone.reload();
  await expect(phone.getByText('line 11', { exact: true }).last()).toBeInViewport();
  await expect(phone.getByTestId('unread-divider')).toHaveCount(0);
  expect(await readPosition(phone, id)).toBe(moved);
  await phone.close();
});

const plan = (status: string) => ({
  sessionUpdate: 'plan',
  entries: [{ content: 'Check the unread marker', priority: 'medium', status }],
});

test('an update to a row already read does not mark the rows after it read', { tag: '@scroll' }, async ({ page }) => {
  await page.setViewportSize({ width: 1000, height: 500 });
  const id = await createSession(page, 'Give me a long answer in sloppod');
  await expect(page.getByText('The end.')).toBeInViewport({ timeout: 20_000 });
  const read = await readPosition(page, id);

  // A plan and a long message come in while scrolled up; the reader scrolls to the plan, which is then updated: the
  // update falls after the message in the log, though the message isn't in view.
  await page.getByText('The end.').hover();
  await page.mouse.wheel(0, -2000);
  await expect(page.getByText('The end.')).not.toBeInViewport();
  await page.request.post('/_mock/update', { data: plan('pending') });
  await page.request.post(`/_mock/say?text=Below%20the%20plan.%0A%0A${LONG_SAY}`);
  await expect(page.getByText('Below the plan.')).toBeAttached();
  // Scroll to the message first, so it's laid out at its size, then back up to the plan above it.
  await page.getByText('Below the plan.').scrollIntoViewIfNeeded();
  await page.getByTestId('plan').scrollIntoViewIfNeeded();
  await expect(page.getByTestId('plan')).toBeInViewport({ ratio: 1 });
  await expect(page.getByText('line 11', { exact: true }).last()).not.toBeInViewport();
  await page.request.post('/_mock/update', { data: plan('completed') });
  await readPosition(page, id, read);

  // The message is still unread, wherever the plan's update falls.
  await page.reload();
  await expect(page.getByTestId('unread-divider')).toBeAttached();
  const divider = (await page.getByTestId('unread-divider').boundingBox())!.y;
  expect(divider).toBeLessThan((await page.getByText('Below the plan.').boundingBox())!.y);
});

const messageChunk = (text: string) => ({
  sessionUpdate: 'agent_message_chunk',
  messageId: 'grows',
  content: { type: 'text', text },
});
const readCall = (toolCallId: string) => ({
  sessionUpdate: 'tool_call',
  toolCallId,
  title: `Read ${toolCallId}.md`,
  kind: 'read',
  status: 'completed',
});

test('more of a message that was read is unread', { tag: '@scroll' }, async ({ page }) => {
  await page.setViewportSize({ width: 1000, height: 500 });
  const id = await createSession(page, 'Give me a long answer in sloppod');
  await expect(page.getByText('The end.')).toBeInViewport({ timeout: 20_000 });
  const first = await readPosition(page, id);
  await page.request.post('/_mock/update', { data: messageChunk('The start of it.') });
  await expect(page.getByText('The start of it.')).toBeInViewport();
  await readPosition(page, id, first);

  await page.getByText('The start of it.').hover();
  await page.mouse.wheel(0, -2000);
  await expect(page.getByText('The start of it.')).not.toBeInViewport();
  await page.request.post('/_mock/update', { data: messageChunk(' And the rest.') });
  await expect(page.getByTestId('unread-divider')).toHaveCount(1);

  // Reloaded, the view opens at the message, not at the bottom.
  await page.reload();
  await expect(page.getByTestId('unread-divider')).toBeInViewport();
  await expect(page.getByText('The start of it. And the rest.')).toBeInViewport();
});

test('a tool call added to a run that was read is unread', { tag: '@scroll' }, async ({ page }) => {
  await page.setViewportSize({ width: 1000, height: 500 });
  const id = await createSession(page, 'Give me a long answer in sloppod');
  await expect(page.getByText('The end.')).toBeInViewport({ timeout: 20_000 });
  const first = await readPosition(page, id);
  await page.request.post('/_mock/update', { data: readCall('first') });
  await page.request.post('/_mock/update', { data: readCall('second') });
  await expect(page.getByTestId('tool-fold')).toBeInViewport();
  await readPosition(page, id, first);

  await page.getByTestId('tool-fold').hover();
  await page.mouse.wheel(0, -2000);
  await expect(page.getByTestId('tool-fold')).not.toBeInViewport();
  await page.request.post('/_mock/update', { data: readCall('third') });
  await expect(page.getByTestId('unread-divider')).toHaveCount(1);

  await page.reload();
  await expect(page.getByTestId('unread-divider')).toBeInViewport();
  await expect(page.getByTestId('tool-fold')).toBeInViewport();
});

test('reads what an item grew by in place, at the bottom', { tag: '@scroll' }, async ({ page }) => {
  await page.setViewportSize({ width: 1000, height: 500 });
  const id = await createSession(page, 'Give me a long answer in sloppod');
  await expect(page.getByText('The end.')).toBeInViewport({ timeout: 20_000 });
  const first = await readPosition(page, id);

  // A word more on the line of a short message changes no size and moves no scroll.
  const chunk = (text: string) => ({
    sessionUpdate: 'agent_message_chunk',
    messageId: 'same-height',
    content: { type: 'text', text },
  });
  await page.request.post('/_mock/update', { data: chunk('Hi') });
  await expect(page.getByText('Hi', { exact: true })).toBeInViewport();
  const second = await readPosition(page, id, first);
  await page.request.post('/_mock/update', { data: chunk('!') });
  await expect(page.getByText('Hi!', { exact: true })).toBeInViewport();
  await readPosition(page, id, second);

  await page.reload();
  await expect(page.getByText('Hi!', { exact: true })).toBeInViewport();
  await expect(page.getByTestId('unread-divider')).toHaveCount(0);
});

test('jumping to the latest message from the unread bar goes on following the bottom', { tag: '@scroll' }, async ({ page }) => {
  await page.setViewportSize({ width: 1000, height: 500 });
  const id = await createSession(page, 'Give me a long answer in sloppod');
  await expect(page.getByText('The end.')).toBeInViewport({ timeout: 20_000 });
  const read = await readPosition(page, id);
  await page.getByText('The end.').hover();
  await page.mouse.wheel(0, -2000);
  await expect(page.getByText('The end.')).not.toBeInViewport();
  await page.request.post(`/_mock/say?text=Unread%20one.%0A%0A${LONG_SAY}`);
  await expect(page.getByTestId('unread-divider')).toHaveCount(1);
  expect(await readPosition(page, id)).toBe(read);

  await page.reload();
  await expect(page.getByTestId('unread-divider')).toBeInViewport();
  await page.getByRole('button', { name: 'Scroll to the latest message' }).click();
  await expect(page.getByText('line 11', { exact: true }).last()).toBeInViewport();
  await expect(page.getByTestId('unread-divider')).toHaveCount(0);
  // The view follows the bottom again, and stays there.
  await page.request.post('/_mock/say?text=Followed.');
  await expect(page.getByText('Followed.')).toBeInViewport();
  await readPosition(page, id, read);
});

test('a read position that failed to save is sent again', async ({ page }) => {
  let attempts = 0;
  await page.route('**/api/v1/sessions/*/read', (route) => {
    attempts++;
    return attempts === 1 ? route.fulfill({ status: 500, body: 'unavailable' }) : route.continue();
  });
  await page.setViewportSize({ width: 1000, height: 500 });
  const id = await createSession(page, 'Give me a long answer in sloppod');
  await expect(page.getByText('The end.')).toBeInViewport({ timeout: 20_000 });
  // With nothing more to read, so nothing to prompt another report but the retry itself.
  await readPosition(page, id);
  expect(attempts).toBeGreaterThan(1);
});

test('the end of the chat stays in view as the message box grows and shrinks', { tag: '@scroll' }, async ({ page }) => {
  await page.setViewportSize({ width: 1000, height: 500 });
  await createSession(page, 'Give me a long answer in sloppod');
  const end = page.getByText('The end.');
  await expect(end).toBeInViewport({ ratio: 1, timeout: 20_000 });
  const box = page.getByRole('textbox', { name: 'Prompt' });

  // Following the bottom, the view stays there as the box takes more room, and as it gives it back.
  await box.click();
  for (const line of ['one', 'two', 'three', 'four']) {
    await box.pressSequentially(line);
    await page.keyboard.press('Shift+Enter');
  }
  await expect(end).toBeInViewport({ ratio: 1 });
  await box.fill('one\ntwo\nthree\nfour\nfive\nsix');
  await expect(end).toBeInViewport({ ratio: 1 });
  await box.fill('');
  await expect(end).toBeInViewport({ ratio: 1 });

  // Scrolled up, what the reader was looking at stays where it is.
  const scroller = page.getByTestId('transcript-scroller');
  await end.hover();
  await page.mouse.wheel(0, -300);
  await expect(end).not.toBeInViewport();
  const top = () => scroller.evaluate((element) => element.scrollTop);
  const kept = await top();
  await box.fill('one\ntwo\nthree\nfour');
  await expect.poll(top).toBe(kept);
});

test('a scroll event that comes with the message box growing does not stop the view following the bottom', { tag: '@scroll' }, async ({ page }) => {
  await page.setViewportSize({ width: 1000, height: 500 });
  await createSession(page, 'Give me a long answer in sloppod');
  const end = page.getByText('The end.');
  await expect(end).toBeInViewport({ ratio: 1, timeout: 20_000 });
  // The reader has just wheeled at the bottom, so their scrolling counts as recent, and the browser reports a scroll as
  // the box grows, before the view has been put back, as Firefox does.
  await page.evaluate(() => {
    const box = document.querySelector('textarea')!;
    const scroller = document.querySelector('[data-testid="transcript-scroller"]')!;
    scroller.dispatchEvent(new WheelEvent('wheel', { deltaY: 100 }));
    // The box taking more room, at once, so the view is shorter by the time the scroll is reported.
    box.style.minHeight = '190px';
    void scroller.clientHeight;
    scroller.dispatchEvent(new Event('scroll'));
  });
  await expect(end).toBeInViewport({ ratio: 1 });
  // Still following: new output scrolls into view.
  await page.request.post('/_mock/say?text=More%20output.');
  await expect(page.getByText('More output.')).toBeInViewport({ ratio: 1 });
});

test('opened at the unread bar, the end of the chat stays in view as the message box grows', { tag: '@scroll' }, async ({ page }) => {
  await page.setViewportSize({ width: 1000, height: 500 });
  const id = await createSession(page, 'Give me a long answer in sloppod');
  await expect(page.getByText('The end.')).toBeInViewport({ timeout: 20_000 });
  const read = await readPosition(page, id);
  await page.getByText('The end.').hover();
  await page.mouse.wheel(0, -2000);
  await expect(page.getByText('The end.')).not.toBeInViewport();
  await page.request.post('/_mock/say?text=Just%20one%20line.');
  await expect(page.getByTestId('unread-divider')).toHaveCount(1);
  expect(await readPosition(page, id)).toBe(read);

  await page.reload();
  const last = page.getByText('Just one line.');
  await expect(page.getByTestId('unread-divider')).toBeInViewport();
  await expect(last).toBeInViewport({ ratio: 1 });
  await page.getByRole('textbox', { name: 'Prompt' }).fill('one\ntwo\nthree\nfour');
  await expect(last).toBeInViewport({ ratio: 1 });
});

/** Makes the page look as if the reader isn't there: in a background tab, or in a window that isn't the focused one. */
async function leave(page: Page, how: 'hidden' | 'unfocused') {
  await page.evaluate((how) => {
    if (how === 'hidden') {
      Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => 'hidden' });
      document.dispatchEvent(new Event('visibilitychange'));
    } else {
      document.hasFocus = () => false;
      window.dispatchEvent(new Event('blur'));
    }
  }, how);
}

async function comeBack(page: Page, how: 'hidden' | 'unfocused') {
  await page.evaluate((how) => {
    if (how === 'hidden') {
      Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => 'visible' });
      document.dispatchEvent(new Event('visibilitychange'));
    } else {
      document.hasFocus = () => true;
      window.dispatchEvent(new Event('focus'));
    }
  }, how);
}

for (const how of ['hidden', 'unfocused'] as const) {
  test(`a page that is ${how} follows the output but doesn't read it, and marks it read once the reader is back`, { tag: '@scroll' }, async ({ page }) => {
    await page.setViewportSize({ width: 1000, height: 500 });
    const id = await createSession(page, 'Give me a long answer in sloppod');
    await expect(page.getByText('The end.')).toBeInViewport({ timeout: 20_000 });
    const read = await readPosition(page, id);
    const divider = page.getByTestId('unread-divider');
    await expect(divider).toHaveCount(0);

    // Output comes in while nobody is looking: the view follows it, and nothing is read.
    await leave(page, how);
    await page.request.post('/_mock/say?text=While%20you%20were%20away.');
    await expect(page.getByText('While you were away.')).toBeInViewport({ ratio: 1 });
    await expect(divider).toHaveCount(1);
    await page.waitForTimeout(2500);
    expect((await (await page.request.get(`/api/v1/sessions/${id}`)).json()).read_seq).toBe(read);

    // Back, the latest is there with the bar above what came in meanwhile, for a moment...
    await comeBack(page, how);
    await expect(page.getByText('While you were away.')).toBeInViewport({ ratio: 1 });
    await expect(divider).toHaveCount(1);
    // ...and read once the reader has been there for a bit, which the other devices hear of.
    await expect(divider).toHaveCount(0);
    await readPosition(page, id, read);
  });
}

test('a session nobody has read yet, opened where nobody is looking, shows what came in as unread', { tag: '@scroll' }, async ({ page }) => {
  await page.setViewportSize({ width: 1000, height: 500 });
  const response = await page.request.post('/api/v1/sessions', { data: { prompt: 'Give me a long answer in sloppod' } });
  const { id } = await response.json();
  expect((await (await page.request.get(`/api/v1/sessions/${id}`)).json()).read_seq).toBeUndefined();
  // Opened in a background tab, before the first read report.
  await page.addInitScript(() => {
    Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => 'hidden' });
  });
  await page.goto(`/sessions/${id}`);
  await expect(page.getByText('The end.')).toBeAttached({ timeout: 20_000 });
  const divider = page.getByTestId('unread-divider');
  await expect(divider).toHaveCount(1);
  await page.waitForTimeout(2500);
  expect((await (await page.request.get(`/api/v1/sessions/${id}`)).json()).read_seq).toBeUndefined();

  await page.request.post('/_mock/say?text=Still%20unread.');
  await expect(page.getByText('Still unread.')).toBeAttached();
  await expect(divider).toHaveCount(1);

  // Looked at, it's read.
  await comeBack(page, 'hidden');
  await expect(divider).toHaveCount(0);
  await readPosition(page, id);
});

test('sending a message scrolls the chat to it, and so does scrolling the session list past its end', { tag: '@scroll' }, async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 500 });
  await createSession(page, 'Give me a long answer in sloppod');
  const end = page.getByText('The end.');
  await expect(end).toBeInViewport({ timeout: 20_000 });
  const scroller = page.getByTestId('transcript-scroller');
  const scrollTop = () => scroller.evaluate((element) => element.scrollTop);

  // Once the list has nowhere left to scroll (it may have a lot of sessions in it), the wheel goes on to the chat, either way.
  await end.hover();
  await page.mouse.wheel(0, -2000);
  await expect(end).not.toBeInViewport();
  const up = await scrollTop();
  await page.getByTestId('sidebar').hover();
  await expect
    .poll(async () => {
      await page.mouse.wheel(0, 300);
      return scrollTop();
    })
    .toBeGreaterThan(up);
  const down = await scrollTop();
  // Back up through the list, however long it is, to its top; then the wheel goes on to the chat.
  await page.mouse.wheel(0, -100_000);
  await expect
    .poll(async () => {
      await page.mouse.wheel(0, -200);
      return scrollTop();
    })
    .toBeLessThan(down);

  // A message sent from up there is scrolled to.
  await page.request.post('/_mock/say?text=Unseen.');
  await expect(page.getByText('Unseen.')).toBeAttached();
  await expect(page.getByText('Unseen.')).not.toBeInViewport();
  const message = 'Where did that go?';
  await page.getByRole('textbox', { name: 'Prompt' }).fill(message);
  await page.getByRole('button', { name: 'Send' }).click();
  await expect(userMessages(page).filter({ hasText: message })).toBeInViewport();
});

/** TEMP: prints the chat's own diagnostics, which a browser console doesn't put in the test's output. */
function showChatLog(page: Page) {
  page.on('console', (message) => {
    if (message.text().startsWith('DBG-history')) console.log(message.text());
  });
}

/** Opens a session whose log is longer than one page of history: the latest in the transcript, the rest below it. */
async function openLongHistory(page: Page) {
  showChatLog(page);
  await page.setViewportSize({ width: 1000, height: 500 });
  const prompt = 'Write a long history in sloppod';
  await createSession(page, prompt);
  await expect(page.getByText('The end.')).toBeVisible({ timeout: 20_000 });
  await page.reload();
  await expect(page.getByText('The end.')).toBeVisible();
  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(0);
  return prompt;
}

/**
 * Reads up to the top of the chat. A wheel event scrolls a page at most in some browsers (Firefox), so what the reader's
 * wheel asks for is scrolled here — the chat itself, by as much as the wheel said — rather than left to the browser.
 */
async function wheelUp(page: Page) {
  const scroller = page.getByTestId('transcript-scroller');
  await scroller.hover();
  await scroller.evaluate((element) => {
    element.dispatchEvent(new WheelEvent('wheel', { deltaY: -100_000, deltaMode: 0, bubbles: true }));
    element.scrollTo({ top: 0 });
  });
}

test('reading up brings in the earlier history by itself, with no click', { tag: '@scroll' }, async ({ page }) => {
  const prompt = await openLongHistory(page);
  await expect(page.getByRole('button', { name: 'Load earlier messages' })).toHaveCount(0);

  await wheelUp(page);
  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(1);
});

test('the row the reader is looking at stays put as the earlier page goes in above it', { tag: '@scroll' }, async ({ page }) => {
  const prompt = await openLongHistory(page);
  // The message the reader is in, found by its words rather than by its place: the page that goes in joins the same
  // message, its own start being what was missing.
  const row = page.locator('[data-testid="transcript"] [data-seq]').filter({ hasText: /word word word/ });
  const end = async () => {
    const box = (await row.boundingBox())!;
    return box.y + box.height;
  };
  // The page the reader has reached for is on its way but held, so what they were looking at is still there.
  await page.request.post('/_mock/hold-history');
  await wheelUp(page);
  await expect(page.getByTestId('loading-earlier')).toBeVisible();
  const held = await end();

  await page.request.post('/_mock/release-history');
  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(1);
  // Nothing of what went in above shows once it has settled (the rows off screen are laid out at a guess until they
  // come near): the words the reader was at are where they were, the message around them growing upwards instead.
  await expect.poll(async () => Math.abs((await end()) - held)).toBeLessThan(2);
});

test('the reader’s place holds when the row they are at is written to as the page comes in', { tag: '@scroll' }, async ({ page }) => {
  await page.setViewportSize({ width: 1000, height: 500 });
  const prompt = 'Write a long log with a tool call in sloppod';
  await createSession(page, prompt);
  await expect(page.getByText('The end.')).toBeVisible({ timeout: 20_000 });
  await page.reload();
  await expect(page.getByText('The end.')).toBeVisible();
  // The page that opens holds the latest of the log, which begins at the tool call: at the top of the chat, that row
  // is the reader's.
  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(0);
  const row = page.locator('[data-testid="transcript"] [data-row-names^="tool:"]');
  await expect(row).toHaveCount(1);

  await page.request.post('/_mock/hold-history');
  await wheelUp(page);
  await expect(page.getByTestId('loading-earlier')).toBeVisible();
  await expect(row).toBeInViewport();

  // The tool goes on working while the page is on its way: the row is the same row, though the seq it ends at moves on.
  const was = await row.getAttribute('data-seq');
  await page.request.post('/_mock/update', {
    data: {
      sessionUpdate: 'tool_call_update',
      toolCallId: (await row.getAttribute('data-row-names'))!.split(' ')[0].replace('tool:', ''),
      status: 'in_progress',
    },
  });
  await expect.poll(async () => row.getAttribute('data-seq')).not.toBe(was);

  const end = async () => {
    const box = (await row.boundingBox())!;
    return box.y + box.height;
  };
  const held = await end();
  await page.request.post('/_mock/release-history');
  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(1);
  // The page that went in above is all that moved the row, and the view was scrolled by as much: the reader's place is
  // where it was, the tool call's own update having been theirs to see rather than something to compensate for.
  await expect.poll(async () => Math.abs((await end()) - held)).toBeLessThan(2);
});

/**
 * Opens a session whose log is more than one page and whose latest page renders shorter than the view: one row, and
 * nothing to scroll. The turn is waited out first, so that what opens is the whole log's latest page.
 */
async function openUnscrollablePage(page: Page) {
  showChatLog(page);
  await page.setViewportSize({ width: 1000, height: 800 });
  const prompt = 'Write a short log with many tool updates in sloppod';
  await createSession(page, prompt);
  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');
  await page.reload();
  await expect(page.locator('[data-testid="transcript"] [data-row-names]')).toHaveCount(1);
  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(0);
  // The page fits the view: there is nothing to scroll, and so no scroll event for anything to be read from.
  expect(await page.getByTestId('transcript-scroller').evaluate((element) => element.scrollHeight <= element.clientHeight)).toBe(true);
  return prompt;
}

test('older history comes in when the page that opens cannot scroll', { tag: '@scroll' }, async ({ page }) => {
  const prompt = await openUnscrollablePage(page);

  // Asking to go up is what reaches the rest of the session, viewport or not.
  await page.getByTestId('transcript-scroller').hover();
  await page.mouse.wheel(0, -200);
  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(1);
});

test('the reader’s place holds when the page before widens a fold they are at', { tag: '@scroll' }, async ({ page }) => {
  await page.setViewportSize({ width: 1000, height: 500 });
  const prompt = 'Write a long log with a run of tool calls in sloppod';
  await createSession(page, prompt);
  await expect(page.getByText('The end.')).toBeVisible({ timeout: 20_000 });
  await page.reload();
  await expect(page.getByText('The end.')).toBeVisible();
  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(0);
  // The row at the top of what the reader has is the run of tool calls the page began in: one call of it, so far.
  const row = page.locator('[data-testid="transcript"] [data-row-names^="tool:"]');
  await expect(row).toHaveCount(1);
  const was = (await row.getAttribute('data-row-names'))!;
  expect(was.split(' ')).toHaveLength(1);

  await page.request.post('/_mock/hold-history');
  await wheelUp(page);
  await expect(page.getByTestId('loading-earlier')).toBeVisible();
  const end = async () => {
    const box = (await row.boundingBox())!;
    return box.y + box.height;
  };
  const held = await end();

  await page.request.post('/_mock/release-history');
  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(1);
  // The run is one fold now, named by an older tool call than the reader's: the reader's call is in it still, so the row
  // was found, and what the page put above them is all that was scrolled past.
  await expect.poll(async () => (await row.getAttribute('data-row-names'))!.split(' ')).toContain(was);
  await expect.poll(async () => Math.abs((await end()) - held)).toBeLessThan(2);
});

/** A touch gesture on the chat: a finger down at `from`, moves to `to`, and up. */
async function swipe(page: Page, from: number, to: number) {
  await page.evaluate(
    ([from, to]) => {
      const scroller = document.querySelector('[data-testid="transcript-scroller"]')!;
      const at = (clientY: number) => new Touch({ identifier: 1, target: scroller, clientX: 40, clientY });
      const send = (type: string, touches: Touch[]) =>
        scroller.dispatchEvent(new TouchEvent(type, { bubbles: true, cancelable: true, touches, changedTouches: touches }));
      send('touchstart', [at(from)]);
      for (let step = 1; step <= 6; step++) send('touchmove', [at(from + ((to - from) * step) / 6)]);
      send('touchend', []);
    },
    [from, to],
  );
}

test('a swipe is read within the gesture it is part of', { tag: '@scroll' }, async ({ page }) => {
  const prompt = await openUnscrollablePage(page);
  // Not every browser lets a test make touch events to send, and there is nothing to check where it doesn't.
  test.skip(
    !(await page.evaluate(() => typeof Touch === 'function' && typeof TouchEvent === 'function')),
    'this browser has no touch events to send',
  );

  // Two swipes up, the second beginning lower down than the first one ended: neither asks for older history.
  await swipe(page, 600, 400);
  await swipe(page, 700, 500);
  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(0);

  // A swipe down does.
  await swipe(page, 400, 700);
  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(1);
});

test('older history comes in for a reader who only drags the scrollbar', { tag: '@scroll' }, async ({ page }) => {
  const prompt = await openLongHistory(page);
  const scroller = page.getByTestId('transcript-scroller');
  // The scrollbar is all they use: no wheel, no key, no touch, and no button to fall back on.
  await page.getByText('The end.').hover();
  await page.mouse.down();
  await scroller.evaluate((element) => element.scrollTo({ top: 0 }));
  await page.mouse.up();

  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(1);
});

test('the start of the session is marked once nothing earlier is left', { tag: '@scroll' }, async ({ page }) => {
  const prompt = await openLongHistory(page);
  await expect(page.getByTestId('history-start')).toHaveCount(0);

  await wheelUp(page);
  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(1);
  // Up to the top of what's now all of it: the marker is at the beginning, and nothing is asked for again.
  await page.mouse.wheel(0, -100_000);
  await expect(page.getByTestId('history-start')).toBeInViewport();
});

test('an earlier page that could not be loaded offers a retry', { tag: '@scroll' }, async ({ page }) => {
  const prompt = await openLongHistory(page);
  await page.request.post('/_mock/fail-history');

  await wheelUp(page);
  const failed = page.getByTestId('history-error');
  await expect(failed).toContainText("Couldn't load earlier messages");
  await expect(failed.getByRole('button', { name: 'Retry' })).toBeVisible();
  // Nothing retries by itself: the reader says when.
  await page.mouse.wheel(0, -500);
  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(0);
  await expect(failed).toBeVisible();

  await failed.getByRole('button', { name: 'Retry' }).click();
  await expect(userMessages(page).filter({ hasText: prompt })).toHaveCount(1);
  await expect(failed).toHaveCount(0);
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

test("a session whose pod can't be placed says why, then starts", async ({ page }) => {
  await createSession(page, 'no room: fix the build in bo0tzz/sloppod');
  const activity = page.getByTestId('activity');
  await expect(activity).toHaveText('Waiting for room (insufficient memory)…');
  await expect(permissionCard(page)).toBeVisible({ timeout: 20_000 });
  await finishTurn(page);
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

test('approves a request to act as the user, with the token they pick', async ({ page }) => {
  const add = async (name: string) =>
    (await (
      await page.request.post('/api/v1/credentials', {
        data: { kind: 'github-user', name, secret: `github_pat_${name}`, config: {} },
      })
    ).json()) as { id: string };
  // The mock's "work" token is bo0tzz-work's; the other is bo0tzz's.
  const work = await add('work');
  const personal = await add('personal');
  try {
    await createSession(page, 'Act as me on bo0tzz/dotfiles');
    const card = page.getByTestId('proposal-card');
    await expect(card).toHaveAttribute('data-kind', 'sloppod.repo.act_as_owner');
    await expect(card).toContainText("sloppod's GitHub App isn't installed there.");
    // The agent's suggestion is the first token that reaches it; the user picks the other, and the card follows.
    await expect(card.getByTestId('proposal-card-title')).toHaveText('Act as bo0tzz-work on bo0tzz/dotfiles');
    await expect(card).toContainText('act on bo0tzz/dotfiles as bo0tzz-work, with your own GitHub token');
    await card.getByTestId('proposal-token').getByRole('combobox').click();
    await page.getByRole('option', { name: 'bo0tzz (personal)' }).click();
    await expect(card.getByTestId('proposal-card-title')).toHaveText('Act as bo0tzz on bo0tzz/dotfiles');
    await expect(card).toContainText('act on bo0tzz/dotfiles as bo0tzz, with your own GitHub token');
    const approving = page.waitForRequest((request) => request.url().endsWith('/approve'));
    await card.getByRole('button', { name: 'Approve' }).click();
    expect((await approving).postDataJSON()).toEqual({ credential: personal.id });
    await expect(card).toBeHidden();
    // Who it acted as stays on the proposal once it's carried out.
    await page.getByRole('button', { name: 'Proposals' }).click();
    await expect(page.getByRole('dialog').getByTestId('proposal-card-title')).toHaveText('Act as bo0tzz on bo0tzz/dotfiles');
    await page.keyboard.press('Escape');
    await expect(page.getByTestId('system-notice')).toContainText('On bo0tzz/dotfiles you now act as bo0tzz.');

    await page.getByRole('button', { name: 'Session actions' }).click();
    await page.getByRole('menuitem', { name: 'Grants' }).click();
    await expect(page.getByTestId('grants').getByTestId('grant-repo')).toContainText('as bo0tzz');
  } finally {
    for (const { id } of [work, personal]) await page.request.delete(`/api/v1/credentials/${id}`);
  }
});

// The mock is shared by the other tests, which expect no GitHub account.
async function deleteGitHubTokens(page: Page) {
  const credentials: { id: string; kind: string }[] = await (await page.request.get('/api/v1/credentials')).json();
  for (const credential of credentials.filter((entry) => entry.kind === 'github-user')) {
    await page.request.delete(`/api/v1/credentials/${credential.id}`);
  }
}

async function openNewSession(page: Page) {
  await page.goto('/');
  await page.getByRole('link', { name: 'New session' }).click();
  await expect(page).toHaveURL(/\/new$/);
}

test('the new-session page suggests a starting set, and sends the one the user edited', async ({ page }) => {
  await openNewSession(page);
  const card = page.getByTestId('starting-card');
  // The empty draft is suggested at once; it says what can be chosen.
  await expect(card).toBeVisible();
  for (const capability of ['github', 'packages']) {
    await expect(card.getByTestId(`capability-${capability}`).getByRole('checkbox')).toBeChecked();
  }
  // Docs come with every session, so there's nothing to choose.
  await expect(card.getByTestId('capability-docs')).toHaveCount(0);
  // Each says what it gives, and nothing claims to be matched from a message that isn't there yet.
  await expect(card.getByTestId('capability-github').getByTestId('capability-description')).toContainText('GitHub');
  await expect(card.getByTestId('capability-packages').getByTestId('capability-description')).toContainText('Package registries');
  await expect(card.getByTestId('starting-source')).toHaveText('');

  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('Fix the build in bo0tzz/sloppod');
  const title = card.getByRole('textbox', { name: 'Title' });
  await expect(title).toHaveValue('Fix the build in bo0tzz/sloppod');
  await expect(card.getByTestId('starting-repos')).toContainText('bo0tzz/sloppod');
  await expect(card.getByTestId('starting-source')).toHaveText('Matched from your message');

  // What the user changes stays, whatever a later suggestion says; the rest follows it.
  await title.fill('Mend the build');
  await card.getByTestId('capability-packages').getByRole('checkbox').click();
  await expect(card.getByTestId('capability-packages').getByRole('checkbox')).not.toBeChecked();
  await box.fill('Fix the build in bo0tzz/memini');
  await expect(card.getByTestId('starting-repos')).toContainText('bo0tzz/memini');
  await expect(title).toHaveValue('Mend the build');
  await expect(card.getByTestId('capability-packages').getByRole('checkbox')).not.toBeChecked();

  await box.press('Enter');
  await expect(page).toHaveURL(/\/sessions\/[0-9a-f-]+$/);
  await expect(page.getByTestId('session-title')).toHaveText('Mend the build');
  await expect(page.getByTestId('session-subtitle')).toHaveText('bo0tzz/memini');
  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Grants' }).click();
  const grants = page.getByTestId('grants');
  await expect(grants.getByTestId('grant-docs')).toBeVisible();
  await expect(grants.getByTestId('grant-packages')).toHaveCount(0);
});

test("a session starts acting as one of the user's GitHub tokens on its repository", async ({ page }) => {
  await page.request.post('/api/v1/credentials', {
    data: { kind: 'github-user', name: 'mine', secret: 'github_pat_x', config: {} },
  });
  try {
    await openNewSession(page);
    const card = page.getByTestId('starting-card');
    const box = page.getByRole('textbox', { name: 'Prompt' });
    await box.fill('Fix the build in bo0tzz/sloppod');
    const row = card.getByTestId('starting-repo');
    // The suggestion picks the repository, never a token: it acts as the App until the user says otherwise.
    await expect(row.getByRole('combobox', { name: 'Act as' })).toContainText('The GitHub App');
    await expect(row.getByRole('combobox', { name: 'Repository' })).toContainText('bo0tzz/sloppod');
    await row.getByRole('combobox', { name: 'Act as' }).click();
    await page.getByRole('option', { name: 'bo0tzz (mine)' }).click();
    await expect(card.getByText('What the session pushes there is attributed to that person.')).toBeVisible();
    // What the token reaches is offered, and a repository from the App's list that it doesn't isn't.
    await row.getByRole('combobox', { name: 'Repository' }).click();
    await expect(page.getByRole('option', { name: 'bo0tzz/memini' })).toHaveCount(0);
    await page.getByRole('option', { name: 'bo0tzz/dotfiles' }).click();

    await box.press('Enter');
    await expect(page).toHaveURL(/\/sessions\/[0-9a-f-]+$/);
    await expect(page.getByTestId('session-subtitle')).toHaveText('bo0tzz/dotfiles');
    await page.getByRole('button', { name: 'Session actions' }).click();
    await page.getByRole('menuitem', { name: 'Grants' }).click();
    await expect(page.getByTestId('grants').getByTestId('grant-repo')).toContainText('as bo0tzz');
    // GitHub comes with a repository, whether or not it was ticked.
    await expect(page.getByTestId('grants').getByTestId('grant-github')).toBeVisible();
  } finally {
    await deleteGitHubTokens(page);
  }
});

test("the card starts a session with one of the user's own capabilities, and with more than one repository", async ({ page }) => {
  const made: { id: string } = await (
    await page.request.post('/api/v1/capabilities', {
      data: { name: 'victorialogs', description: "Search the cluster's logs", mcp_url: 'https://logs.example.com/mcp' },
    })
  ).json();
  try {
    await openNewSession(page);
    const card = page.getByTestId('starting-card');
    const own = card.getByTestId('capability-victorialogs').getByRole('checkbox');
    // It's offered, and is never in a suggestion.
    await expect(own).not.toBeChecked();
    await page.getByRole('textbox', { name: 'Prompt' }).fill('Fix the build in bo0tzz/sloppod');
    await expect(card.getByTestId('starting-repos')).toContainText('bo0tzz/sloppod');
    await expect(own).not.toBeChecked();
    await own.click();
    // A second repository, from the App's list less the one the first has.
    await card.getByRole('button', { name: 'Add repository' }).click();
    await card.getByTestId('starting-repo').nth(1).getByRole('combobox', { name: 'Repository' }).click();
    await expect(page.getByRole('option', { name: 'bo0tzz/sloppod' })).toHaveCount(0);
    await page.getByRole('option', { name: 'bo0tzz/memini' }).click();

    await page.getByRole('textbox', { name: 'Prompt' }).press('Enter');
    await expect(page).toHaveURL(/\/sessions\/[0-9a-f-]+$/);
    await expect(page.getByTestId('session-subtitle')).toHaveText('bo0tzz/sloppod');
    await page.getByRole('button', { name: 'Session actions' }).click();
    await page.getByRole('menuitem', { name: 'Grants' }).click();
    const grants = page.getByTestId('grants');
    await expect(grants.getByTestId('grant-victorialogs')).toContainText("Search the cluster's logs");
    await expect(grants.getByTestId('grant-repo')).toHaveCount(2);
  } finally {
    await page.request.delete(`/api/v1/capabilities/${made.id}`);
  }
});

// Holds the suggestion for `prompt` back for `ms`, as a slow runtime does.
async function delaySuggestion(page: Page, prompt: string, ms: number) {
  await page.route('**/api/v1/sessions/suggest', async (route) => {
    if (route.request().postDataJSON().prompt === prompt) await new Promise((resolve) => setTimeout(resolve, ms));
    // The page may be gone by then, after a test that doesn't wait for it.
    await route.continue().catch(() => {});
  });
}

test('what can be chosen comes from the empty draft, however late it answers', async ({ page }) => {
  await delaySuggestion(page, '', 2500);
  await openNewSession(page);
  await page.getByRole('textbox', { name: 'Prompt' }).fill('question: how does the library behave?');
  const card = page.getByTestId('starting-card');
  await expect(card.getByTestId('starting-source')).toHaveText('Suggested by the model');
  // The judgement came first, with packages alone; the empty draft's answer still says what else can be chosen.
  await expect(card.getByTestId('capability-github').getByRole('checkbox')).not.toBeChecked();
  await expect(card.getByTestId('capability-packages').getByRole('checkbox')).toBeChecked();
});

test('an answer for an earlier draft never replaces the card for a later one', async ({ page }) => {
  await delaySuggestion(page, 'Fix the build in bo0tzz/sloppod', 2000);
  await openNewSession(page);
  const card = page.getByTestId('starting-card');
  await expect(card).toBeVisible();
  const box = page.getByRole('textbox', { name: 'Prompt' });
  const earlier = page.waitForResponse((response) => response.url().endsWith('/sessions/suggest') && response.request().postDataJSON().prompt.startsWith('Fix'));
  await box.fill('Fix the build in bo0tzz/sloppod');
  await page.waitForRequest((request) => request.url().endsWith('/sessions/suggest') && request.postDataJSON().prompt.startsWith('Fix'));
  // Typed again while the first answer is on its way, so that it lands while the new draft waits to be asked about.
  await page.waitForTimeout(1500);
  await box.fill('question: how does the library behave?');
  await earlier;
  await expect(card.getByRole('textbox', { name: 'Title' })).not.toHaveValue('Fix the build in bo0tzz/sloppod');
  await expect(card.getByTestId('starting-source')).toHaveText('Suggested by the model');
  await expect(card.getByRole('textbox', { name: 'Title' })).toHaveValue('question: how does the library behave?');
  await expect(card.getByTestId('starting-repo')).toHaveCount(0);
});

test('sending before the suggestion for the message is in uses that suggestion', async ({ page }) => {
  await openNewSession(page);
  await expect(page.getByTestId('starting-card')).toBeVisible();
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('Fix the build in bo0tzz/sloppod');
  await box.press('Enter');
  await expect(page).toHaveURL(/\/sessions\/[0-9a-f-]+$/);
  await expect(page.getByTestId('session-subtitle')).toHaveText('bo0tzz/sloppod');
  await expect(page.getByTestId('session-title')).toHaveText('Fix the build in bo0tzz/sloppod');
});

test('a session starts with the suggestion for the message sent, though the draft changed while it waited', async ({ page }) => {
  await delaySuggestion(page, 'Fix the build in bo0tzz/sloppod', 2000);
  await openNewSession(page);
  await expect(page.getByTestId('starting-card')).toBeVisible();
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('Fix the build in bo0tzz/sloppod');
  await box.press('Enter');
  // The suggestion for that message is slow; typing another one meanwhile gets its own, which comes first.
  await box.fill('question: how does the library behave?');
  await expect(page.getByTestId('starting-source')).toHaveText('Suggested by the model');
  await expect(page).toHaveURL(/\/sessions\/[0-9a-f-]+$/);
  await expect(page.getByTestId('session-title')).toHaveText('Fix the build in bo0tzz/sloppod');
  await expect(page.getByTestId('session-subtitle')).toHaveText('bo0tzz/sloppod');
});

test('a slow suggestion doesn\'t hold up starting the session', async ({ page }) => {
  // A cold judgement can take a minute; sending waits for it only a few seconds.
  await delaySuggestion(page, 'Fix the build in bo0tzz/sloppod', 60_000);
  await openNewSession(page);
  await expect(page.getByTestId('starting-card')).toBeVisible();
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('Fix the build in bo0tzz/sloppod');
  await box.press('Enter');
  await expect(page).toHaveURL(/\/sessions\/[0-9a-f-]+$/, { timeout: 8000 });
  // Without the suggestion the runtime derives the rest, as it did before there were suggestions.
  await expect(page.getByTestId('session-subtitle')).toHaveText('bo0tzz/sloppod');
});

test('choosing no repository sends none, though the message names one', async ({ page }) => {
  await openNewSession(page);
  const card = page.getByTestId('starting-card');
  await page.getByRole('textbox', { name: 'Prompt' }).fill('Fix the build in bo0tzz/sloppod');
  await expect(card.getByTestId('starting-repos')).toContainText('bo0tzz/sloppod');
  await card.getByRole('button', { name: 'Remove repository' }).click();
  await page.getByRole('textbox', { name: 'Prompt' }).press('Enter');
  await expect(page).toHaveURL(/\/sessions\/[0-9a-f-]+$/);
  await expect(page.getByTestId('session-subtitle')).toHaveText('No repository');
});

test('a judgement of the first message shows as its source, and gives the card its repository and grants', async ({ page }) => {
  await openNewSession(page);
  const card = page.getByTestId('starting-card');
  await page.getByRole('textbox', { name: 'Prompt' }).fill('question: how does the library behave?');
  await expect(card.getByTestId('starting-source')).toHaveText('Suggested by the model');
  await expect(card.getByTestId('starting-repo')).toHaveCount(0);
  await expect(card.getByTestId('capability-packages').getByRole('checkbox')).toBeChecked();
  await expect(card.getByTestId('capability-github').getByRole('checkbox')).not.toBeChecked();
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

test("adds a repository as one of the user's GitHub tokens", async ({ page }) => {
  await page.request.post('/api/v1/credentials', {
    data: { kind: 'github-user', name: 'mine', secret: 'github_pat_x', config: {} },
  });
  await createSession(page, 'Just chat for now');

  const picker = page.getByTestId('add-repo');
  const openPicker = async () => {
    await page.getByRole('button', { name: 'Session actions' }).click();
    await page.getByRole('menuitem', { name: 'Add repository…' }).click();
  };
  await openPicker();
  await expect(picker.getByText('What the session pushes there is attributed to that person.')).toHaveCount(0);
  await picker.getByRole('combobox', { name: 'Act as' }).click();
  await page.getByRole('option', { name: 'bo0tzz (mine)' }).click();
  await expect(picker.getByText('What the session pushes there is attributed to that person.')).toBeVisible();

  // The token's repositories are offered, not the App's.
  await picker.getByRole('combobox', { name: 'Repository' }).click();
  await expect(page.getByRole('option', { name: 'bo0tzz/memini' })).toHaveCount(0);
  await page.getByRole('option', { name: 'bo0tzz/dotfiles' }).click();
  await page.getByRole('button', { name: 'Add', exact: true }).click();
  await expect(page.getByTestId('session-subtitle')).toHaveText('bo0tzz/dotfiles');

  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Grants' }).click();
  await expect(page.getByRole('dialog').getByText('as bo0tzz')).toBeVisible();
  await page.keyboard.press('Escape');

  // It isn't offered again to the same token, but the App can still have it.
  await openPicker();
  await picker.getByRole('combobox', { name: 'Act as' }).click();
  await page.getByRole('option', { name: 'bo0tzz (mine)' }).click();
  await picker.getByRole('combobox', { name: 'Repository' }).click();
  await expect(page.getByRole('option', { name: 'bo0tzz/sloppod' })).toBeVisible();
  await expect(page.getByRole('option', { name: 'bo0tzz/dotfiles' })).toHaveCount(0);
  await page.keyboard.press('Escape');

  // Nor is it offered to the App: adding it without the token would leave the grant acting as the person.
  await picker.getByRole('combobox', { name: 'Act as' }).click();
  await page.getByRole('option', { name: 'The GitHub App' }).click();
  await picker.getByRole('combobox', { name: 'Repository' }).click();
  await expect(page.getByRole('option', { name: 'bo0tzz/sloppod' })).toBeVisible();
  await expect(page.getByRole('option', { name: 'bo0tzz/dotfiles' })).toHaveCount(0);

  await deleteGitHubTokens(page);
});

test('a token that reaches more repositories than it lists can add one that is not listed', async ({ page }) => {
  await page.request.post('/api/v1/credentials', {
    data: { kind: 'github-user', name: 'wide', secret: 'github_pat_many', config: {} },
  });
  await createSession(page, 'Just chat for now');
  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Add repository…' }).click();

  const picker = page.getByTestId('add-repo');
  await picker.getByRole('combobox', { name: 'Act as' }).click();
  await page.getByRole('option', { name: 'bo0tzz (wide)' }).click();
  await picker.getByTestId('unlisted-repo').fill('someone/else');
  await page.getByRole('button', { name: 'Add', exact: true }).click();
  await expect(page.getByTestId('session-subtitle')).toHaveText('someone/else');

  // The listed repositories are still offered, and a name that isn't one is refused.
  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Add repository…' }).click();
  await picker.getByRole('combobox', { name: 'Act as' }).click();
  await page.getByRole('option', { name: 'bo0tzz (wide)' }).click();
  await picker.getByRole('combobox', { name: 'Repository' }).click();
  await expect(page.getByRole('option', { name: 'bo0tzz/dotfiles' })).toBeVisible();
  await page.keyboard.press('Escape');
  await picker.getByTestId('unlisted-repo').fill('not a repository');
  await page.getByRole('button', { name: 'Add', exact: true }).click();
  await expect(picker.getByText("doesn't reach not a repository")).toBeVisible();

  await deleteGitHubTokens(page);
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

test("Esc on a question card keeps the agent's questions; the composer's Esc still stops the turn", async ({ page }) => {
  await createSession(page, 'Please ask me how to do it');
  await expect(questionCard(page)).toBeVisible();
  // The card takes focus when it appears, so a stray Esc goes to it.
  await expect(questionCard(page)).toBeFocused();
  await page.keyboard.press('Escape');
  await expect(questionCard(page)).toBeVisible();
  await expect(phase(page)).toHaveAttribute('data-phase', 'waiting');

  // Nor does Esc from one of its options.
  const option = questionField(page, 'question_0').getByRole('radio', { name: /Exponential/ });
  await option.check();
  await page.keyboard.press('Escape');
  await expect(questionCard(page)).toBeVisible();
  await expect(option).toBeChecked();

  await page.getByRole('textbox', { name: 'Prompt' }).focus();
  await page.keyboard.press('Escape');
  await expect(questionCard(page)).toBeHidden();
  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');
});

test("a question card leaves the composer focused while something's being written there", async ({ page, context }) => {
  const id = await createSession(page, 'Add backoff to the reconnect logic in sloppod');
  await finishTurn(page);
  const prompt = page.getByRole('textbox', { name: 'Prompt' });
  await prompt.fill('half a thought');
  // The question comes from a message sent in another tab.
  const other = await context.newPage();
  await other.goto(`/sessions/${id}`);
  const otherPrompt = other.getByRole('textbox', { name: 'Prompt' });
  await otherPrompt.fill('Please ask me how to do it');
  await otherPrompt.press('Enter');
  await expect(questionCard(page)).toBeVisible();
  await expect(prompt).toBeFocused();
  await expect(prompt).toHaveValue('half a thought');
  await expect(questionCard(other)).toBeFocused();
});

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

  // One question at a time: the second one, and its option descriptions, wait for Next.
  await expect(questionCard(page).getByTestId('question-step')).toHaveText('Question 1 of 2');
  await expect(questionField(page, 'question_1')).toHaveCount(0);
  await expect(questionCard(page).getByRole('button', { name: 'Answer' })).toHaveCount(0);

  const backoff = questionField(page, 'question_0');
  await expect(backoff).toContainText('Backoff');
  await expect(backoff).toContainText('Which backoff strategy should reconnects use?');
  await expect(backoff).toContainText('Double the delay after each failure');
  await backoff.getByRole('radio', { name: /Exponential/ }).check();
  await expect(backoff.getByTestId('option-preview')).toContainText('Math.min(500 * 2 ** attempt');
  await questionCard(page).getByRole('button', { name: 'Next' }).click();
  await expect(questionCard(page).getByTestId('question-step')).toHaveText('Question 2 of 2');
  await expect(questionField(page, 'question_0')).toHaveCount(0);
  await expect(questionCard(page)).not.toContainText('Double the delay after each failure');

  // Back keeps what was picked.
  await questionCard(page).getByRole('button', { name: 'Back' }).click();
  await expect(questionField(page, 'question_0').getByRole('radio', { name: /Exponential/ })).toBeChecked();
  await questionCard(page).getByRole('button', { name: 'Next' }).click();

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

test('Enter in the "Other" field moves on, and on the last question submits the answers', async ({ page }) => {
  await createSession(page, 'Ask me before you start');
  await questionField(page, 'question_0_custom').getByRole('textbox').press('Enter');
  await expect(questionCard(page).getByTestId('question-step')).toHaveText('Question 2 of 2');
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

test('a highlight shows as a card in the chat and in the for-you list, and is still there after a reload', async ({ page }) => {
  await page.setViewportSize({ width: 1920, height: 1000 });
  const prompt = 'Highlight: Your question about mise tools: use `mise use`, see https://mise.jdx.dev';
  const id = await createSession(page, prompt);
  const card = page.getByTestId('highlight-card');
  const item = page.getByTestId('side-panel').getByTestId('highlight-item');
  await expect(card).toContainText('Your question about mise tools');
  await expect(card.getByRole('link', { name: 'https://mise.jdx.dev' })).toBeVisible();
  await expect(item).toContainText('Your question about mise tools');
  // The tool call is the card, not another row beside it.
  await expect(page.getByTestId('tool-call').filter({ hasText: 'highlight' })).toHaveCount(0);
  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');
  await expect(phase(page)).toHaveText('Waiting on you');
  await expect(phase(page)).toHaveAttribute('title', /1 highlight/);

  await page.reload();
  await expect(page).toHaveURL(new RegExp(id));
  await expect(page.getByTestId('highlight-card')).toContainText('Your question about mise tools');
  await expect(page.getByTestId('side-panel').getByTestId('highlight-item')).toHaveCount(1);
  await expect(phase(page)).toHaveText('Waiting on you');
});

test('several highlights are open at once, each until it is marked seen, and marking one starts no turn', async ({ page }) => {
  await page.setViewportSize({ width: 1920, height: 1000 });
  const prompt = 'Highlight: Merge pull request 12 by hand | Re-run the nightly job';
  await createSession(page, prompt);
  const panel = page.getByTestId('side-panel');
  await expect(panel.getByTestId('highlight-item')).toHaveCount(2);
  await expect(page.getByTestId('highlight-card')).toHaveCount(2);
  await expect(phase(page)).toHaveAttribute('title', /2 highlights/);
  const row = page.getByRole('navigation', { name: 'Sessions' }).getByTestId('session-row').filter({ hasText: 'Highlight: Merge pull request 12' });
  await expect(row.getByTestId('phase')).toHaveText('Waiting on you');

  // Marked seen from the list: the card in the chat shows it, and the other stays open.
  const turnsBefore = await agentMessages(page).count();
  const noticesBefore = await userMessages(page).count();
  await panel.getByTestId('highlight-item').filter({ hasText: 'Merge pull request 12' }).getByRole('button', { name: 'Seen' }).click();
  await expect(panel.getByTestId('highlight-item')).toHaveCount(1);
  await expect(page.getByTestId('highlight-card').filter({ hasText: 'Merge pull request 12' })).toHaveAttribute('data-seen', 'true');
  await expect(page.getByTestId('highlight-card').filter({ hasText: 'Re-run the nightly job' })).toHaveAttribute('data-seen', 'false');
  await expect(phase(page)).toHaveText('Waiting on you');

  // The last one, from its card in the chat: nothing waits on the user any more.
  await page.getByTestId('highlight-card').filter({ hasText: 'Re-run the nightly job' }).getByRole('button', { name: 'Seen' }).click();
  await expect(panel.getByTestId('highlight-item')).toHaveCount(0);
  await expect(phase(page)).toHaveText('Idle');
  await expect(row.getByTestId('phase')).toHaveText('Idle');
  // No turn started, and the agent wasn't told: the chat has nothing new, and the session is idle.
  await page.waitForTimeout(500);
  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');
  expect(await agentMessages(page).count()).toBe(turnsBefore);
  expect(await userMessages(page).count()).toBe(noticesBefore);

  // Seen stays seen after a reload.
  await page.reload();
  await expect(page.getByTestId('highlight-card')).toHaveCount(2);
  await expect(page.getByTestId('highlight-card').first()).toHaveAttribute('data-seen', 'true');
  await expect(page.getByTestId('highlight-item')).toHaveCount(0);
});

test('a highlight the way codex-acp reports it shows as a card too, after a reload as well', async ({ page }) => {
  await page.setViewportSize({ width: 1920, height: 1000 });
  const id = await createSession(page, 'Highlight (codex): Your question about mise tools: use mise use | Merge pull request 12 by hand');
  // Its own title, the input wrapped with the server and tool, the result as rawOutput: still cards, one for each call.
  await expect(page.getByTestId('highlight-card')).toHaveCount(2);
  await expect(page.getByTestId('highlight-card').first()).toContainText('Your question about mise tools');
  await expect(page.getByTestId('highlight-card').last()).toContainText('Merge pull request 12');
  await expect(page.getByTestId('tool-call').filter({ hasText: 'highlight' })).toHaveCount(0);
  await expect(page.getByTestId('side-panel').getByTestId('highlight-item')).toHaveCount(2);

  await page.reload();
  await expect(page).toHaveURL(new RegExp(id));
  await expect(page.getByTestId('highlight-card')).toHaveCount(2);
  await page.getByTestId('highlight-card').last().getByRole('button', { name: 'Seen' }).click();
  await expect(page.getByTestId('highlight-card').last()).toHaveAttribute('data-seen', 'true');
  await expect(page.getByTestId('highlight-card').first()).toHaveAttribute('data-seen', 'false');
});

test('a highlight retried after a failed call shows as the retry\'s card, not the failed one\'s', async ({ page }) => {
  await page.setViewportSize({ width: 1920, height: 1000 });
  await createSession(page, 'Highlight (retry): Merge pull request 12 by hand');
  // The result of the retry names no highlight, so it's matched by text: the failed call has the same text and mustn't take it.
  await expect(page.getByTestId('highlight-card')).toHaveCount(1);
  const failed = page.getByTestId('tool-call').filter({ hasText: 'highlight' });
  await expect(failed).toHaveCount(1);
  await expect(failed).toHaveAttribute('data-status', 'failed');
  const order = await page.getByTestId('transcript').locator('[data-testid="tool-call"], [data-testid="highlight-card"]').evaluateAll((nodes) => nodes.map((node) => node.getAttribute('data-testid')));
  expect(order).toEqual(['tool-call', 'highlight-card']);
});

test('a highlight that could not be marked seen says so, and can be tried again', async ({ page }) => {
  await page.setViewportSize({ width: 1920, height: 1000 });
  await createSession(page, 'Highlight: Re-run the nightly job');
  const item = page.getByTestId('side-panel').getByTestId('highlight-item');
  let failing = true;
  await page.route('**/highlights/*/seen', (route) => (failing ? route.fulfill({ status: 500, json: { error: 'database unavailable' } }) : route.fallback()));
  await item.getByRole('button', { name: 'Seen' }).click();
  await expect(item.getByTestId('highlight-error')).toContainText('database unavailable');
  await expect(item.getByRole('button', { name: 'Seen' })).toBeEnabled();
  failing = false;
  await item.getByRole('button', { name: 'Seen' }).click();
  await expect(page.getByTestId('side-panel').getByTestId('highlight-item')).toHaveCount(0);
  await expect(page.getByTestId('highlight-card')).toHaveAttribute('data-seen', 'true');
});

test('a highlight the agent takes back leaves the for-you list, and its card stays muted with the reason', async ({ page }) => {
  await page.setViewportSize({ width: 1920, height: 1000 });
  const id = await createSession(page, 'Highlight (retract): Merge pull request 12 by hand | It was pull request 21, not 12');
  const card = page.getByTestId('highlight-card');
  await expect(card).toHaveCount(1);
  await expect(card).toHaveAttribute('data-retracted', 'true');
  await expect(card.getByTestId('highlight-retraction')).toHaveText('Retracted: It was pull request 21, not 12');
  await expect(card).toContainText('Merge pull request 12 by hand');
  await expect(card.getByRole('button', { name: 'Seen' })).toHaveCount(0);
  // Not for the user any more: no list item, and nothing waits on them.
  await expect(page.getByTestId('highlight-item')).toHaveCount(0);
  await expect(phase(page)).toHaveText('Idle');
  const row = page.getByRole('navigation', { name: 'Sessions' }).getByTestId('session-row').filter({ hasText: 'Highlight (retract)' });
  await expect(row.getByTestId('phase')).toHaveText('Idle');

  await page.reload();
  await expect(page).toHaveURL(new RegExp(id));
  await expect(page.getByTestId('highlight-card')).toHaveAttribute('data-retracted', 'true');
  await expect(page.getByTestId('highlight-item')).toHaveCount(0);
});

test('snoozing a highlight moves it to the Snoozed group, out of what waits, and starts no turn', async ({ page }) => {
  await page.setViewportSize({ width: 1920, height: 1000 });
  // Its own highlight texts: the session list is one page, and a row is found by its title, so this test's must not
  // share a prefix with another test's session.
  await createSession(page, 'Highlight: Snooze the deploy check | Re-run the nightly job');
  const panel = page.getByTestId('side-panel');
  await expect(panel.getByTestId('highlight-item')).toHaveCount(2);
  await expect(phase(page)).toHaveAttribute('title', /2 highlights/);
  const row = page.getByRole('navigation', { name: 'Sessions' }).getByTestId('session-row').filter({ hasText: 'Snooze the deploy check' });

  const turnsBefore = await agentMessages(page).count();
  const noticesBefore = await userMessages(page).count();
  await panel.getByTestId('highlight-item').filter({ hasText: 'Snooze the deploy check' }).getByRole('button', { name: 'Snooze' }).click();
  await page.getByRole('menuitem', { name: 'In an hour' }).click();

  // Out of what waits on the user, into a group folded away under it.
  await expect(panel.getByTestId('highlight-item')).toHaveCount(1);
  await expect(phase(page)).toHaveAttribute('title', /1 highlight/);
  const toggle = panel.getByTestId('snoozed-toggle');
  await expect(toggle).toHaveText(/Snoozed \(1\)/);
  await expect(panel.getByTestId('snoozed-items')).toHaveCount(0);
  // Its card in the chat says it's put off, and keeps Seen.
  const card = page.getByTestId('highlight-card').filter({ hasText: 'Snooze the deploy check' });
  await expect(card).toHaveAttribute('data-snoozed', 'true');
  await expect(card).toContainText('Snoozed until');
  await expect(card.getByRole('button', { name: 'Seen' })).toBeVisible();

  // Unfolded: its text, until when, and Seen and Snooze again.
  await toggle.click();
  const snoozed = panel.getByTestId('snoozed-item');
  await expect(snoozed).toHaveCount(1);
  await expect(snoozed).toContainText('Snooze the deploy check');
  await expect(snoozed.getByTestId('highlight-snoozed-until')).toContainText('until');
  await expect(snoozed.getByRole('button', { name: 'Snooze' })).toBeVisible();

  // Put the other off too: nothing waits on the user, and no turn ran for either.
  await panel.getByTestId('highlight-item').getByRole('button', { name: 'Snooze' }).click();
  await page.getByRole('menuitem', { name: 'In an hour' }).click();
  await expect(panel.getByTestId('highlight-item')).toHaveCount(0);
  await expect(panel.getByTestId('snoozed-toggle')).toHaveText(/Snoozed \(2\)/);
  await expect(phase(page)).toHaveText('Idle');
  await expect(row.getByTestId('phase')).toHaveText('Idle');
  await page.waitForTimeout(500);
  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');
  expect(await agentMessages(page).count()).toBe(turnsBefore);
  expect(await userMessages(page).count()).toBe(noticesBefore);

  // Seen from the group is done with it for good, and stays so after a reload.
  await panel.getByTestId('snoozed-item').filter({ hasText: 'Snooze the deploy check' }).getByRole('button', { name: 'Seen' }).click();
  await expect(panel.getByTestId('snoozed-toggle')).toHaveText(/Snoozed \(1\)/);
  await expect(card).toHaveAttribute('data-seen', 'true');
  await page.reload();
  await expect(panel.getByTestId('snoozed-toggle')).toHaveText(/Snoozed \(1\)/);
  await expect(card).toHaveAttribute('data-seen', 'true');
});

test('a snoozed highlight comes back at its time, without a reload', async ({ page }) => {
  await page.setViewportSize({ width: 1920, height: 1000 });
  const id = await createSession(page, 'Highlight: Re-run the nightly job');
  const panel = page.getByTestId('side-panel');
  const item = panel.getByTestId('highlight-item');
  await expect(item).toHaveCount(1);

  // Put off for a moment: the check the UI's choices never need brings it back when its time passes.
  const [highlight] = await (await page.request.get(`/api/v1/sessions/${id}/highlights`)).json();
  await page.request.post(`/api/v1/sessions/${id}/highlights/${highlight.id}/snooze`, {
    data: { until: new Date(Date.now() + 3000).toISOString() },
  });
  await expect(item).toHaveCount(0);
  await expect(panel.getByTestId('snoozed-toggle')).toHaveText(/Snoozed \(1\)/);
  await expect(phase(page)).toHaveText('Idle');

  // Back among what waits on the user, with nothing reloaded.
  await expect(item).toHaveCount(1);
  await expect(panel.getByTestId('snoozed-toggle')).toHaveCount(0);
  await expect(phase(page)).toHaveText('Waiting on you');
  await expect(page.getByTestId('highlight-card')).toHaveAttribute('data-snoozed', 'false');
});

test('snoozing a highlight again pushes it further out, past the first time', async ({ page }) => {
  await page.setViewportSize({ width: 1920, height: 1000 });
  const id = await createSession(page, 'Highlight: Check the deploy after the nightly job');
  const panel = page.getByTestId('side-panel');
  await expect(panel.getByTestId('highlight-item')).toHaveCount(1);
  const [highlight] = await (await page.request.get(`/api/v1/sessions/${id}/highlights`)).json();
  const snoozeFor = (ms: number) =>
    page.request.post(`/api/v1/sessions/${id}/highlights/${highlight.id}/snooze`, {
      data: { until: new Date(Date.now() + ms).toISOString() },
    });

  await snoozeFor(2000);
  await expect(panel.getByTestId('snoozed-toggle')).toHaveText(/Snoozed \(1\)/);
  // Pushed a few seconds further out, as Snooze again does: the first time passing doesn't bring it back.
  await snoozeFor(6000);
  await page.waitForTimeout(2500);
  await expect(panel.getByTestId('snoozed-toggle')).toHaveText(/Snoozed \(1\)/);
  await expect(panel.getByTestId('highlight-item')).toHaveCount(0);

  // And it comes back at the time it was pushed to.
  await expect(panel.getByTestId('highlight-item')).toHaveCount(1);
  await expect(panel.getByTestId('snoozed-toggle')).toHaveCount(0);
});

test('the agent can still take back a highlight the user has snoozed', async ({ page }) => {
  await page.setViewportSize({ width: 1920, height: 1000 });
  await createSession(page, 'Highlight: Merge pull request 12 by hand');
  const panel = page.getByTestId('side-panel');
  await panel.getByTestId('highlight-item').getByRole('button', { name: 'Snooze' }).click();
  await page.getByRole('menuitem', { name: 'In an hour' }).click();
  await expect(panel.getByTestId('snoozed-toggle')).toHaveText(/Snoozed \(1\)/);

  // Snoozing isn't seeing it: the agent takes it back, and it leaves the user's list for good.
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('Retract the highlight: It was pull request 21, not 12');
  await box.press('Enter');
  const card = page.getByTestId('highlight-card');
  await expect(card).toHaveAttribute('data-retracted', 'true');
  await expect(card.getByTestId('highlight-retraction')).toHaveText('Retracted: It was pull request 21, not 12');
  await expect(panel.getByTestId('snoozed-toggle')).toHaveCount(0);
  await expect(panel.getByTestId('snoozed-item')).toHaveCount(0);
});

test('a question the agent withdraws closes, with a note in the chat, and stops counting as waiting', async ({ page }) => {
  await createSession(page, 'Withdraw the question: The user answered in the chat');
  await expect(page.getByTestId('notice')).toContainText('Withdrawn: The user answered in the chat');
  await expect(page.getByTestId('question-card')).toHaveCount(0);
  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');
  await expect(phase(page)).toHaveText('Idle');
  // The agent was told nothing: the answer would have come as a message, and the chat has none.
  await expect(page.getByText('sloppod: the user dismissed your question')).toHaveCount(0);
});

test('on a narrow screen the folded panel says how many highlights wait', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await createSession(page, 'Highlight: One thing for you | Another thing for you');
  await expect(page.getByTestId('waiting-summary-text')).toHaveText('2 highlights for you');
});

test('questions asked in separate requests show one after another', async ({ page }) => {
  await createSession(page, 'Use ask_user twice');
  await expect(questionCard(page)).toHaveCount(1);
  await expect(questionCard(page)).toContainText('Which backoff strategy should reconnects use?');
  await expect(questionCard(page).getByTestId('questions-queued')).toHaveText('One more request after this');
  // A request with one question has no steps.
  await expect(questionCard(page).getByTestId('question-step')).toHaveCount(0);

  await questionField(page, 'question_0').getByRole('radio', { name: /Fixed/ }).check();
  await questionCard(page).getByRole('button', { name: 'Answer' }).click();
  await expect(questionCard(page)).toContainText('Which tests should I add?');
  await expect(questionCard(page).getByTestId('questions-queued')).toHaveCount(0);
  await expect(questionField(page, 'question_0').getByRole('checkbox', { name: /Unit/ })).not.toBeChecked();
  await questionField(page, 'question_0').getByRole('checkbox', { name: /Unit/ }).check();
  await questionCard(page).getByRole('button', { name: 'Answer' }).click();
  await expect(questionCard(page)).toHaveCount(0);
  await expect(agentMessages(page).filter({ hasText: 'Thanks, going with that. Backoff: Fixed.' })).toBeVisible();
  await expect(agentMessages(page).filter({ hasText: 'Thanks, going with that. Tests: Unit.' })).toBeVisible();
});

test('an answer given while the agent is replying waits for the reply, and outlives a stop', async ({ page }) => {
  await createSession(page, 'Ask me, then keep talking');
  await expect(questionCard(page)).toBeVisible();
  await questionField(page, 'question_0').getByRole('radio', { name: /Fixed/ }).check();
  await questionCard(page).getByRole('button', { name: 'Answer' }).click();
  await expect(questionCard(page)).toHaveCount(0);
  // It isn't shown in the middle of the agent's reply, which would split it.
  await expect(page.getByTestId('system-notice')).toHaveCount(0);

  // Stopped before the agent took it, the answer still reaches it.
  await page.getByRole('textbox', { name: 'Prompt' }).focus();
  await page.keyboard.press('Escape');
  await expect(page.getByTestId('system-notice')).toContainText('the user answered your question');
  await expect(agentMessages(page).filter({ hasText: 'Thanks, going with that. Backoff: Fixed.' })).toBeVisible();
});

test('shows an open question again after a reload', async ({ page }) => {
  await createSession(page, 'Ask me something, then reload');
  await expect(questionCard(page)).toBeVisible();
  await page.reload();
  await expect(questionCard(page)).toBeVisible();
  await questionField(page, 'question_0_custom').getByRole('textbox').fill('Whatever you think is best');
  await questionCard(page).getByRole('button', { name: 'Next' }).click();
  await questionCard(page).getByRole('button', { name: 'Answer' }).click();
  await expect(agentMessages(page).filter({ hasText: 'Backoff: Whatever you think is best.' })).toBeVisible();
});

test('two identical questions asked separately stay two questions', async ({ page }) => {
  await createSession(page, 'Use ask_user to ask me the same question twice');
  await expect(questionCard(page)).toBeVisible();
  await expect(questionCard(page).getByTestId('questions-queued')).toHaveText('One more request after this');
  await questionField(page, 'question_0').getByRole('radio', { name: /Exponential/ }).check();
  await questionCard(page).getByRole('button', { name: 'Answer' }).click();
  // The second one is still open, with nothing of the first one's answer in it.
  await expect(questionCard(page)).toBeVisible();
  await expect(questionCard(page).getByTestId('questions-queued')).toHaveCount(0);
  await expect(questionField(page, 'question_0').getByRole('radio', { name: /Exponential/ })).not.toBeChecked();
  await questionField(page, 'question_0').getByRole('radio', { name: /Fixed/ }).check();
  await questionCard(page).getByRole('button', { name: 'Answer' }).click();
  await expect(questionCard(page)).toBeHidden();
});

test('a half-given answer stays through a dropped connection and a reload', async ({ page }) => {
  // Every connection goes through here, so the test can drop one, as a flaky network would.
  const sockets: WebSocketRoute[] = [];
  await page.routeWebSocket(/\/acp$/, (socket) => {
    socket.connectToServer();
    sockets.push(socket);
  });
  await createSession(page, 'Please ask me how to do it');
  await expect(questionCard(page)).toBeVisible();
  await questionField(page, 'question_0').getByRole('radio', { name: /Exponential/ }).check();
  await questionCard(page).getByRole('button', { name: 'Next' }).click();
  await questionField(page, 'question_1').getByRole('checkbox', { name: /Unit/ }).check();

  // While the connection is down the question stays, greyed out, and can't be answered.
  await sockets.at(-1)!.close();
  await expect(questionCard(page)).toHaveAttribute('data-stale', 'true');
  await expect(questionCard(page).getByTestId('question-reconnecting')).toBeVisible();
  await expect(questionCard(page).getByRole('button', { name: 'Answer' })).toBeDisabled();
  // Once it's back, it's the same question, answer and step included.
  await expect(questionCard(page)).not.toHaveAttribute('data-stale', 'true');
  await expect(questionCard(page).getByTestId('question-step')).toHaveText('Question 2 of 2');
  await expect(questionField(page, 'question_1').getByRole('checkbox', { name: /Unit/ })).toBeChecked();

  await page.reload();
  await expect(questionCard(page).getByTestId('question-step')).toHaveText('Question 2 of 2');
  await expect(questionField(page, 'question_1').getByRole('checkbox', { name: /Unit/ })).toBeChecked();
  await questionCard(page).getByRole('button', { name: 'Back' }).click();
  await expect(questionField(page, 'question_0').getByRole('radio', { name: /Exponential/ })).toBeChecked();
  await questionCard(page).getByRole('button', { name: 'Next' }).click();
  await questionCard(page).getByRole('button', { name: 'Answer' }).click();
  await expect(agentMessages(page).filter({ hasText: 'Backoff: Exponential.' })).toContainText('Tests: Unit.');

  // Answered, it's forgotten: nothing is kept for the next question.
  const kept = await page.evaluate(() => Object.keys(localStorage).filter((key) => key.startsWith('sloppod.answer.')));
  expect(kept).toEqual([]);
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

test("the next proposal waiting shows its own review history, not the last one's", async ({ page }) => {
  const id = await createSession(page, 'Propose a reviewed PR for sloppod');
  const put = await page.request.put(`/api/v1/sessions/${id}/review`, { data: { review: 'required' } });
  expect(put.status()).toBe(200);
  await finishTurn(page);
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('Propose another reviewed PR for sloppod');
  await box.press('Enter');
  await permissionCard(page).getByRole('button', { name: 'Allow', exact: true }).click();
  await expect(page.getByTestId('proposals-bar')).toContainText('2 proposals to review');

  await page.getByTestId('proposals-bar').getByRole('button', { name: 'Review' }).click();
  const history = page.getByTestId('review-history');
  await history.getByText('Review by Codex').click();
  const rounds = history.getByTestId('review-round');
  await expect(rounds).toHaveCount(2);
  const first = await rounds.first().textContent();
  await page.getByRole('button', { name: 'Approve', exact: true }).click();

  // The next one opens in its place with its history folded, and its own rounds once opened.
  await expect(page.getByTestId('proposal-previous')).toBeVisible();
  await expect(rounds).toHaveCount(0);
  await history.getByText('Review by Codex').click();
  await expect(rounds).toHaveCount(2);
  expect(await rounds.first().textContent()).not.toBe(first);
});

test('deciding a proposal moves on to the next one waiting', async ({ page }) => {
  await createSession(page, 'Propose a PR for sloppod');
  await finishTurn(page);
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('File issues for sloppod');
  await box.press('Enter');
  await expect(page.getByTestId('proposals-bar')).toContainText('2 proposals to review');

  // The bar's Review opens the oldest one straight away; the list is a tap away for picking another.
  await page.getByTestId('proposals-bar').getByRole('button', { name: 'List' }).click();
  await expect(page.getByRole('dialog').getByTestId('proposal-card')).toHaveCount(2);
  await page.keyboard.press('Escape');
  await page.getByTestId('proposals-bar').getByRole('button', { name: 'Review' }).click();
  const detail = page.getByTestId('proposal-detail');
  await expect(detail.getByTestId('proposal-title')).toHaveText('Reconnect the ACP socket with exponential backoff');
  await page.getByRole('button', { name: 'Approve', exact: true }).click();

  // The oldest one decided, the issues show in its place, saying what became of the pull request.
  await expect(detail.getByTestId('issue-draft')).toHaveCount(3);
  const previous = page.getByTestId('proposal-previous');
  await expect(previous).toContainText(/(Approved|Opened): pull request/);
  await expect(previous).toContainText('Reconnect the ACP socket with exponential backoff');
  await page.getByRole('button', { name: 'File 3 issues' }).click();

  await expect(detail).toBeHidden();
  await expect(page.getByTestId('proposal-card')).toHaveCount(0);
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

test('a session that needs the user moves above the ones that do not, and drops back once it does not', async ({ page }) => {
  await page.setViewportSize({ width: 1400, height: 900 });
  const older = 'Say something, the older session in the order';
  const newer = 'Say something, the newer session in the order';
  const olderId = await createSession(page, older);
  await finishTurn(page);
  await createSession(page, newer);
  await finishTurn(page);
  const rows = page.getByRole('navigation', { name: 'Sessions' }).getByTestId('session-row');
  // Where each is in the list, as the sessions of other tests come and go around them.
  const above = async (first: string, second: string) => {
    const texts = await rows.evaluateAll((nodes) => nodes.map((node) => node.textContent ?? ''));
    return texts.findIndex((text) => text.includes(first)) < texts.findIndex((text) => text.includes(second));
  };
  await expect.poll(() => above('the newer session', 'the older session')).toBe(true);

  // Something opens in the older one (nothing else happens in it): it goes above the newer one, without a reload.
  const highlight = await (await page.request.post(`/_mock/highlight?session=${olderId}&text=Look%20at%20this`)).json();
  await expect.poll(() => above('the older session', 'the newer session')).toBe(true);
  await expect(page.getByTestId('session-group').filter({ hasText: 'Waiting on you' })).toHaveCount(1);

  // The user has seen it: it drops back to where its last activity puts it.
  await page.request.post(`/api/v1/sessions/${olderId}/highlights/${highlight.id}/seen`, { data: {} });
  await expect.poll(() => above('the newer session', 'the older session')).toBe(true);
});

test('a folded working group can always be unfolded, even once no other group is left', async ({ page }) => {
  await page.setViewportSize({ width: 1400, height: 900 });
  const phases = async () => {
    const sessions: { id: string; phase: string; parent_id?: string }[] = await (await page.request.get('/api/v1/sessions')).json();
    return sessions;
  };
  const setPhase = (id: string, phase: string) => page.request.post(`/_mock/phase?session=${id}&phase=${phase}`);
  const before = await phases();
  try {
    // Everything else parks, so only these are running; six at work and one idle make two groups.
    for (const session of before) await setPhase(session.id, 'parked');
    const ids: string[] = [];
    for (let i = 0; i < 7; i++) {
      const response = await page.request.post('/api/v1/sessions', { data: { prompt: `Start the tests in the background, number ${i} in a long list` } });
      ids.push((await response.json()).id);
    }
    await page.goto('/');
    // They finish their turns and sit idle (the background commands they started don't ask the user for anything).
    for (const id of ids) await expect.poll(async () => (await phases()).find((s) => s.id === id)?.phase).toBe('idle');
    for (const id of ids.slice(0, 6)) await setPhase(id, 'working');
    // The home page's own list.
    const nav = page.getByTestId('session-list');
    await expect(nav.getByTestId('session-group').filter({ hasText: 'Working (6)' })).toBeVisible();
    await nav.getByRole('button', { name: 'Fold' }).click();
    await expect(nav.getByTestId('session-row').filter({ hasText: 'in a long list' })).toHaveCount(1);

    // The last of the other group starts working: one group is left, and the way to unfold it is still there.
    await setPhase(ids[6], 'working');
    await expect(nav.getByRole('button', { name: 'Show' })).toBeVisible();
    await nav.getByRole('button', { name: 'Show' }).click();
    await expect(nav.getByTestId('session-row').filter({ hasText: 'in a long list' })).toHaveCount(7);
  } finally {
    for (const session of before) await setPhase(session.id, session.phase);
  }
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
  // A sub-session runs on the cheaper model unless the setting says otherwise.
  await expect(parentRow.getByTestId('sub-sessions').getByTestId('session-model')).toHaveText('sonnet');
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
  await page.request.post('/_mock/drop');
  await expect.poll(() => holding).toBe(true);

  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Delete session' }).click();
  await page.getByRole('button', { name: 'Delete', exact: true }).click();
  await expect(page).toHaveURL(/\/$/);
  const newer = page.waitForResponse((response) => response.url().endsWith('/api/v1/sessions') && response.status() === 200);
  await page.request.post('/_mock/drop');
  await newer;
  await expect(rows).toHaveCount(0);

  const late = page.waitForResponse((response) => response.url().endsWith('/api/v1/sessions'));
  release!();
  await late;
  // Checked once, not retried: a later fetch would hide a session the late list put back.
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
  await expect(grants.getByText("Claude's API, with the first of the owner's Claude accounts this grant names.")).toBeVisible();
  await expect(grants.getByText(/^bo0tzz\/sloppod: read, and push the session branch `sloppod\//)).toBeVisible();
});

async function addCredential(page: Page, kind: string, name: string) {
  const response = await page.request.post('/api/v1/credentials', { data: { kind, name, secret: `secret-${name}`, config: {} } });
  return ((await response.json()) as { id: string }).id;
}

async function openGrants(page: Page) {
  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Grants' }).click();
  return page.getByTestId('grants');
}

test("a model grant names the user's accounts, and the user picks which one it uses", async ({ page }) => {
  const work = await addCredential(page, 'claude', 'Work');
  const spare = await addCredential(page, 'claude', 'Spare');
  try {
    await createSession(page, 'Fix the build in bo0tzz/sloppod');
    const model = (await openGrants(page)).getByTestId('grant-model');
    // Made with all of the user's Claude accounts, the default first.
    const accounts = model.getByTestId('grant-account');
    await expect(accounts).toHaveCount(3);
    await expect(accounts.nth(0)).toContainText('Personal');
    await expect(accounts.nth(0)).toContainText('used first');

    // The third goes to the front, the others keep their order.
    const changing = page.waitForRequest((request) => request.method() === 'PATCH');
    await accounts.nth(2).getByRole('button', { name: 'Use first' }).click();
    const sent = (await changing).postDataJSON().credentials;
    expect(sent[0]).toBe(spare);
    expect(sent[2]).toBe(work);
    await expect(accounts.nth(0)).toContainText('Spare');
    await expect(accounts.nth(0)).toContainText('used first');
    await expect(accounts.nth(1)).toContainText('Personal');

    await accounts.nth(1).getByRole('button', { name: 'Remove' }).click();
    await expect(accounts).toHaveCount(2);
    await accounts.nth(1).getByRole('button', { name: 'Remove' }).click();
    await expect(accounts).toHaveCount(1);
    await expect(accounts.nth(0)).toContainText('Spare');
    // The last account can't be removed, and one removed can be added back.
    await expect(accounts.getByRole('button', { name: 'Remove' })).toHaveCount(0);
    await model.getByRole('combobox', { name: 'Add an account' }).click();
    await page.getByRole('option', { name: 'Personal' }).click();
    await expect(accounts).toHaveCount(2);
    await expect(accounts.nth(1)).toContainText('Personal');
  } finally {
    for (const id of [work, spare]) await page.request.delete(`/api/v1/credentials/${id}`);
  }
});

test('a model grant whose accounts were all deleted is given another', async ({ page }) => {
  const temporary = await addCredential(page, 'claude', 'Temporary');
  try {
    const id = await createSession(page, 'Fix the build in bo0tzz/sloppod');
    const session = await (await page.request.get(`/api/v1/sessions/${id}`)).json();
    const grant = session.grants.find((g: { capability: string }) => g.capability === 'model');
    await page.request.patch(`/api/v1/sessions/${id}/grants/${grant.id}`, { data: { credentials: [temporary] } });
  } finally {
    await page.request.delete(`/api/v1/credentials/${temporary}`);
  }
  const model = (await openGrants(page)).getByTestId('grant-model');
  const accounts = model.getByTestId('grant-account');
  await expect(accounts).toHaveCount(1);
  await expect(accounts.nth(0)).toContainText('A deleted account');
  const changing = page.waitForRequest((request) => request.method() === 'PATCH');
  await model.getByRole('combobox', { name: 'Add an account' }).click();
  await page.getByRole('option', { name: 'Personal' }).click();
  expect((await changing).postDataJSON().credentials).toHaveLength(1);
  await expect(accounts).toHaveCount(1);
  await expect(accounts.nth(0)).toContainText('Personal');
  await expect(accounts.nth(0)).toContainText('used first');
});

test('a grant made before the user had an account of its kind is given one later', async ({ page }) => {
  await createSession(page, 'Fix the build in bo0tzz/sloppod');
  let grants = await openGrants(page);
  const add = grants.getByTestId('add-grant');
  await add.getByRole('combobox', { name: 'Grant' }).click();
  await page.getByRole('option', { name: 'codex' }).click();
  await add.getByRole('button', { name: 'Grant' }).click();
  await expect(grants.getByTestId('grant-codex').getByTestId('grant-account-default')).toBeVisible();
  await page.keyboard.press('Escape');
  const login = await addCredential(page, 'chatgpt', 'Reviews');
  try {
    grants = await openGrants(page);
    const codex = grants.getByTestId('grant-codex');
    await codex.getByRole('combobox', { name: 'Add an account' }).click();
    await page.getByRole('option', { name: 'Reviews' }).click();
    await expect(codex.getByTestId('grant-account')).toContainText('Reviews');
    await expect(codex.getByTestId('grant-account-default')).toHaveCount(0);
  } finally {
    await page.request.delete(`/api/v1/credentials/${login}`);
  }
});

test('grants, narrows, times and revokes the grants of a running session', async ({ page }) => {
  await createSession(page, 'Fix the build in bo0tzz/sloppod');
  // The agent's first turn ends before anything is granted: a notice reaches a turn that's still running only when it
  // takes it, which a turn waiting on a permission request doesn't, so whether the last one showed depended on timing.
  await finishTurn(page);
  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Grants' }).click();
  const grants = page.getByTestId('grants');

  const add = grants.getByTestId('add-grant');
  await add.getByRole('combobox', { name: 'Grant' }).click();
  await page.getByRole('option', { name: 'codex' }).click();
  await add.getByRole('combobox', { name: 'Runs out' }).click();
  await page.getByRole('option', { name: 'In an hour' }).click();
  await add.getByRole('button', { name: 'Grant' }).click();
  const codex = grants.getByTestId('grant-codex');
  await expect(codex).toContainText("Codex's API");
  await expect(codex).toContainText('Runs out');
  // One that doesn't run out says so, rather than saying nothing.
  await expect(grants.getByTestId('grant-packages')).toContainText("Doesn't run out");
  await expect(codex).not.toContainText("Doesn't run out");

  const repo = grants.getByTestId('grant-repo');
  await repo.getByRole('button', { name: 'Make read only' }).click();
  await expect(repo).toContainText('bo0tzz/sloppod: read only.');
  await expect(repo.getByRole('button', { name: 'Make read only' })).toHaveCount(0);

  await grants.getByTestId('grant-packages').getByRole('button', { name: 'Revoke' }).click();
  await expect(grants.getByTestId('grant-packages')).toHaveCount(0);
  await expect(page.getByText("sloppod: the user revoked this session's grant packages.")).toBeVisible();
});

test('grants a session one of the work graphs at a level', async ({ page }) => {
  await createSession(page, 'Fix the build in bo0tzz/sloppod');
  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Grants' }).click();
  const grants = page.getByTestId('grants');
  const add = grants.getByTestId('add-grant');
  await add.getByRole('combobox', { name: 'Grant' }).click();
  await page.getByRole('option', { name: 'work' }).click();
  // A graph has to be chosen first.
  await expect(add.getByRole('button', { name: 'Grant' })).toBeDisabled();
  await add.getByRole('combobox', { name: 'Graph' }).click();
  await page.getByRole('option', { name: 'sloppod' }).click();
  await add.getByRole('combobox', { name: 'Level' }).click();
  await page.getByRole('option', { name: 'Plan' }).click();
  await add.getByRole('button', { name: 'Grant' }).click();
  await expect(grants.getByTestId('grant-work')).toContainText('The work graph sloppod');
  await expect(grants.getByTestId('grant-work')).toContainText('propose plan changes');
  // The granted graph isn't offered again; work is, only while other graphs (from other tests) are left.
  await add.getByRole('combobox', { name: 'Grant' }).click();
  const work = page.getByRole('option', { name: 'work' });
  if ((await work.count()) > 0) {
    await work.click();
    await add.getByRole('combobox', { name: 'Graph' }).click();
    await expect(page.getByRole('option', { name: 'sloppod', exact: true })).toHaveCount(0);
  }
});

test('a grant gets a new expiry, and a refused change can be tried again', async ({ page }) => {
  await createSession(page, 'Fix the build in bo0tzz/sloppod');
  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Grants' }).click();
  const model = page.getByTestId('grants').getByTestId('grant-model');

  await model.getByRole('combobox').click();
  await page.getByRole('option', { name: 'In a day' }).click();
  await expect(model).toContainText('Runs out');

  let refused = false;
  await page.route('**/api/v1/sessions/*/grants/*', async (route) => {
    if (refused) return route.fallback();
    refused = true;
    await route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ error: 'try again' }) });
  });
  await model.getByRole('combobox').click();
  await page.getByRole('option', { name: 'Never' }).click();
  await expect(page.getByTestId('grants').getByText('try again')).toBeVisible();
  await expect(model).toContainText('Runs out');
  await expect(model.getByRole('combobox')).toContainText('Change when it runs out');

  await model.getByRole('combobox').click();
  await page.getByRole('option', { name: 'Never' }).click();
  await expect(model).not.toContainText('Runs out');
});

test("adds a capability of one's own, grants it to a session, and deletes it", async ({ page }) => {
  await page.goto('/settings');
  const section = page.getByTestId('capabilities');
  await expect(section).toContainText('No capability of your own yet.');
  const fill = async (name: string, url = 'https://logs.example.com/mcp') => {
    await section.getByRole('button', { name: 'Add capability' }).click();
    await section.getByLabel('Name', { exact: true }).fill(name);
    await section.getByLabel('Description', { exact: true }).fill("Search the cluster's logs");
    await section.getByLabel('MCP server URL').fill(url);
    await section.getByRole('button', { name: 'Save' }).click();
  };

  // A name a built-in has, and one with credentials in its URL, are refused with the runtime's reason.
  await fill('docs');
  await expect(section.getByText('docs is a name sloppod uses')).toBeVisible();
  await section.getByLabel('Name', { exact: true }).fill('victorialogs');
  await section.getByLabel('MCP server URL').fill('https://user:pass@logs.example.com/mcp');
  await section.getByRole('button', { name: 'Save' }).click();
  await expect(section.getByText('must not have credentials')).toBeVisible();
  await section.getByLabel('MCP server URL').fill('https://logs.example.com/mcp');
  await section.getByRole('button', { name: 'Save' }).click();
  const listed = section.getByTestId('capability');
  await expect(listed).toHaveCount(1);
  await expect(listed).toContainText('victorialogs');
  await expect(listed).toContainText("Search the cluster's logs");
  await expect(listed).toContainText('https://logs.example.com/mcp');

  // A name that is taken is refused.
  await fill('victorialogs');
  await expect(section.getByText('already have a capability called victorialogs')).toBeVisible();
  await section.getByRole('button', { name: 'Cancel' }).click();

  // It's offered when granting, after the built-ins, and its description shows like any grant's.
  await createSession(page, 'Fix the build in bo0tzz/sloppod');
  await page.getByRole('button', { name: 'Session actions' }).click();
  await page.getByRole('menuitem', { name: 'Grants' }).click();
  const grants = page.getByTestId('grants');
  await grants.getByTestId('add-grant').getByRole('combobox', { name: 'Grant' }).click();
  await page.getByRole('option', { name: 'victorialogs' }).click();
  await grants.getByTestId('add-grant').getByRole('button', { name: 'Grant' }).click();
  const granted = grants.getByTestId('grant-victorialogs');
  await expect(granted).toContainText("Search the cluster's logs");

  // Deleting it takes it out of the list, with a confirmation.
  await page.goto('/settings');
  await section.getByRole('button', { name: 'Delete victorialogs' }).click();
  await page.getByRole('button', { name: 'Delete', exact: true }).click();
  await expect(section.getByTestId('capability')).toHaveCount(0);
  await expect(section).toContainText('No capability of your own yet.');
});

test('a token for a capability is added in settings and chosen for the capability', async ({ page }) => {
  await page.goto('/settings');
  const tokens = page.getByTestId('credentials-token');
  await tokens.getByRole('button', { name: 'Add token' }).click();
  await tokens.getByLabel('Name', { exact: true }).fill('logs key');
  await tokens.getByLabel('Header').fill('X-API-Key');
  await tokens.getByLabel('Token', { exact: true }).fill('secret');
  await tokens.getByRole('button', { name: 'Save' }).click();
  await expect(tokens.getByTestId('credential')).toContainText('logs key');
  // As the runtime keeps it: header names are lowercased.
  await expect(tokens.getByTestId('credential')).toContainText('Header: x-api-key');

  const section = page.getByTestId('capabilities');
  await section.getByRole('button', { name: 'Add capability' }).click();
  await section.getByLabel('Name', { exact: true }).fill('metrics');
  await section.getByLabel('Description', { exact: true }).fill('Query the metrics');
  await section.getByLabel('MCP server URL').fill('https://metrics.example.com/mcp');
  await section.getByRole('combobox', { name: 'Token' }).click();
  await page.getByRole('option', { name: 'logs key' }).click();
  await section.getByRole('button', { name: 'Save' }).click();
  await expect(section.getByTestId('capability')).toContainText('Token: logs key');

  // Deleting the token says what it does to the capability that sends it, and detaches it there, as the runtime's
  // foreign key does.
  await tokens.getByRole('button', { name: 'Delete logs key' }).click();
  await expect(page.getByText('stop sending it at once, also to sessions that already have them')).toBeVisible();
  await page.getByRole('button', { name: 'Delete', exact: true }).click();
  await expect(tokens.getByTestId('credential')).toHaveCount(0);
  await expect(section.getByTestId('capability')).toContainText('metrics');
  await expect(section.getByTestId('capability')).not.toContainText('Token:');

  await section.getByRole('button', { name: 'Delete metrics' }).click();
  await page.getByRole('button', { name: 'Delete', exact: true }).click();
  await expect(section.getByTestId('capability')).toHaveCount(0);

  // A header name is letters, digits and dashes.
  await tokens.getByRole('button', { name: 'Add token' }).click();
  await tokens.getByLabel('Name', { exact: true }).fill('odd');
  await tokens.getByLabel('Header').fill('X API Key');
  await tokens.getByLabel('Token', { exact: true }).fill('secret');
  await tokens.getByRole('button', { name: 'Save' }).click();
  await expect(tokens.getByText('a header name is letters, digits and dashes')).toBeVisible();
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
  // A rewrite shows against what the issue says now.
  await expect(updates.nth(2).getByTestId('issue-update-title')).toHaveText(
    'Title: Startup failures are only in the logs → Startup failures show in the UI',
  );
  const bodyDiff = updates.nth(2).getByTestId('diff');
  await expect(bodyDiff).toContainText('Body of #59');
  await expect(bodyDiff).toContainText('+1');
  await expect(bodyDiff).toContainText('−1');
  await expect(bodyDiff.getByText('A pod that fails to start says nothing.')).toBeVisible();
  await expect(bodyDiff.getByText('A pod that fails to start says why in the UI.')).toBeVisible();

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

test('reviews and approves updates to open pull requests', async ({ page }) => {
  await createSession(page, 'Tidy the pull requests in sloppod');
  const card = page.getByTestId('proposal-card');
  await expect(card).toHaveAttribute('data-kind', 'github.pr.update');
  await expect(card).toContainText('Pull request updates');
  await card.getByRole('button', { name: 'Review' }).click();
  const updates = page.getByTestId('issue-update');
  await expect(updates).toHaveCount(2);
  await expect(updates.nth(0)).toContainText('#96');
  await expect(updates.nth(0)).toContainText('· close');
  await expect(updates.nth(0)).not.toContainText('close as');
  await expect(updates.nth(0)).toContainText('Remove labels: wip');
  await expect(updates.nth(1)).toContainText('mark ready for review');
  // A rewrite shows against what the pull request says now.
  await expect(updates.nth(1).getByTestId('issue-update-title')).toHaveText('Title: WIP: pod failures → Show why a pod fails to start');
  const bodyDiff = updates.nth(1).getByTestId('diff');
  await expect(bodyDiff).toContainText('Body of #97');
  await expect(bodyDiff.getByText('Closes #59.')).toBeVisible();

  // Leave the close out, and make the other.
  await updates.nth(0).getByRole('checkbox', { name: 'Make this update' }).click();
  await page.getByRole('button', { name: 'Make 1 update' }).click();
  await expect(page.getByTestId('system-notice').last()).toContainText('Your pull request updates were approved and made');
  const proposals: { kind: string; payload: { updates: { number: number; ready?: boolean }[] } }[] = await (
    await page.request.get('/api/v1/proposals')
  ).json();
  const made = proposals.filter((p) => p.kind === 'github.pr.update').at(-1)!;
  expect(made.payload.updates).toEqual([expect.objectContaining({ number: 97, ready: true })]);
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

// Two accounts' usage for the mock to report: a Claude one with three windows, and a ChatGPT one at its limit.
async function mockQuotas(page: Page) {
  const inTwoHours = new Date(Date.now() + 2 * 3600_000).toISOString();
  const quota = (kind: string, name: string, limited: boolean, windows: [string, number, string][]) => ({
    credential_id: `${kind}-${name}`,
    kind,
    name,
    limited,
    resets_at: limited ? inTwoHours : null,
    windows: windows.map(([window, utilization, status]) => ({ window, utilization, resets_at: inTwoHours, status })),
    observed_at: new Date().toISOString(),
  });
  await page.request.post('/_mock/quotas', {
    data: [
      quota('claude', 'work', false, [
        ['seven_day', 0.95, 'warning'],
        ['1_day', 0.4, 'allowed'],
        ['five_hour', 0.2, 'allowed'],
      ]),
      quota('chatgpt', 'reviews', true, [['five_hour', 1, 'rejected']]),
    ],
  });
}

test('a phone has no sidebar, and the home page shows the usage meters instead', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await mockQuotas(page);
  try {
    await page.goto('/');
    await expect(page.getByTestId('sidebar')).toHaveCount(0);
    const meters = page.getByTestId('quota');
    await expect(meters).toHaveCount(2);
    await expect(meters.filter({ hasText: 'work' }).getByTestId('quota-window')).toHaveCount(3);
    await expect(meters.filter({ hasText: 'reviews' }).getByTestId('quota-limited')).toContainText('Limit reached');
    await expect(meters.first()).toBeInViewport();
    // Nothing runs off the screen's width.
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
    expect(overflow).toBeLessThanOrEqual(0);
  } finally {
    await page.request.post('/_mock/quotas', { data: [] });
  }
});

test('the sidebar shows every usage window of each account, by its name', async ({ page }) => {
  await mockQuotas(page);
  try {
    await page.goto('/settings');
    const meters = page.getByTestId('sidebar').getByTestId('quota');
    await expect(meters).toHaveCount(2);
    // Each account by its name, with every window it reports, shortest first.
    const work = meters.filter({ hasText: 'work' });
    await expect(work).toContainText('Claude');
    const workWindows = work.getByTestId('quota-window');
    await expect(workWindows).toHaveCount(3);
    await expect(workWindows.nth(0)).toHaveAttribute('data-window', 'five_hour');
    await expect(workWindows.nth(0)).toContainText('5-hour');
    await expect(workWindows.nth(0)).toContainText('20%, resets in 2 hours');
    await expect(workWindows.nth(0)).toHaveAttribute('data-level', 'normal');
    await expect(workWindows.nth(1)).toHaveAttribute('data-window', '1_day');
    await expect(workWindows.nth(1)).toContainText('Day');
    await expect(workWindows.nth(2)).toContainText('Week');
    await expect(workWindows.nth(2)).toContainText('95%');
    await expect(workWindows.nth(2)).toHaveAttribute('data-level', 'warning');
    await expect(work.getByTestId('quota-limited')).toHaveCount(0);
    const reviews = meters.filter({ hasText: 'reviews' });
    await expect(reviews).toContainText('ChatGPT');
    await expect(reviews.getByTestId('quota-limited')).toContainText('Limit reached, resets in 2 hours');
    await expect(reviews.getByTestId('quota-window')).toHaveAttribute('data-level', 'danger');
  } finally {
    await page.request.post('/_mock/quotas', { data: [] });
  }
});

/** Has the mock report one Claude account's 5-hour window at `percent`, or no quotas at all. */
const setWorkQuota = (page: Page, percent: number | null) =>
  page.request.post('/_mock/quotas', {
    data:
      percent === null
        ? []
        : [
            {
              credential_id: 'claude-work',
              kind: 'claude',
              name: 'work',
              limited: false,
              resets_at: null,
              windows: [{ window: 'five_hour', utilization: percent / 100, resets_at: null, status: 'allowed' }],
              observed_at: new Date().toISOString(),
            },
          ],
  });

test('the usage meters follow quota changes as they happen, without a reload', async ({ page }) => {
  const set = (percent: number | null) => setWorkQuota(page, percent);
  await set(20);
  try {
    await page.goto('/settings');
    const meter = page.getByTestId('sidebar').getByTestId('quota-window');
    await expect(meter).toContainText('20%');
    await set(30);
    await expect(meter).toContainText('30%');

    // Across switching to a session and on to another.
    await createSession(page, 'Meters while switching sessions');
    await set(40);
    await expect(meter).toContainText('40%');
    await createSession(page, 'Another session for the meters');
    await set(50);
    await expect(meter).toContainText('50%');

    // After the tab was hidden and shown again.
    const visibility = (state: 'hidden' | 'visible') =>
      page.evaluate((state) => {
        Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => state });
        document.dispatchEvent(new Event('visibilitychange'));
      }, state);
    await visibility('hidden');
    await set(60);
    await visibility('visible');
    await expect(meter).toContainText('60%');

    // A change while the connection is down shows once it's back.
    await page.request.post('/_mock/drop?for=1500');
    await set(70);
    await expect(meter).toContainText('70%');

    // An account that's gone takes its meter with it.
    await set(null);
    await expect(page.getByTestId('sidebar').getByTestId('quota')).toHaveCount(0);
  } finally {
    await set(null);
  }
});

test('the page keeps one socket for the list, the meters and the open session, across a reconnect', async ({ page }) => {
  await page.setViewportSize({ width: 1400, height: 900 });
  const sockets: PageSocket[] = [];
  page.on('websocket', (socket) => sockets.push(socket));
  await setWorkQuota(page, 20);
  try {
    // Its own title each run: a retry, or a repeat, against the same mock finds its own row, not an earlier run's.
    const prompt = `Propose a PR for sloppod, over one socket ${Date.now().toString(36)}`;
    const id = await createSession(page, prompt);
    await finishTurn(page);
    const sidebar = page.getByTestId('sidebar');
    const row = page.getByRole('navigation', { name: 'Sessions' }).getByTestId('session-row').filter({ hasText: prompt });
    await expect(row.getByTestId('phase')).toHaveText('Waiting on you');
    const meter = sidebar.getByTestId('quota-window');
    await expect(meter).toContainText('20%');

    // Moving about the app, a session after another and the settings, opens no other socket.
    await page.getByRole('link', { name: 'New session' }).click();
    // Until the new-session page is up, the session's own composer is the "Prompt" box: typing then would send there.
    await expect(page).toHaveURL(/\/new$/);
    await expect(page.getByTestId('session-title')).toHaveText('New session');
    const box = page.getByRole('textbox', { name: 'Prompt' });
    await box.fill('Say something over the one socket');
    await box.press('Enter');
    await expect(page).toHaveURL(/\/sessions\/[0-9a-f-]+$/);
    await finishTurn(page);
    await page.getByRole('link', { name: 'Settings' }).first().click();
    await expect(page).toHaveURL(/\/settings$/);
    await row.click();
    await expect(page).toHaveURL(new RegExp(id));
    expect(sockets).toHaveLength(1);
    expect(sockets[0].isClosed()).toBe(false);

    // Down for a while: what changes meanwhile is read once it's back, with no timer to read it.
    await page.request.post('/_mock/drop?for=1500');
    const proposals: { id: string; status: string }[] = await (await page.request.get(`/api/v1/proposals?session=${id}`)).json();
    await page.request.post(`/api/v1/proposals/${proposals.find((p) => p.status === 'pending')!.id}/reject`, { data: {} });
    await setWorkQuota(page, 70);
    await expect(meter).toContainText('70%');
    await expect(row.getByTestId('phase')).toHaveText('Idle');
    // Refused attempts show as sockets too: only one is open.
    const open = () => sockets.filter((socket) => !socket.isClosed());
    expect(open()).toHaveLength(1);

    // And what changes after is pushed on the new socket: the meter, and the session shown.
    await setWorkQuota(page, 80);
    await expect(meter).toContainText('80%');
    const again = page.getByRole('textbox', { name: 'Prompt' });
    await again.fill('Propose a PR for sloppod, once more over the new socket');
    await again.press('Enter');
    await finishTurn(page);
    expect(open()).toHaveLength(1);
  } finally {
    await setWorkQuota(page, null);
  }
});

test('a session opened while a reconnect is still fetching the rest loads once those are through', async ({ page }) => {
  await page.setViewportSize({ width: 1400, height: 900 });
  const first = 'Say something, the first of two on one socket';
  const second = 'Say something, the second of two on one socket';
  await createSession(page, first);
  await finishTurn(page);
  await createSession(page, second);
  await finishTurn(page);
  const nav = page.getByRole('navigation', { name: 'Sessions' });

  // The reconnect's fetch of the quotas is held, so the connection is still coming up when the other session opens.
  let hold = false;
  let release!: () => void;
  const released = new Promise<void>((resolve) => (release = resolve));
  let fetching!: () => void;
  const started = new Promise<void>((resolve) => (fetching = resolve));
  await page.route('**/api/v1/quotas', async (route) => {
    if (hold) {
      fetching();
      await released;
    }
    await route.fallback();
  });
  hold = true;
  await page.request.post('/_mock/drop');
  await started;
  await nav.getByTestId('session-row').filter({ hasText: first }).click();
  await expect(page).toHaveURL(/\/sessions\/[0-9a-f-]+$/);
  release();
  await expect(agentMessages(page).filter({ hasText: FINAL })).toBeVisible();
  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');
  await expect(page.getByTestId('reconnecting')).toBeHidden();
});

test('a session left while its load is on its way is closed once the load is through', async ({ page }) => {
  await page.setViewportSize({ width: 1400, height: 900 });
  const first = 'Say something, left while it reloads';
  const second = 'Say something, opened while it reloads';
  const firstId = await createSession(page, first);
  await finishTurn(page);
  await createSession(page, second);
  await finishTurn(page);
  const nav = page.getByRole('navigation', { name: 'Sessions' });
  await nav.getByTestId('session-row').filter({ hasText: first }).click();
  await expect(page).toHaveURL(new RegExp(firstId));
  await expect(agentMessages(page).filter({ hasText: FINAL })).toBeVisible();

  // What the page sends and gets on the sockets from here on.
  const frames: { dir: 'sent' | 'received'; message: { id?: number; method?: string; params?: { sessionId?: string } } }[] = [];
  page.on('websocket', (socket) => {
    socket.on('framesent', ({ payload }) => frames.push({ dir: 'sent', message: JSON.parse(String(payload)) }));
    socket.on('framereceived', ({ payload }) => frames.push({ dir: 'received', message: JSON.parse(String(payload)) }));
  });
  // The socket comes back at once, but the runtime takes 3 s to answer the load; the page moves on meanwhile.
  await page.request.post('/_mock/drop?slowload=3000');
  await expect
    .poll(() => frames.some((frame) => frame.dir === 'sent' && frame.message.method === 'session/load' && frame.message.params?.sessionId === firstId))
    .toBe(true);
  await nav.getByTestId('session-row').filter({ hasText: second }).click();
  await expect(page).not.toHaveURL(new RegExp(firstId));

  // The close goes after the load has been answered, not before, when the runtime wouldn't have the subscription yet.
  await expect.poll(() => frames.some((frame) => frame.dir === 'sent' && frame.message.method === 'session/close')).toBe(true);
  const load = frames.find((frame) => frame.dir === 'sent' && frame.message.method === 'session/load' && frame.message.params?.sessionId === firstId)!;
  const answered = frames.findIndex((frame) => frame.dir === 'received' && frame.message.id === load.message.id && frame.message.method === undefined);
  const closed = frames.findIndex((frame) => frame.dir === 'sent' && frame.message.method === 'session/close' && frame.message.params?.sessionId === firstId);
  expect(answered).toBeGreaterThan(-1);
  expect(closed).toBeGreaterThan(answered);
});

test('a session left and entered again while its load is on its way is loaded once the first is closed', async ({ page }) => {
  await page.setViewportSize({ width: 1400, height: 900 });
  // Every load and close the page sends, from the first socket on.
  const sent: { method: string; sessionId: string }[] = [];
  page.on('websocket', (socket) =>
    socket.on('framesent', ({ payload }) => {
      const message = JSON.parse(String(payload));
      if (message.method === 'session/load' || message.method === 'session/close') {
        sent.push({ method: message.method, sessionId: message.params?.sessionId });
      }
    }),
  );
  const first = 'Say something, left and come back to';
  const second = 'Say something, the other one meanwhile';
  const firstId = await createSession(page, first);
  await finishTurn(page);
  await createSession(page, second);
  await finishTurn(page);
  const nav = page.getByRole('navigation', { name: 'Sessions' });
  const row = (prompt: string) => nav.getByTestId('session-row').filter({ hasText: prompt });

  const methods = () => sent.filter((frame) => frame.sessionId === firstId).map((frame) => frame.method);
  // Entering the first session so far: one load, and a close when it was left for the second.
  const before = methods().length;
  // On a connection that's up, the runtime takes 3 s to answer a load: the first session is opened, left and opened
  // again before it has.
  await page.request.post('/_mock/slow-loads?for=3000');
  await row(first).click();
  await expect.poll(() => methods().slice(before)).toEqual(['session/load']);
  await row(second).click();
  await expect(page).not.toHaveURL(new RegExp(firstId));
  await row(first).click();
  await expect(page).toHaveURL(new RegExp(firstId));

  // The second load waits for the first to be answered and closed, or the close would end the one that is shown.
  await expect(agentMessages(page).filter({ hasText: FINAL })).toBeVisible();
  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');
  expect(methods().slice(before)).toEqual(['session/load', 'session/close', 'session/load']);
  // And the session is live on the socket: a message runs a turn.
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('And once more, back again');
  await box.press('Enter');
  await expect(permissionCard(page)).toBeVisible();
});

test("a session whose load keeps failing doesn't disturb the list or the meters, or the socket", async ({ page }) => {
  await page.setViewportSize({ width: 1400, height: 900 });
  const sockets: PageSocket[] = [];
  page.on('websocket', (socket) => sockets.push(socket));
  await setWorkQuota(page, 20);
  try {
    const waiting = 'Propose a PR for sloppod, the list follows it';
    const waitingId = await createSession(page, waiting);
    await finishTurn(page);
    const other = 'Say something, its load will fail';
    await createSession(page, other);
    await finishTurn(page);
    const nav = page.getByRole('navigation', { name: 'Sessions' });
    const row = nav.getByTestId('session-row').filter({ hasText: waiting });
    await expect(row.getByTestId('phase')).toHaveText('Waiting on you');
    const meter = page.getByTestId('sidebar').getByTestId('quota-window');
    await expect(meter).toContainText('20%');

    // Each created session reloaded the page, so this is the socket it has now.
    const socketsBefore = sockets.length;
    // The runtime can't read a session for a while: opening it fails over and over, with backoff.
    await page.request.post('/_mock/fail-loads?for=4000');
    await nav.getByTestId('session-row').filter({ hasText: waiting }).click();
    await expect(page.getByText('the runtime could not read the session')).toBeVisible();

    // Meanwhile the list and the meters follow what the runtime pushes, on the same socket.
    await setWorkQuota(page, 70);
    await expect(meter).toContainText('70%');
    const proposals: { id: string; status: string }[] = await (await page.request.get(`/api/v1/proposals?session=${waitingId}`)).json();
    await page.request.post(`/api/v1/proposals/${proposals.find((p) => p.status === 'pending')!.id}/reject`, { data: {} });
    await expect(row.getByTestId('phase')).not.toHaveText('Waiting on you');

    // And the session loads on its own once the runtime can read it. Nothing reconnected on the way.
    await expect(page.getByText('the runtime could not read the session')).toBeHidden({ timeout: 20_000 });
    await expect(page.getByRole('textbox', { name: 'Prompt' })).toBeEnabled();
    expect(sockets).toHaveLength(socketsBefore);
    expect(sockets[socketsBefore - 1].isClosed()).toBe(false);
  } finally {
    await setWorkQuota(page, null);
  }
});

test("a consumer that failed early tries again though a later one is still loading", async ({ page }) => {
  await page.setViewportSize({ width: 1400, height: 900 });
  const sockets: PageSocket[] = [];
  page.on('websocket', (socket) => sockets.push(socket));
  await setWorkQuota(page, 20);
  try {
    await createSession(page, 'Say something, the meters fail first');
    await finishTurn(page);
    const meter = page.getByTestId('sidebar').getByTestId('quota-window');
    await expect(meter).toContainText('20%');

    // After the reconnect the quotas fail once; the session's load, which comes after, takes longer than the retry's
    // first delay, so the connection is still coming up when it's due.
    let calls = 0;
    await page.route('**/api/v1/quotas', (route) =>
      ++calls === 1 ? route.fulfill({ status: 500, json: { error: 'the quotas are unavailable' } }) : route.fallback(),
    );
    const socketsBefore = sockets.length;
    await page.request.post('/_mock/drop?slowload=3000');
    await expect.poll(() => calls).toBe(1);
    // The retry comes once the session's load is through and the connection is up, not before and not never.
    await expect.poll(() => calls, { timeout: 20_000 }).toBe(2);
    await setWorkQuota(page, 70);
    await expect(meter).toContainText('70%');
    // One reconnect, and nothing else.
    expect(sockets).toHaveLength(socketsBefore + 1);
  } finally {
    await setWorkQuota(page, null);
  }
});

test("a fetch left pending across a reconnect doesn't cancel the retry of the new connection's", async ({ page }) => {
  await page.setViewportSize({ width: 1400, height: 900 });
  await setWorkQuota(page, 20);
  try {
    // The page's first quotas fetch is held; the one after the reconnect fails; the held one is let go afterwards.
    let calls = 0;
    let release!: () => void;
    const released = new Promise<void>((resolve) => (release = resolve));
    await page.route('**/api/v1/quotas', async (route) => {
      const call = ++calls;
      if (call === 1) {
        const response = await route.fetch();
        await released;
        await route.fulfill({ response });
      } else if (call === 2) {
        await route.fulfill({ status: 500, json: { error: 'the quotas are unavailable' } });
      } else {
        await route.fallback();
      }
    });
    await page.goto('/settings');
    await expect.poll(() => calls).toBe(1);
    await page.request.post('/_mock/drop');
    await expect.poll(() => calls).toBe(2);
    release();
    // The held fetch is old news; the retry of the failed one still comes, and fills the meters.
    await expect(page.getByTestId('sidebar').getByTestId('quota-window')).toContainText('20%', { timeout: 20_000 });
    expect(calls).toBeGreaterThanOrEqual(3);
  } finally {
    await setWorkQuota(page, null);
  }
});

test("a connection's quotas fetched after a newer connection's never replace them", async ({ page }) => {
  // The first connection's fetch is held back until the connection has dropped and come back with its own.
  let first: (() => void) | undefined;
  const firstLanded = new Promise<void>((resolve) => (first = resolve));
  let read: (() => void) | undefined;
  const firstRead = new Promise<void>((resolve) => (read = resolve));
  let held = false;
  await page.route('**/api/v1/quotas', async (route) => {
    if (held) return route.continue();
    held = true;
    const response = await route.fetch();
    read?.();
    await new Promise((resolve) => setTimeout(resolve, 3000));
    await route.fulfill({ response });
    first?.();
  });
  await setWorkQuota(page, 20);
  try {
    await page.goto('/settings');
    const meter = page.getByTestId('sidebar').getByTestId('quota-window');
    // The held fetch has read 20%; then it's 30%, pushed.
    await firstRead;
    await setWorkQuota(page, 30);
    await expect(meter).toContainText('30%');
    const refetched = page.waitForResponse('**/api/v1/quotas');
    await page.request.post('/_mock/drop');
    await refetched;
    await expect(meter).toContainText('30%');
    // The first fetch, with 20%, lands now; it's older than what's shown, and changes nothing.
    await firstLanded;
    await page.waitForTimeout(300);
    await expect(meter).toContainText('30%', { timeout: 1000 });
  } finally {
    await setWorkQuota(page, null);
  }
});

test('only the transcript scrolls, not the page around it', async ({ page }) => {
  await page.setViewportSize({ width: 1000, height: 500 });
  await createSession(page, 'Give me a long answer in sloppod');
  await expect(page.getByText('The end.')).toBeInViewport({ timeout: 20_000 });
  const transcript = page.getByTestId('transcript-scroller');
  // Anything in the transcript laid out outside its scroller would stretch what's around it, most with the transcript
  // scrolled to the top.
  await transcript.evaluate((element) => element.scrollTo({ top: 0 }));
  const overflowing = await transcript.evaluate((element) => {
    const found = [];
    for (let ancestor = element.parentElement; ancestor; ancestor = ancestor.parentElement) {
      if (ancestor.scrollHeight > ancestor.clientHeight) found.push(ancestor.className);
    }
    return { transcript: element.scrollHeight > element.clientHeight, found };
  });
  expect(overflowing).toEqual({ transcript: true, found: [] });
  await expect(page.getByRole('textbox', { name: 'Prompt' })).toBeInViewport({ ratio: 1 });
});

test('a GitHub account is checked when added, and shows whose it is, what it reaches and when it expires', async ({ page }) => {
  await page.goto('/settings');
  const section = page.getByTestId('credentials-github-user');
  const row = (name: string) => section.getByTestId('credential').filter({ hasText: name });
  const add = async (name: string, token: string) => {
    await section.getByRole('button', { name: 'Add GitHub account' }).click();
    await section.getByLabel('Name', { exact: true }).fill(name);
    await section.getByLabel('Fine-grained personal access token', { exact: true }).fill(token);
    await section.getByRole('button', { name: 'Save' }).click();
  };
  await expect(section).toContainText('No GitHub account yet');

  await add('Classic', 'ghp_classic');
  await expect(section.getByText("that's a classic token")).toBeVisible();
  await section.getByRole('button', { name: 'Cancel' }).click();

  await add('Mine', 'github_pat_mine');
  await expect(row('Mine')).toContainText('Account: bo0tzz');
  await expect(row('Mine').getByTestId('token-repositories')).toHaveText('Reaches 2 repositories');
  await expect(row('Mine').getByTestId('token-repositories')).toHaveAttribute('title', 'bo0tzz/sloppod\nbo0tzz/dotfiles');
  await expect(row('Mine').getByTestId('token-expiry')).toHaveAttribute('data-level', 'ok');
  await expect(page.getByText('github_pat_mine')).toHaveCount(0);

  await add('Expiring', 'github_pat_soon');
  const expiry = row('Expiring').getByTestId('token-expiry');
  await expect(expiry).toHaveAttribute('data-level', 'soon');
  await expect(expiry).toContainText('Expires in 3 days; make a new token and replace this one');

  for (const name of ['Mine', 'Expiring']) {
    await row(name).getByRole('button', { name: `Delete ${name}` }).click();
    await page.getByRole('button', { name: 'Delete', exact: true }).click();
    await expect(row(name)).toHaveCount(0);
  }
});

test('a session at its usage limit says when it carries on', async ({ page }) => {
  await page.setViewportSize({ width: 1400, height: 900 });
  await createSession(page, 'Hit the limit');
  await finishTurn(page);
  await page.request.post('/_mock/limit?minutes=90');
  const banner = page.getByTestId('limited');
  await expect(banner).toContainText('usage limit');
  await expect(banner).toContainText('in 1 h 30 min');
  await expect(page.getByTestId('session-row').first().getByTestId('session-limited')).toBeVisible();

  await page.request.post('/_mock/limit?minutes=0');
  await expect(banner).toBeHidden();
  await expect(page.getByTestId('session-limited')).toBeHidden();
});

test.describe('in Amsterdam', () => {
  test.use({ timezoneId: 'Europe/Amsterdam' });

  test('a user without a timezone gets the browser’s, and can choose another', async ({ page }) => {
    await page.request.post('/_mock/forget-timezone');
    await page.goto('/settings');
    const setting = page.getByTestId('timezone-setting').getByRole('combobox', { name: 'Timezone' });
    await expect(setting).toContainText('Europe/Amsterdam');
    await expect.poll(async () => (await (await page.request.get('/api/v1/settings')).json()).timezone).toBe('Europe/Amsterdam');

    await setting.click();
    await page.getByRole('option', { name: 'Asia/Tokyo', exact: true }).click();
    await expect(setting).toContainText('Asia/Tokyo');
    await expect.poll(async () => (await (await page.request.get('/api/v1/settings')).json()).timezone).toBe('Asia/Tokyo');

    // Once set, it's kept, whatever the browser's.
    await page.reload();
    await expect(setting).toContainText('Asia/Tokyo');
  });
});

test.describe('in UTC', () => {
  test.use({ timezoneId: 'UTC' });

  test('a browser in UTC gets UTC, which can be chosen again after another', async ({ page }) => {
    await page.request.post('/_mock/forget-timezone');
    await page.goto('/settings');
    const setting = page.getByTestId('timezone-setting').getByRole('combobox', { name: 'Timezone' });
    await expect.poll(async () => (await (await page.request.get('/api/v1/settings')).json()).timezone).toBe('UTC');
    await expect(setting).toContainText('UTC');

    await setting.click();
    await page.getByRole('option', { name: 'Asia/Tokyo', exact: true }).click();
    await expect(setting).toContainText('Asia/Tokyo');
    await setting.click();
    await page.getByRole('option', { name: 'UTC', exact: true }).click();
    await expect.poll(async () => (await (await page.request.get('/api/v1/settings')).json()).timezone).toBe('UTC');
  });

  test('the snooze choices follow a timezone chosen in Settings, without a reload', async ({ page }) => {
    await page.setViewportSize({ width: 1920, height: 1000 });
    await page.request.post('/_mock/forget-timezone');
    const id = await createSession(page, 'Highlight: Put the deploy check off until the morning');
    const panel = page.getByTestId('side-panel');
    // The menu has been open once already, in the browser's own zone.
    await panel.getByTestId('highlight-item').getByRole('button', { name: 'Snooze' }).click();
    await expect(page.getByRole('menuitem', { name: 'In an hour' })).toBeVisible();
    await page.keyboard.press('Escape');

    // Another timezone in Settings, and back to the session without a reload.
    await page.getByRole('link', { name: 'Settings' }).first().click();
    const setting = page.getByTestId('timezone-setting').getByRole('combobox', { name: 'Timezone' });
    await setting.click();
    await page.getByRole('option', { name: 'Asia/Tokyo', exact: true }).click();
    await expect.poll(async () => (await (await page.request.get('/api/v1/settings')).json()).timezone).toBe('Asia/Tokyo');
    await page.getByRole('navigation', { name: 'Sessions' }).getByRole('link', { name: /Put the deploy check off/ }).click();

    // Tomorrow morning is 09:00 in that zone, not in the one the menu was opened in before the change.
    await panel.getByTestId('highlight-item').getByRole('button', { name: 'Snooze' }).click();
    await page.getByRole('menuitem', { name: 'Tomorrow morning (09:00)' }).click();
    await expect.poll(async () => {
      const [highlight] = await (await page.request.get(`/api/v1/sessions/${id}/highlights`)).json();
      return highlight.snoozed_until ?? null;
    }).not.toBeNull();
    const [highlight] = await (await page.request.get(`/api/v1/sessions/${id}/highlights`)).json();
    const clockIn = (zone: string) =>
      new Intl.DateTimeFormat('en-GB', { timeZone: zone, hour: '2-digit', minute: '2-digit', hour12: false }).format(
        new Date(highlight.snoozed_until),
      );
    expect(clockIn('Asia/Tokyo')).toBe('09:00');
    expect(clockIn('UTC')).not.toBe('09:00');

    // The card says the same 09:00 the choice meant, in that zone: read in the browser's it would say 00:00.
    const until = page.getByTestId('highlight-card').filter({ hasText: 'Put the deploy check off' }).getByTestId('highlight-snoozed-until');
    await expect(until).toContainText('09:00');
    await expect(until).toHaveAttribute('title', /09:00/);
  });
});

test.describe('on a phone', () => {
  test.use({ viewport: { width: 390, height: 844 } });

  test('a question waits folded to one line, and unfolded keeps its actions in view', async ({ page }) => {
    await createSession(page, 'Please ask me how to do it');
    const panel = page.getByTestId('waiting-panel');
    const summary = page.getByTestId('waiting-summary');
    await expect(summary).toContainText('The agent asks: Please answer the following questions.');
    await expect(questionCard(page)).toBeHidden();
    expect((await panel.boundingBox())!.height).toBeLessThan(80);
    // Folded, it still takes focus the way the card would (#365), so a stray Esc keeps the questions.
    await expect(summary).toBeFocused();
    await page.keyboard.press('Escape');
    await expect(phase(page)).toHaveAttribute('data-phase', 'waiting');

    await summary.getByRole('button', { name: 'Answer' }).click();
    await expect(questionCard(page)).toBeFocused();
    expect((await panel.boundingBox())!.height).toBeLessThanOrEqual(844 * 0.4 + 1);
    await expect(questionCard(page).getByTestId('question-actions')).toBeInViewport();
    // No preview takes room until there's one to show: here, once an option is picked.
    await expect(questionCard(page).getByTestId('option-preview')).toHaveCount(0);
    await questionField(page, 'question_0').getByRole('radio', { name: /Exponential/ }).check();
    await expect(questionCard(page).getByTestId('option-preview')).toContainText('Math.min(500 * 2 ** attempt');
    await expect(questionCard(page).getByTestId('question-actions')).toBeInViewport();

    // It folds away again, keeping what was picked.
    await panel.getByRole('button', { name: 'Fold' }).click();
    await expect(summary).toBeVisible();
    await summary.getByRole('button', { name: 'Answer' }).click();
    await expect(questionField(page, 'question_0').getByRole('radio', { name: /Exponential/ })).toBeChecked();
  });

  test('folding keeps a half-answered question, even when nothing can be stored', async ({ page }) => {
    await page.addInitScript(() => {
      Storage.prototype.setItem = () => {
        throw new DOMException('Full', 'QuotaExceededError');
      };
    });
    await createSession(page, 'Please ask me how to do it');
    await page.getByTestId('waiting-summary').getByRole('button', { name: 'Answer' }).click();
    await questionField(page, 'question_0').getByRole('radio', { name: /Exponential/ }).check();
    await questionCard(page).getByRole('button', { name: 'Next' }).click();
    await expect(questionCard(page).getByTestId('question-step')).toHaveText('Question 2 of 2');

    await page.getByTestId('waiting-panel').getByRole('button', { name: 'Fold' }).click();
    await page.getByTestId('waiting-summary').getByRole('button', { name: 'Answer' }).click();
    await expect(questionCard(page)).toBeFocused();
    await expect(questionCard(page).getByTestId('question-step')).toHaveText('Question 2 of 2');
    await questionCard(page).getByRole('button', { name: 'Back' }).click();
    await expect(questionField(page, 'question_0').getByRole('radio', { name: /Exponential/ })).toBeChecked();
  });

  test("a permission request's diff wraps", async ({ page }) => {
    await createSession(page, 'Add backoff to the reconnect logic in sloppod');
    await page.getByTestId('waiting-summary').getByRole('button', { name: 'Review' }).click();
    const diff = permissionCard(page).getByTestId('diff').first();
    await expect(diff).toBeVisible();
    // Its lines wrap rather than scroll sideways.
    const lines = diff.locator('table').locator('..');
    expect(await lines.evaluate((element) => element.scrollWidth - element.clientWidth)).toBeLessThanOrEqual(0);
  });

  test('the model selector and Revoke fit', async ({ page }) => {
    await createSession(page, 'Add backoff to the reconnect logic in sloppod');
    const model = page.getByTestId('model-select').getByRole('combobox');
    await expect(model).toContainText('Default · Sonnet');
    expect(await model.evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(true);

    await page.getByRole('button', { name: 'Session actions' }).click();
    await page.getByRole('menuitem', { name: 'Grants' }).click();
    const revoke = page.getByTestId('grants').getByRole('button', { name: 'Revoke' }).first();
    const row = revoke.locator('xpath=ancestor::div[contains(@class, "flex-wrap")][1]');
    const select = row.getByRole('combobox');
    const [revokeBox, selectBox] = [await revoke.boundingBox(), await select.boundingBox()];
    expect(Math.abs(revokeBox!.y + revokeBox!.height / 2 - (selectBox!.y + selectBox!.height / 2))).toBeLessThan(8);
  });
});

test('dialogs take focus when they open and give it back when they close', async ({ page }) => {
  await createSession(page, 'Add backoff to the reconnect logic in sloppod');
  await finishTurn(page);
  const menu = page.getByRole('button', { name: 'Session actions' });
  const focusInDialog = () => page.evaluate(() => Boolean(document.activeElement?.closest('[role="dialog"]')));

  // From the session menu, every time: the first Esc closes the dialog, and focus goes back to ⋮.
  for (let round = 0; round < 3; round++) {
    await menu.click();
    await page.getByRole('menuitem', { name: 'Grants' }).click();
    await expect(page.getByTestId('grants')).toBeVisible();
    await expect.poll(focusInDialog).toBe(true);
    await page.keyboard.press('Escape');
    await expect(page.getByTestId('grants')).toBeHidden();
    await expect(menu).toBeFocused();
  }

  await menu.click();
  await page.getByRole('menuitem', { name: 'Add repository…' }).click();
  await expect(page.getByTestId('add-repo')).toBeVisible();
  await expect.poll(focusInDialog).toBe(true);
  await page.keyboard.press('Escape');
  await expect(page.getByRole('dialog')).toBeHidden();
  await expect(menu).toBeFocused();

  // Opened with the keyboard, the menu focuses its first item, so Enter picks it.
  await page.keyboard.press('Enter');
  await expect(page.getByRole('menuitem', { name: 'Grants' })).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(page.getByTestId('grants')).toBeVisible();
  await expect.poll(focusInDialog).toBe(true);
  await page.keyboard.press('Escape');
  await expect(menu).toBeFocused();

  // Not just from the menu: the proposals list gives focus back to its button.
  const proposals = page.getByRole('button', { name: 'Proposals' });
  await proposals.click();
  await expect(page.getByRole('dialog')).toBeVisible();
  await expect.poll(focusInDialog).toBe(true);
  await page.keyboard.press('Escape');
  await expect(page.getByRole('dialog')).toBeHidden();
  await expect(proposals).toBeFocused();
});

/** A new work graph with these items, made through the UI; returns the graph's page. Names are unique per test run. */
async function workGraph(page: Page, items: string[]): Promise<string> {
  const name = `plan-${Math.random().toString(36).slice(2, 8)}`;
  await page.goto('/work');
  await page.getByRole('textbox', { name: 'New graph' }).fill(name);
  await page.getByRole('button', { name: 'Create' }).click();
  await page.getByTestId('work-graphs').getByRole('link', { name }).click();
  await expect(page.getByTestId('session-title')).toHaveText(name);
  for (const title of items) {
    await page.getByRole('textbox', { name: 'New item' }).fill(title);
    await page.getByRole('button', { name: 'Add', exact: true }).click();
    await expect(page.getByTestId('work-tree').getByRole('button', { name: title, exact: true })).toBeVisible();
  }
  return name;
}

const workItemIn = (page: Page, section: string, title: string) =>
  page.getByTestId(section).locator(`[data-testid="work-item"][data-title="${title}"]`);

async function openWorkItem(page: Page, title: string) {
  await page.getByTestId('work-tree').getByRole('button', { name: title, exact: true }).click();
  const detail = page.getByTestId('work-item-detail');
  await expect(detail).toBeVisible();
  return detail;
}

test('a work graph shows what is ready first, then the rest by state, and its structure', async ({ page }) => {
  await workGraph(page, ['Design', 'Build', 'Ship']);
  const ready = page.getByTestId('work-ready');
  await expect(ready.getByTestId('work-item')).toHaveText([/Design/, /Build/, /Ship/]);

  // Build waits on Design: it leaves the ready list and says why.
  const build = await openWorkItem(page, 'Build');
  await build.getByRole('combobox', { name: 'Add a blocker' }).click();
  await page.getByRole('option', { name: 'Design' }).click();
  await build.getByRole('button', { name: 'Block' }).click();
  await expect(build.getByTestId('work-blockers')).toContainText('Design');
  await page.keyboard.press('Escape');
  await expect(ready.getByTestId('work-item')).toHaveText([/Design/, /Ship/]);
  await expect(workItemIn(page, 'work-open', 'Build').getByTestId('blocked-by')).toHaveText('Blocked by Design');

  // A cycle is refused, with the reason.
  const design = await openWorkItem(page, 'Design');
  await design.getByRole('combobox', { name: 'Add a blocker' }).click();
  await page.getByRole('option', { name: 'Build' }).click();
  await design.getByRole('button', { name: 'Block' }).click();
  await expect(design).toContainText('would close a cycle');

  // Design done releases Build.
  await design.getByRole('combobox', { name: 'State' }).click();
  await page.getByRole('option', { name: 'done' }).click();
  await page.keyboard.press('Escape');
  await expect(ready.getByTestId('work-item')).toHaveText([/Build/, /Ship/]);
  await expect(workItemIn(page, 'work-done', 'Design')).toBeVisible();

  // A finished blocker blocks nothing: Build's row no longer names Design.
  await expect(workItemIn(page, 'work-tree', 'Build').getByTestId('blocked-by')).toHaveCount(0);
  // What blocks an item blocks its parts too: Docs, part of Ship, waits on what blocks Ship.
  await page.getByRole('textbox', { name: 'New item' }).fill('Docs');
  await page.getByRole('combobox', { name: 'Part of' }).click();
  await page.getByRole('option', { name: 'Ship' }).click();
  await page.getByRole('button', { name: 'Add', exact: true }).click();
  await expect(workItemIn(page, 'work-ready', 'Docs')).toBeVisible();
  const ship = await openWorkItem(page, 'Ship');
  await ship.getByRole('combobox', { name: 'Add a blocker' }).click();
  await page.getByRole('option', { name: 'Build' }).click();
  await ship.getByRole('button', { name: 'Block' }).click();
  await page.keyboard.press('Escape');
  await expect(workItemIn(page, 'work-ready', 'Docs')).toHaveCount(0);
  await expect(workItemIn(page, 'work-open', 'Docs').getByTestId('blocked-by')).toHaveText('Blocked by Build (through Ship)');

  // Parts sit under what they're part of.
  await page.getByRole('textbox', { name: 'New item' }).fill('Tests');
  await page.getByRole('combobox', { name: 'Part of' }).click();
  await page.getByRole('option', { name: 'Ship' }).click();
  await page.getByRole('button', { name: 'Add', exact: true }).click();
  const tests = page.getByTestId('work-tree').locator('[data-testid="work-item"][data-title="Tests"]');
  await expect(tests).toHaveAttribute('style', /padding-inline-start: 1\.25rem/);
});

test("an item's detail links, changes and undoes from its history", async ({ page }) => {
  await workGraph(page, ['Fix the build']);
  const detail = await openWorkItem(page, 'Fix the build');

  // A completing pull request: it opens on GitHub, and makes the item active.
  await detail.getByRole('textbox', { name: 'Target' }).fill('bo0tzz/sloppod#12');
  await detail.getByRole('combobox', { name: 'Link' }).click();
  await page.getByRole('option', { name: 'Pull request' }).click();
  await detail.getByRole('button', { name: 'Add link' }).click();
  const pull = detail.getByTestId('work-links').getByRole('link', { name: 'Pull request bo0tzz/sloppod#12' });
  await expect(pull).toHaveAttribute('href', 'https://github.com/bo0tzz/sloppod/pull/12');
  await expect(detail.getByRole('combobox', { name: 'State' })).toContainText('active');

  // Edited, then undone from the history.
  await detail.getByRole('button', { name: 'Edit' }).click();
  await detail.getByRole('textbox', { name: 'Title' }).fill('Fix the flaky build');
  await detail.getByRole('button', { name: 'Save' }).click();
  await expect(page.getByRole('dialog').getByRole('paragraph').first()).toHaveText('Fix the flaky build');
  const renamed = detail.getByTestId('work-event').filter({ hasText: 'renamed it' });
  await renamed.getByRole('button', { name: 'Undo' }).click();
  await expect(page.getByRole('dialog').getByRole('paragraph').first()).toHaveText('Fix the build');
  // The rules' own change has no undo; it follows the link.
  await expect(detail.getByTestId('work-event').filter({ hasText: '(by the rules)' }).getByRole('button', { name: 'Undo' })).toHaveCount(0);

  // Merged: the owner says so on the link, and the item is done by it.
  await detail.getByTestId('work-links').getByRole('combobox').first().click();
  await page.getByRole('option', { name: 'finished' }).click();
  await expect(detail.getByRole('combobox', { name: 'State' })).toContainText('done');
  // Reopened, it's active again.
  await detail.getByTestId('work-links').getByRole('combobox').first().click();
  await page.getByRole('option', { name: 'open', exact: true }).click();
  await expect(detail.getByRole('combobox', { name: 'State' })).toContainText('active');

  // Done by hand isn't allowed while the pull request decides; dropping is.
  await detail.getByRole('combobox', { name: 'State' }).click();
  await page.getByRole('option', { name: 'done' }).click();
  await expect(detail).toContainText("it's done when its completing links are finished");
  await detail.getByRole('combobox', { name: 'State' }).click();
  await page.getByRole('option', { name: 'dropped' }).click();
  await page.keyboard.press('Escape');
  await expect(workItemIn(page, 'work-dropped', 'Fix the build')).toBeVisible();
});

test('items rerank by dragging, and the home page shows what is ready', async ({ page }) => {
  const name = await workGraph(page, ['First', 'Second', 'Third']);
  const tree = page.getByTestId('work-tree').getByTestId('work-tree-item');
  await tree.filter({ hasText: 'Third' }).dragTo(tree.filter({ hasText: 'First' }));
  await expect(page.getByTestId('work-ready').getByTestId('work-item')).toHaveText([/Third/, /First/, /Second/]);
  // Without dragging, from an item's detail.
  const second = await openWorkItem(page, 'Second');
  await second.getByRole('button', { name: 'Move up' }).click();
  await page.keyboard.press('Escape');
  await expect(page.getByTestId('work-ready').getByTestId('work-item')).toHaveText([/Third/, /Second/, /First/]);

  await page.goto('/');
  const frontier = page.locator(`[data-testid="work-frontier-graph"][data-graph="${name}"]`);
  await expect(frontier.getByRole('link')).toHaveText([name, 'Third', 'Second', 'First']);
  await frontier.getByRole('link', { name }).click();
  await expect(page.getByTestId('session-title')).toHaveText(name);
});

test('work graphs are a click away in the sidebar', async ({ page }) => {
  await page.goto('/settings');
  await page.getByTestId('sidebar').getByRole('link', { name: 'Work' }).click();
  await expect(page).toHaveURL(/\/work$/);
  await expect(page.getByTestId('work-graphs')).toBeVisible();
});

test.describe('work on a phone', () => {
  test.use({ viewport: { width: 390, height: 844 } });

  test('a work graph and an item fit, and change', async ({ page }) => {
    await page.goto('/');
    await page.getByRole('link', { name: 'Work' }).click();
    await expect(page).toHaveURL(/\/work$/);
    await workGraph(page, ['Read the issue', 'Write the fix']);
    const fits = async () => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth);
    expect(await fits()).toBe(true);
    const detail = await openWorkItem(page, 'Write the fix');
    expect(await fits()).toBe(true);
    await detail.getByRole('button', { name: 'Move up' }).click();
    await page.keyboard.press('Escape');
    await expect(page.getByTestId('work-ready').getByTestId('work-item')).toHaveText([/Write the fix/, /Read the issue/]);
  });
});

test('saves the model sub-sessions run on', async ({ page }) => {
  const defaults = page.getByTestId('session-defaults');
  const field = defaults.getByRole('combobox', { name: 'Sub-sessions' });
  await page.goto('/settings');
  await expect(field).toContainText('A cheaper model (Sonnet)');
  await field.click();
  await page.getByRole('option', { name: 'The same as their parent', exact: true }).click();
  await defaults.getByRole('button', { name: 'Save' }).click();
  await expect(defaults.getByRole('button', { name: 'Save' })).toBeDisabled();
  await page.reload();
  await expect(field).toContainText('The same as their parent');
  const settings: { sub_session_model: string | null; model: string | null } = await (
    await page.request.get('/api/v1/settings')
  ).json();
  expect(settings.sub_session_model).toBe('parent');
  expect(settings.model).toBeNull();
  await page.request.put('/api/v1/settings', { data: { sub_session_model: null } });
});

test('a selection runs from one message across tool calls to the next', async ({ page }) => {
  // Tall enough for the whole transcript, so the drag needs no scrolling.
  await page.setViewportSize({ width: 1280, height: 2000 });
  await createSession(page, 'Select across turns in sloppod');
  await finishTurn(page);
  // The tool calls are folded away unless opened; open them, as the selection takes them in.
  await expandFolds(page);

  // From a message, past the tool calls after it, to the last message: copied as on any page, tool calls included.
  const before = agentMessages(page).filter({ hasText: 'Reconnects use a fixed' });
  const after = agentMessages(page).filter({ hasText: FINAL });
  const from = (await before.locator('p').first().boundingBox())!;
  const to = (await after.locator('p').last().boundingBox())!;
  expect(from.y).toBeGreaterThanOrEqual(0);
  expect(to.y + to.height).toBeLessThanOrEqual(page.viewportSize()!.height);
  // On the first line of one and the last of the other, not the gap between lines.
  await page.mouse.move(from.x + 1, from.y + 8);
  await page.mouse.down();
  await page.mouse.move(to.x + to.width - 1, to.y + to.height - 8, { steps: 20 });
  await page.mouse.up();

  const selected = await page.evaluate(() => window.getSelection()?.toString() ?? '');
  expect(selected).toMatch(/^Reconnects use a fixed/);
  expect(selected).toContain('npm test -- reconnect');
  expect(selected).toMatch(/approve it\.?\s*$/);
});

test('sub-sessions can follow their parent before the agent has listed models', async ({ page }) => {
  await page.request.post('/_mock/models?offered=0');
  try {
    const defaults = page.getByTestId('session-defaults');
    await page.goto('/settings');
    // The default model has nothing to choose from yet, so it's typed in.
    await expect(defaults.getByRole('textbox', { name: 'Model' })).toBeVisible();
    const field = defaults.getByRole('combobox', { name: 'Sub-sessions' });
    await field.click();
    await page.getByRole('option', { name: 'The same as their parent', exact: true }).click();
    await defaults.getByRole('button', { name: 'Save' }).click();
    await expect(defaults.getByRole('button', { name: 'Save' })).toBeDisabled();
    const settings: { sub_session_model: string | null } = await (await page.request.get('/api/v1/settings')).json();
    expect(settings.sub_session_model).toBe('parent');
  } finally {
    await page.request.put('/api/v1/settings', { data: { sub_session_model: null } });
    await page.request.post('/_mock/models');
  }
});

test('saves the instructions all sessions get', async ({ page }) => {
  const section = page.getByTestId('instructions-setting');
  const save = section.getByRole('button', { name: 'Save' });
  await page.goto('/settings');
  await expect(save).toBeDisabled();
  await section.getByRole('textbox', { name: 'Instructions' }).fill('  - Only comment code for intent a reader could not infer.\n');
  await save.click();
  await expect(save).toBeDisabled();
  await page.reload();
  await expect(section.getByRole('textbox', { name: 'Instructions' })).toHaveValue(
    '- Only comment code for intent a reader could not infer.',
  );
  const settings: { instructions: string | null } = await (await page.request.get('/api/v1/settings')).json();
  expect(settings.instructions).toBe('- Only comment code for intent a reader could not infer.');
  await page.request.put('/api/v1/settings', { data: { instructions: null } });
});

test('what is typed while the instructions save is kept', async ({ page }) => {
  const section = page.getByTestId('instructions-setting');
  const field = section.getByRole('textbox', { name: 'Instructions' });
  const save = section.getByRole('button', { name: 'Save' });
  await page.goto('/settings');
  let release = () => {};
  const held = new Promise<void>((resolve) => (release = resolve));
  await page.route('**/api/v1/settings', async (route) => {
    if (route.request().method() === 'PUT') await held;
    await route.continue();
  });
  await field.fill('First line.');
  await save.click();
  await field.fill('First line.\nSecond line.');
  release();
  await expect(page.getByText('Saved your instructions')).toBeVisible();
  await expect(field).toHaveValue('First line.\nSecond line.');
  await expect(save).toBeEnabled();
  await page.unroute('**/api/v1/settings');
  await page.request.put('/api/v1/settings', { data: { instructions: null } });
});

test('a work-plan proposal reads as what it changes', async ({ page }) => {
  const graph = '0192f000-0000-7000-8000-000000000001';
  const add = async (title: string) => {
    const created = await page.request.post(`/api/v1/work/graphs/${graph}/items`, { data: { title } });
    expect(created.status()).toBe(201);
  };
  for (const title of ['Parser work', 'Write the parser', 'Old exporter']) await add(title);
  await createSession(page, 'Plan the work');

  const card = page.getByTestId('proposal-card');
  await expect(card.getByTestId('proposal-card-title')).toHaveText('Plan for sloppod: move 1 item, drop 1');
  await expect(card).toContainText('Work plan');
  await expect(card).toContainText('The parser comes first now');
  await card.getByRole('button', { name: 'Review' }).click();

  const dialog = page.getByRole('dialog');
  await expect(dialog.getByTestId('proposal-title')).toHaveText('Plan for sloppod: move 1 item, drop 1');
  await expect(dialog.getByTestId('proposal-reason')).toHaveText('The parser comes first now, and the old exporter is superseded.');
  await expect(dialog.getByTestId('plan-change')).toHaveText(['Move “Write the parser” to the top of “Parser work”', 'Drop “Old exporter”']);
  await expect(dialog.getByTestId('proposal-payload')).toHaveCount(0);
});

test('a proposal of a kind the UI does not know reads by its summary', async ({ page }) => {
  await createSession(page, 'Propose something new');

  const card = page.getByTestId('proposal-card');
  await expect(card.getByTestId('proposal-card-title')).toHaveText('a widget for the future');
  await card.getByRole('button', { name: 'Review' }).click();
  const dialog = page.getByRole('dialog');
  await expect(dialog.getByTestId('proposal-title')).toHaveText('a widget for the future');
  await expect(dialog.getByTestId('proposal-payload')).toContainText('a widget');
  await expect(dialog).not.toContainText('sloppod.example.future');
});

test('an idle session with commands running shows it is waiting on them', async ({ page }) => {
  const prompt = 'Run the tests in the background';
  await createSession(page, prompt);
  await expect(agentMessages(page).filter({ hasText: 'in the background' })).toBeVisible();

  const header = phase(page);
  await expect(header).toHaveText('Waiting on 2 commands');
  await expect(header).toHaveAttribute('data-phase', 'idle');
  await expect(header).toHaveAttribute('title', /cargo test --workspace\nnpm run build/);
  const row = page.getByTestId('session-row').filter({ hasText: prompt });
  await expect(row.getByTestId('phase')).toHaveText('Waiting on 2 commands');

  // Once they're done, it's idle again.
  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill('Tell me when the background work is done');
  await box.press('Enter');
  await expect(agentMessages(page).filter({ hasText: 'Both finished' })).toBeVisible();
  await expect(header).toHaveText('Idle');
  await expect(row.getByTestId('phase')).toHaveText('Idle');
});

test('an idle session with a proposal or a question waiting shows it is waiting on you', async ({ page }) => {
  await page.setViewportSize({ width: 1400, height: 900 });
  await createSession(page, 'Propose a PR for sloppod, the one to wait on');
  await finishTurn(page);
  const sidebar = page.getByRole('navigation', { name: 'Sessions' });
  const row = sidebar.getByTestId('session-row').filter({ hasText: 'Propose a PR for sloppod, the one to wait on' });
  await expect(phase(page)).toHaveText('Waiting on you');
  await expect(row.getByTestId('phase')).toHaveText('Waiting on you');
  await expect(row.getByTestId('phase')).toHaveAttribute('data-phase', 'idle');
  await expect(row.getByTestId('phase')).toHaveAttribute('title', /1 proposal/);

  // The home page says so too.
  await page.goto('/');
  await expect(page.getByTestId('session-row').filter({ hasText: 'Propose a PR for sloppod, the one to wait on' }).getByTestId('phase').first()).toHaveText(
    'Waiting on you',
  );

  // Decided, it's idle.
  await page.getByTestId('session-row').filter({ hasText: 'Propose a PR for sloppod, the one to wait on' }).first().click();
  await page.getByTestId('proposal-card').getByRole('button', { name: 'Review' }).click();
  await page.getByRole('button', { name: 'Reject…' }).click();
  await page.getByRole('button', { name: 'Reject', exact: true }).click();
  // The header follows the proposals the view already has: the runtime doesn't push the session when one is decided.
  await expect(phase(page)).toHaveText('Idle');
  await expect(row.getByTestId('phase')).toHaveText('Idle');

  // An ask_user question, which outlives the turn.
  await createSession(page, 'Use ask_user about the retry budget');
  await expect(phase(page)).toHaveAttribute('data-phase', 'working');
  await page.getByRole('button', { name: 'Stop' }).click();
  await expect(phase(page)).toHaveAttribute('data-phase', 'idle');
  const asked = sidebar.getByTestId('session-row').filter({ hasText: 'Use ask_user about the retry budget' });
  await expect(asked.getByTestId('phase')).toHaveText('Waiting on you');
  await expect(asked.getByTestId('phase')).toHaveAttribute('title', /1 question/);
});

test('the session list follows what waits on you as it is pushed, without fetching', async ({ page }) => {
  await page.setViewportSize({ width: 1400, height: 900 });
  const prompt = 'Propose a PR for sloppod, left for the list to follow';
  const id = await createSession(page, prompt);
  await finishTurn(page);
  const row = page.getByRole('navigation', { name: 'Sessions' }).getByTestId('session-row').filter({ hasText: prompt });
  await expect(row.getByTestId('phase')).toHaveText('Waiting on you');

  // Another session shown, and the list no longer fetched: a proposal of the first one decided elsewhere has to
  // reach the list by push.
  await createSession(page, 'Say something while the list is watching');
  await finishTurn(page);
  await page.route('**/api/v1/sessions', (route) => route.abort());
  const proposals: { id: string; status: string }[] = await (await page.request.get(`/api/v1/proposals?session=${id}`)).json();
  const pending = proposals.find((proposal) => proposal.status === 'pending')!;
  await page.request.post(`/api/v1/proposals/${pending.id}/reject`, { data: {} });
  await expect(row.getByTestId('phase')).not.toHaveText('Waiting on you');
});

test('a list fetched before a push does not undo it', async ({ page }) => {
  await page.setViewportSize({ width: 1400, height: 900 });
  const prompt = 'Propose a PR for sloppod, left for a stale list to undo';
  const id = await createSession(page, prompt);
  await finishTurn(page);
  const row = page.getByRole('navigation', { name: 'Sessions' }).getByTestId('session-row').filter({ hasText: prompt });
  await expect(row.getByTestId('phase')).toHaveText('Waiting on you');
  await createSession(page, 'Say something while a list is on its way');
  await finishTurn(page);

  // A list fetch on its way, held until the proposal has been decided and the push has come: its copy of the session
  // is the one from before, with the same updated_at, as what a session waits on doesn't move it.
  let hold!: () => void;
  const held = new Promise<void>((resolve) => (hold = resolve));
  let answered!: () => void;
  const done = new Promise<void>((resolve) => (answered = resolve));
  let started = false;
  await page.route('**/api/v1/sessions', async (route) => {
    // Later fetches (the timer's) fail, so none is applied ahead of this one, which an older list never would be.
    if (started) return route.abort();
    started = true;
    await held;
    const list: { id: string; waiting: { proposals: number } }[] = await (await page.request.get('/api/v1/sessions')).json();
    for (const session of list) if (session.id === id) session.waiting = { ...session.waiting, proposals: 1 };
    await route.fulfill({ json: list });
    answered();
  });
  await page.request.post('/_mock/drop');
  await expect.poll(() => started).toBe(true);

  const proposals: { id: string; status: string }[] = await (await page.request.get(`/api/v1/proposals?session=${id}`)).json();
  const pending = proposals.find((proposal) => proposal.status === 'pending')!;
  await page.request.post(`/api/v1/proposals/${pending.id}/reject`, { data: {} });
  // Idle again once the agent has taken the decision, so the badge is the only thing a stale copy could bring back. The
  // label says Unknown meanwhile: the list's fetch, held, keeps the connection coming up. The phase itself is read.
  await expect(row.getByTestId('phase')).toHaveAttribute('data-phase', 'idle');
  hold();
  await done;
  // Long enough for the list to have been applied; not retried, as a badge that came back would only go with a fetch.
  await page.evaluate(() => new Promise((resolve) => setTimeout(resolve, 300)));
  expect(await row.getByTestId('phase').textContent()).not.toContain('Waiting on you');
});

test('arrow keys move between the rows of the session list', async ({ page }) => {
  await page.setViewportSize({ width: 1400, height: 900 });
  await createSession(page, 'First session about arrows');
  await finishTurn(page);
  await createSession(page, 'Second session about arrows');
  await finishTurn(page);
  await createSession(page, 'Third session about arrows');
  await finishTurn(page);
  const rows = page.getByRole('navigation', { name: 'Sessions' }).getByTestId('session-row').filter({ hasText: 'session about arrows' });
  await expect(rows).toHaveCount(3);

  await rows.first().focus();
  await page.keyboard.press('ArrowDown');
  await expect(rows.nth(1)).toBeFocused();
  await page.keyboard.press('ArrowDown');
  await expect(rows.nth(2)).toBeFocused();
  await page.keyboard.press('ArrowUp');
  await expect(rows.nth(1)).toBeFocused();
  await page.keyboard.press('ArrowUp');
  await expect(rows.first()).toBeFocused();

  // The first row of the list has nothing above it: the key leaves it where it is.
  const top = page.getByRole('navigation', { name: 'Sessions' }).getByTestId('session-row').first();
  await top.focus();
  await page.keyboard.press('ArrowUp');
  await expect(top).toBeFocused();
});

test("a session's disk use shows in the list, its header and Settings, and stays while it's parked", async ({ page }) => {
  const id = await createSession(page, 'Add backoff to the reconnect logic in sloppod');
  await finishTurn(page);
  const row = page.locator(`[data-testid="session-row"][href="/sessions/${id}"]`);
  await expect(row.getByTestId('session-disk')).toHaveText('700 MB');
  await expect(page.getByTestId('session-screen').getByTestId('session-disk')).toHaveText('700 MB');

  await page.goto('/settings');
  const storage = page.getByTestId('storage');
  const listed = storage.locator(`[data-testid="storage-session"][data-session="${id}"]`);
  await expect(listed.getByTestId('storage-used')).toHaveText('700 MB');
  await expect(storage.getByTestId('storage-total')).toContainText(/^\d+ volumes?, .* used/);
  await listed.getByRole('button', { name: /^Park the session/ }).click();
  await expect(listed.getByRole('button', { name: /^Park the session/ })).toHaveCount(0);
  await expect(listed.getByTestId('storage-used')).toHaveText('700 MB');
});

test('Storage lists every volume: a session never measured, a reviewer, and one of no known session', async ({ page }) => {
  const [parked, reviewer] = await (await page.request.post('/_mock/unmeasured')).json();
  await page.goto('/settings');
  const storage = page.getByTestId('storage');
  const row = (id: string) => storage.locator(`[data-testid="storage-session"][data-session="${id}"]`);

  // Parked before sessions said how much they use: listed, and not measured, rather than left out.
  await expect(row(parked).getByTestId('storage-used')).toHaveText('not measured');
  await expect(row(reviewer).getByTestId('storage-used')).toHaveText('not measured');
  await expect(row(reviewer).getByTestId('storage-reviewer')).toHaveText('reviewer');
  await expect(storage.getByTestId('storage-total')).toContainText(/\d+ not measured/);

  // A volume whose session nobody knows has a group of its own, by its name, and is left alone.
  const unknown = storage.getByTestId('storage-unknown');
  await expect(unknown).toContainText('of no known session');
  await expect(unknown.getByTestId('storage-unknown-volume')).toHaveText('workdir-sloppod-00000000-0000-7000-8000-000000000000');
});

test('what sessions on endpoint models spent shows next to the usage meters, with the budget', async ({ page }) => {
  await page.goto('/');
  const spend = page.getByTestId('spend');
  await expect(spend).toHaveCount(0);
  const at = new Date().toISOString();
  await page.request.post('/_mock/spend', {
    data: {
      total: 2.75,
      keys: [
        { key_id: 'session:1', session_id: '1', name: 'Port the parser', spend: 0.75, max_budget: 5, observed_at: at },
        { key_id: 'credential:2', session_id: null, name: 'litellm', spend: 2, max_budget: null, observed_at: at },
        { key_id: 'session:3', session_id: '3', name: 'Nothing yet', spend: 0, max_budget: 5, observed_at: at },
      ],
    },
  });
  // Pushed, not polled: it shows without a reload.
  await expect(page.getByTestId('spend-total').first()).toHaveText('$2.75 spent');
  const keys = page.getByTestId('spend').first().getByTestId('spend-key');
  await expect(keys).toHaveCount(2);
  await expect(keys.nth(0)).toContainText('Port the parser');
  await expect(keys.nth(0)).toContainText('$0.750 of $5.00');
  await expect(keys.nth(1)).toContainText('litellm');
  await expect(keys.nth(1)).toContainText('$2.00');
  await page.request.post('/_mock/spend', { data: { total: 0, keys: [] } });
  await expect(page.getByTestId('spend')).toHaveCount(0);
});

test('a budget per session is saved with the defaults', async ({ page }) => {
  await page.goto('/settings');
  const defaults = page.getByTestId('session-defaults');
  const budget = defaults.getByTestId('session-budget');
  await budget.fill('2.5');
  await defaults.getByRole('button', { name: 'Save' }).click();
  await expect(defaults.getByRole('button', { name: 'Save' })).toBeDisabled();
  await page.reload();
  await expect(budget).toHaveValue('2.5');
  // Something that isn't a number of dollars is an error, and the budget saved stays.
  for (const typed of ['$5', '1,50', '0']) {
    await budget.fill(typed);
    await expect(defaults.getByTestId('session-budget-error')).toBeVisible();
    await expect(defaults.getByRole('button', { name: 'Save' })).toBeDisabled();
  }
  await page.reload();
  await expect(budget).toHaveValue('2.5');
  await budget.fill('');
  await defaults.getByRole('button', { name: 'Save' }).click();
  await expect(defaults.getByRole('button', { name: 'Save' })).toBeDisabled();
  await page.reload();
  await expect(budget).toHaveValue('');
});
