#!/usr/bin/env node
/**
 * Home Edge — Playwright mobile screenshot gallery
 *
 * Usage (via `cargo xtask screenshot`, which sets these env vars):
 *   SCREENSHOT_BASE_URL=http://127.0.0.1:8199
 *   SCREENSHOT_OUT_DIR=screenshots
 *
 * Or run directly:
 *   SCREENSHOT_BASE_URL=http://localhost:8124 \
 *   SCREENSHOT_OUT_DIR=screenshots \
 *   node scripts/screenshot.js
 *
 * Requires: npx playwright install chromium  (first time only)
 */

const { chromium } = require('playwright');
const fs = require('fs');
const path = require('path');

const BASE_URL = (process.env.SCREENSHOT_BASE_URL || 'http://127.0.0.1:8199').replace(/\/$/, '');
const OUT_DIR  = path.resolve(process.env.SCREENSHOT_OUT_DIR || 'screenshots');

// iPhone 14 Pro viewport
const VIEWPORT = { width: 390, height: 844 };

// [path, slug, label]
const ROUTES = [
  ['/',                                                          'dashboard',              'Dashboard'],
  ['/areas',                                                     'areas',                  'Areas'],
  ['/history',                                                   'history',                'History'],
  ['/logbook',                                                   'logbook',                'Logbook'],
  ['/devices',                                                   'devices',                'Devices'],
  ['/notifications',                                             'notifications',          'Notifications'],
  ['/zigbee',                                                    'zigbee-devices',         'Zigbee Devices'],
  ['/zigbee/0xec1bbdfffecafe01',                                 'zigbee-sensor-detail',   'Zigbee Sensor Detail'],
  ['/zigbee/0xec1bbdfffecafe01/entities/sensor.snzb02_temperature', 'zigbee-entity-edit',  'Zigbee Entity Edit'],
  ['/zigbee/0xec1bbdfffecafe02',                                 'zigbee-bulb-detail',     'Zigbee Bulb Detail'],
  ['/settings',                                                  'settings',               'Settings'],
  ['/settings/users',                                            'settings-users',         'Settings – Users'],
  ['/profile',                                                   'profile',                'Profile'],
  ['/system',                                                    'system',                 'System'],
  ['/developer-tools',                                           'developer-tools',        'Developer Tools'],
];

async function main() {
  fs.mkdirSync(OUT_DIR, { recursive: true });

  const browser = await chromium.launch();
  const context = await browser.newContext({
    viewport: VIEWPORT,
    deviceScaleFactor: 2,
    userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1',
  });

  const results = [];

  for (const [routePath, slug, label] of ROUTES) {
    const url = `${BASE_URL}${routePath}`;
    const file = `${slug}.png`;
    const dest = path.join(OUT_DIR, file);
    console.log(`  ${label.padEnd(30)} → ${file}`);

    const page = await context.newPage();
    try {
      await page.goto(url, { waitUntil: 'networkidle', timeout: 10_000 });
      // Let any HTMX fragments settle.
      await page.waitForTimeout(300);
      await page.screenshot({ path: dest, fullPage: true });
      results.push({ slug, label, file, ok: true });
    } catch (err) {
      console.warn(`    ⚠ ${label}: ${err.message}`);
      results.push({ slug, label, file, ok: false, error: err.message });
    } finally {
      await page.close();
    }
  }

  await browser.close();

  // Write index.html gallery.
  writeGallery(results);
  console.log(`\nGallery: ${path.join(OUT_DIR, 'index.html')}`);
}

function writeGallery(results) {
  const cards = results.map(({ label, file, ok, error }) => {
    if (!ok) {
      return `
        <div class="card error">
          <div class="label">${label}</div>
          <div class="err">${error ?? 'unknown error'}</div>
        </div>`;
    }
    return `
        <div class="card">
          <div class="label">${label}</div>
          <a href="${file}" target="_blank">
            <img src="${file}" alt="${label}" loading="lazy">
          </a>
        </div>`;
  }).join('\n');

  const html = `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Home Edge — Screenshot Gallery</title>
<style>
  *, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
  body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
         background: #111; color: #eee; padding: 1rem; }
  h1 { margin-bottom: 1rem; font-size: 1.25rem; color: #fff; }
  .grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(220px, 1fr)); gap: 1rem; }
  .card { background: #1e1e1e; border-radius: 8px; overflow: hidden; }
  .label { padding: .5rem .75rem; font-size: .8rem; color: #aaa; white-space: nowrap;
           overflow: hidden; text-overflow: ellipsis; }
  .card img { width: 100%; display: block; }
  .card.error { border: 1px solid #c0392b; }
  .card .err { padding: .5rem .75rem; font-size: .75rem; color: #e74c3c; }
</style>
</head>
<body>
<h1>Home Edge — Mobile Screenshot Gallery (390 × 844)</h1>
<div class="grid">
${cards}
</div>
</body>
</html>`;

  fs.writeFileSync(path.join(OUT_DIR, 'index.html'), html, 'utf8');
}

main().catch(err => {
  console.error(err);
  process.exit(1);
});
