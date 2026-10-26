import { mkdirSync, writeFileSync } from 'node:fs';
import { expect, test, type Page } from '@playwright/test';

// One small live turn against a real runtime: prompt, permission, a steer, and a reload mid-turn.
// It uses a real model, so it only runs with SLOPPOD_REAL_TURN=1. It never approves proposals.

const SESSION = process.env.SLOPPOD_SESSION ?? '2677e6a5-833c-466c-8a94-41eee6a260e8';
// A write, so the default mode asks for permission and the turn blocks long enough to reload and steer.
const PROMPT = 'Run touch /tmp/sloppod-ui-check, then reply with one word.';
const STEER = 'Make that one word French.';
const dir = '.screenshots/real';

test.skip(!process.env.SLOPPOD_REAL_TURN, 'runs a real prompt; set SLOPPOD_REAL_TURN=1');

type Frame = { at: number; dir: 'in' | 'out'; message: unknown };

test('live turn with a permission, a steer and a reload', async ({ page }) => {
  test.setTimeout(8 * 60_000);
  const start = Date.now();
  const frames: Frame[] = [];
  page.on('websocket', (socket) => {
    const record = (dir: Frame['dir']) => ({ payload }: { payload: string | Buffer }) => {
      try {
        frames.push({ at: Date.now() - start, dir, message: JSON.parse(String(payload)) });
      } catch {
        frames.push({ at: Date.now() - start, dir, message: String(payload) });
      }
    };
    socket.on('framesent', record('out'));
    socket.on('framereceived', record('in'));
  });
  const shot = (name: string) => page.screenshot({ path: `${dir}/turn-${name}.png` });
  const phase = () => page.getByTestId('session-screen').getByTestId('phase');
  const card = page.getByTestId('permission-request');

  async function allowPending() {
    // Pick the allow-once option by its kind in the request we received, not by its label.
    const requests = frames
      .filter((f) => f.dir === 'in' && (f.message as { method?: string }).method === 'session/request_permission')
      .map((f) => (f.message as { params: { options: { name: string; kind: string }[] } }).params);
    const option = requests.at(-1)?.options.find((o) => o.kind === 'allow_once');
    if (!option) throw new Error('permission request without an allow_once option');
    await card.first().getByRole('button', { name: option.name, exact: true }).click();
  }

  try {
    await page.goto(`/sessions/${SESSION}`);
    await expect(page.getByTestId('message-user').first()).toBeVisible();
    await expect(phase()).toHaveAttribute('data-phase', 'idle');
    const usersBefore = await page.getByTestId('message-user').count();

    const box = page.getByRole('textbox', { name: 'Prompt' });
    await box.fill(PROMPT);
    await box.press('Enter');
    const prompt = page.getByTestId('message-user').filter({ hasText: PROMPT });
    await expect(prompt).toHaveCount(1);
    await expect(phase()).toHaveAttribute('data-phase', /working|waiting/);

    await expect(card).toBeVisible({ timeout: 120_000 });
    await expect(phase()).toHaveAttribute('data-phase', 'waiting');
    await shot('permission');

    // Reload while the turn is blocked: the history comes back once and the request is sent again.
    await page.reload();
    await expect(card).toBeVisible();
    await expect(prompt).toHaveCount(1);
    await shot('permission-after-reload');

    await page.getByRole('textbox', { name: 'Steering message' }).fill(STEER);
    await page.getByRole('button', { name: 'Steer' }).click();
    const steer = page.getByTestId('message-user').filter({ hasText: STEER });
    await expect(steer).toHaveCount(1);
    await expect(steer).toHaveAttribute('data-steering', 'true');
    await shot('steered');

    await allowPending();
    await expect(card).toBeHidden();

    const deadline = Date.now() + 4 * 60_000;
    while (Date.now() < deadline) {
      if ((await phase().getAttribute('data-phase')) === 'idle') break;
      if (await card.first().isVisible()) {
        await allowPending();
      }
      await page.waitForTimeout(1000);
    }
    await expect(phase()).toHaveAttribute('data-phase', 'idle', { timeout: 5_000 });
    await expect(page.getByTestId('message-user')).toHaveCount(usersBefore + 2);
    await shot('done');

    await page.reload();
    await expect(page.getByTestId('message-user')).toHaveCount(usersBefore + 2);
    await page.setViewportSize({ width: 390, height: 844 });
    await page.emulateMedia({ colorScheme: 'dark' });
    await shot('done-mobile-dark');
  } finally {
    mkdirSync('test-results/real', { recursive: true });
    writeFileSync('test-results/real/turn-frames.json', JSON.stringify(frames, null, 1));
  }
});
