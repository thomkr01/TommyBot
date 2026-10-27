// Logs into the dashboard through the real OAuth callback (against mock_discord.py), then opens every
// sidebar page, failing on console errors/panics or blank pages. Screenshots go to $E2E_OUT.
const { chromium } = require('playwright');
const base = process.env.DASHBOARD_URL || 'http://127.0.0.1:8080';
const out = process.env.E2E_OUT || '/tmp/e2e';
const fs = require('fs');
fs.mkdirSync(out, { recursive: true });

(async () => {
  const browser = await chromium.launch();
  const ctx = await browser.newContext({ viewport: { width: 1400, height: 900 } });
  await ctx.addCookies([{ name: 'oauth_state', value: 's', url: base }]);
  const page = await ctx.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(`pageerror: ${e.message}`));
  page.on('console', (m) => m.type() === 'error' && errors.push(`console: ${m.text()}`));
  page.on('response', (r) => r.status() >= 500 && errors.push(`${r.status()} ${r.url()}`));

  await page.goto(`${base}/auth/callback?code=x&state=s`);
  await page.waitForSelector('.card', { timeout: 15000 });
  await page.screenshot({ path: `${out}/servers.png` });
  await page.locator('.card').first().click();
  await page.waitForSelector('nav a, .side a, aside a', { timeout: 15000 });

  const links = [...new Set(await page.$$eval('a[href^="#/"]', (as) => as.map((a) => a.getAttribute('href'))))]
    .filter((h) => /^#\/\d+\//.test(h));
  // tabs that no sidebar link reaches
  links.push(links[0].replace(/^(#\/\d+\/).*/, '$1raid_log'));
  console.log(`${links.length} pages`);
  const failed = [];
  for (const h of links) {
    errors.length = 0;
    await page.goto(`${base}/${h}`);
    await page.waitForTimeout(700);
    const text = (await page.innerText('body')).trim();
    const name = h.replace(/[^\w]+/g, '_');
    await page.screenshot({ path: `${out}/${name}.png` });
    if (errors.length || text.length < 40) failed.push(`${h}: ${errors.join(' | ') || 'blank page'}`);
  }
  // Embed builder: the bot posts the embed, then edits the same message.
  const embeds = links[0].replace(/^(#\/\d+\/).*/, '$1embeds');
  await page.goto(`${base}/${embeds}`);
  await page.fill('input[placeholder="Name"]', 'e2e');
  await page.selectOption('select[aria-label="channel"]', { index: 1 });
  await page.locator('.row', { hasText: 'Title' }).locator('input').first().fill('Hello');
  await page.click('text=Post');
  await page.waitForSelector('text=Embed posted.', { timeout: 5000 }).catch(() => failed.push('embeds: post failed'));
  await page.click('text=Update message');
  await page.waitForSelector('text=Embed edited.', { timeout: 5000 }).catch(() => failed.push('embeds: edit failed'));
  const sent = await (await fetch('http://127.0.0.1:9100/sent')).json();
  if (!sent.some((m) => /\/messages\/1$/.test(m.path) && m.body.includes('Hello'))) failed.push('embeds: no edit reached Discord');
  await browser.close();
  if (failed.length) { console.log('FAIL\n' + failed.join('\n')); process.exit(1); }
  console.log('PASS');
})();
