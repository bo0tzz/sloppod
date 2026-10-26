// Renders static/icon.svg into the PNG icons the manifest and iOS need.
//   node scripts/icons.mjs
import { existsSync, readFileSync } from 'node:fs';
import { chromium } from '@playwright/test';

const svg = readFileSync('static/icon.svg', 'utf8');
// Maskable and Apple icons get cropped by the platform, so they need a full-bleed square.
const square = svg.replace('rx="112"', 'rx="0"');

const icons = [
  { file: 'static/icon-192.png', size: 192, svg },
  { file: 'static/icon-512.png', size: 512, svg },
  { file: 'static/icon-maskable-512.png', size: 512, svg: square },
  { file: 'static/apple-touch-icon.png', size: 180, svg: square },
];

const executablePath = process.env.CHROMIUM_PATH ?? (existsSync('/usr/bin/chromium') ? '/usr/bin/chromium' : undefined);
const browser = await chromium.launch({ executablePath });
try {
  const page = await browser.newPage();
  for (const icon of icons) {
    await page.setViewportSize({ width: icon.size, height: icon.size });
    await page.setContent(
      `<style>html,body{margin:0;background:transparent}svg{display:block;width:100vw;height:100vh}</style>${icon.svg}`,
    );
    await page.screenshot({ path: icon.file, omitBackground: true });
    console.log(`wrote ${icon.file}`);
  }
} finally {
  await browser.close();
}
