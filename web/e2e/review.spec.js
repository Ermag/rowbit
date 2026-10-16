// A reviewer signs in, reads a held change row by row, approves it, sees it
// applied, undoes it, and rejects another. e2e/run.sh proposes the changes.
import { expect, test } from '@playwright/test';

const password = process.env.ROWBIT_PASSWORD;

async function signIn(page) {
  await page.goto('/');
  await expect(page).toHaveURL(/\/login$/);
  await page.getByLabel('Your name').fill('e2e');
  await page.getByLabel('Page password').fill(password);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('heading', { name: /Waiting for a person/ })).toBeVisible();
}

test('review, approve, undo and reject', async ({ page }) => {
  const errors = [];
  page.on('console', (m) => m.type() === 'error' && errors.push(m.text()));
  page.on('pageerror', (e) => errors.push(e.message));

  await signIn(page);
  await expect(page.getByRole('heading', { name: 'Waiting for a person (3)' })).toBeVisible();
  await page.screenshot({ path: 'e2e/out/queue.png', fullPage: true });

  // The oldest held change: three rows.
  await page.getByRole('link', { name: /#1/ }).click();
  await expect(page.getByText('updates 3 rows in')).toBeVisible();
  // NULL is told apart from the text 'NULL', and numbers from text.
  await expect(page.locator('i.null').first()).toBeVisible();
  await expect(page.locator('td.before', { hasText: '"NULL"' })).toBeVisible();
  await page.screenshot({ path: 'e2e/out/change.png', fullPage: true });

  await page.getByRole('button', { name: 'Approve and apply all 3 rows' }).click();
  await expect(page.locator('.state.applied')).toBeVisible();
  await page.getByRole('button', { name: 'Undo' }).click();
  await expect(page.locator('.state.undone')).toBeVisible();

  await page.goto('/#/changes/2');
  await page.getByPlaceholder('Why reject?').fill('not today');
  await page.getByRole('button', { name: 'Reject' }).click();
  await expect(page.locator('.state.rejected')).toBeVisible();

  await page.getByRole('link', { name: 'Log' }).click();
  await expect(page.getByText("The audit log's hash chain checks out.")).toBeVisible();
  await page.screenshot({ path: 'e2e/out/log.png', fullPage: true });

  // The CSP held: nothing was blocked, nothing threw.
  expect(errors).toEqual([]);
});
