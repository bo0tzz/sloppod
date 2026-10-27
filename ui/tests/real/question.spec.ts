import { expect, test } from '@playwright/test';

// The agent asks a question (AskUserQuestion) and the UI answers it, against a real runtime.
// It uses a real model, so it only runs with SLOPPOD_REAL_TURN=1 and an idle SLOPPOD_SESSION.

const SESSION = process.env.SLOPPOD_SESSION ?? '';
const PROMPT =
  'Use the AskUserQuestion tool to ask me one question: which colour I prefer, with the options Red and Blue. ' +
  'Then tell me my answer in one short sentence. Do nothing else.';
const dir = '.screenshots/real';

test.skip(!process.env.SLOPPOD_REAL_TURN || !SESSION, 'runs a real prompt; set SLOPPOD_REAL_TURN=1 and SLOPPOD_SESSION=<id>');

test('answers a question from the agent', async ({ page }) => {
  test.setTimeout(5 * 60_000);
  const phase = () => page.getByTestId('session-screen').getByTestId('phase');

  await page.goto(`/sessions/${SESSION}`);
  await expect(page.getByTestId('message-user').first()).toBeVisible();
  await expect(phase()).toHaveAttribute('data-phase', 'idle');

  const box = page.getByRole('textbox', { name: 'Prompt' });
  await box.fill(PROMPT);
  await box.press('Enter');

  const card = page.getByTestId('question-card');
  await expect(card).toBeVisible({ timeout: 120_000 });
  await expect(phase()).toHaveAttribute('data-phase', 'waiting');
  await page.screenshot({ path: `${dir}/question.png` });

  // A reload while the question is open shows it again.
  await page.reload();
  await expect(card).toBeVisible();

  await card.getByText('Blue', { exact: true }).click();
  await card.getByRole('button', { name: 'Answer' }).click();
  await expect(card).toBeHidden();
  await expect(page.getByTestId('message-agent').last()).toContainText(/blue/i, { timeout: 120_000 });
  await expect(phase()).toHaveAttribute('data-phase', 'idle');
  await page.screenshot({ path: `${dir}/question-answered.png` });
});
