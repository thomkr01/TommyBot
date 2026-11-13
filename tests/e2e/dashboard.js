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
  // the sidebar's server links render after the header's
  await page.waitForFunction(() => document.querySelectorAll('a[href^="#/"]').length > 10, null, { timeout: 15000 });

  const links = [...new Set(await page.$$eval('a[href^="#/"]', (as) => as.map((a) => a.getAttribute('href'))))]
    .filter((h) => /^#\/\d+\//.test(h));
  // tabs that no sidebar link reaches
  links.push(links[0].replace(/^(#\/\d+\/).*/, '$1raid_log'));
  links.push(links[0].replace(/^(#\/\d+\/).*/, '$1raider_matches'));
  links.push('#/commands', '#/update', '#/raiders', '#/raider-appeal');
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
    // A ">" in an unbraced view! attribute ends the tag, and the rest of the code shows up as page text.
    if (/move \||on:\w+=/i.test(text)) failed.push(`${h}: source code shown as text`);
  }
  // Embed builder: the bot posts the embed, then edits the same message.
  const embeds = links[0].replace(/^(#\/\d+\/).*/, '$1embeds');
  await page.goto(`${base}/${embeds}`);
  await page.fill('input[placeholder="Name"]', 'e2e');
  await page.selectOption('select[aria-label="channel"]', { index: 1 });
  // Toolbar shortcut: Ctrl+B on a selection wraps it in Discord's ** and the preview renders it bold.
  const title = page.locator('.row', { hasText: 'Title' }).locator('input').first();
  await title.fill('Hello');
  await title.selectText();
  await page.keyboard.press('Control+b');
  if ((await title.inputValue()) !== '**Hello**') failed.push('embeds: Ctrl+B did not bold');
  await page.locator('textarea[data-md]').fill('Welcome to the **server**!\n> Read the *rules* first\n- be __nice__\n- no ~~spam~~\n||secret|| and `code`');
  if (!(await page.locator('.dmsg .embed b', { hasText: 'Hello' }).count())) failed.push('embeds: preview did not render bold');
  await page.locator('.eb').screenshot({ path: `${out}/embed_builder.png` });
  await page.click('text=Post');
  await page.waitForSelector('text=Embed posted.', { timeout: 5000 }).catch(() => failed.push('embeds: post failed'));
  await page.click('text=Update message');
  await page.waitForSelector('text=Embed edited.', { timeout: 5000 }).catch(() => failed.push('embeds: edit failed'));
  const sent = await (await fetch('http://127.0.0.1:9100/sent')).json();
  if (!sent.some((m) => /\/messages\/1$/.test(m.path) && m.body.includes('Hello'))) failed.push('embeds: no edit reached Discord');
  // Notes: an edit keeps the created time and shows up in the note's history.
  await page.goto(`${base}/${links[0].replace(/^(#\/\d+\/).*/, '$1notes')}`);
  await page.fill('input[aria-label="user ID"]', '42');
  const tag = Date.now();
  await page.fill('input[aria-label="new note"]', `first note ${tag}`);
  await page.click('text=Add note');
  // Newest note first; earlier runs may have left notes behind.
  // Wait for this run's note: older notes are listed at once, and the reload that adds the new one on top would swap the card under the fill.
  await page.waitForFunction((t) => document.querySelector('input[aria-label="note text"]')?.value === t, `first note ${tag}`, { timeout: 5000 }).catch(() => failed.push('notes: added note not listed'));
  const card = page.locator('.card:has(input[aria-label="note text"])').first();
  await card.locator('input[aria-label="note text"]').fill(`edited note ${tag}`);
  await card.locator('button', { hasText: 'Save' }).click();
  await card.locator('summary', { hasText: 'History (1)' }).click({ timeout: 5000 }).catch(() => failed.push('notes: no history after edit'));
  if (!(await card.locator('li', { hasText: `first note ${tag} → edited note ${tag}` }).count())) failed.push('notes: history misses the change');
  await page.screenshot({ path: `${out}/notes_history.png` });
  // Log setup reuses the public "Logs" category and makes it private (hides it from @everyone).
  const gid = links[0].match(/^#\/(\d+)\//)[1];
  // Setup is a sensitive action; mark the session as stepped up instead of driving TOTP.
  require('child_process').execSync(`psql "${process.env.DATABASE_URL}" -qc "update dash_session set step_up_until = now() + interval '1 hour'"`);
  const setup = await page.evaluate(async (g) => {
    const csrf = (await (await fetch('/api/me')).json()).csrf;
    const { version } = await (await fetch(`/api/guilds/${g}/config`)).json();
    const r = await fetch(`/api/guilds/${g}/logging/setup`, {
      method: 'POST', headers: { 'content-type': 'application/json', 'x-csrf-token': csrf },
      body: JSON.stringify({ expected_version: version }),
    });
    return `${r.status} ${await r.text()}`;
  }, gid);
  const after = await (await fetch('http://127.0.0.1:9100/sent')).json();
  if (!after.some((m) => m.path === `/channels/300000000000000002/permissions/${gid}`)) failed.push(`log setup: public Logs category not made private (${setup})`);
  // Update page: the mock user owns the app; no updater runs here, so the page says how to start it.
  await page.goto(`${base}/#/update`);
  await page.waitForSelector('text=The updater is not running', { timeout: 5000 }).catch(() => failed.push('update: no updater notice'));
  if (!(await page.locator('header a[href="#/update"]').count())) failed.push('update: no header link for the bot owner');
  // Number fields: a ">" inside a view! attribute once ended the tag and printed the code as text.
  await page.goto(`${base}/${links[0].replace(/^(#\/\d+\/).*/, '$1automod')}`);
  await page.waitForSelector('.step input', { timeout: 5000 }).catch(() => failed.push('automod: no number field'));
  if (/num\(\)/i.test(await page.innerText('body'))) failed.push('automod: number field shows source code');
  // Cancel on a changed setting puts dropdowns back too: a plain `selected=` attribute stops counting once the user has picked.
  await page.goto(`${base}/${links[0].replace(/^(#\/\d+\/).*/, '$1logging')}`);
  // No wait here on purpose: a change made right after the page loaded once got its bar hidden again (ui/PITFALLS.md #3).
  const drop = page.locator('select:has(option[value=""]:text-is("none"))').first();
  // Wait for the channel options too: the select shows with only "none" until the channel list arrives.
  await drop.locator('option:nth-child(2)').waitFor({ state: 'attached', timeout: 5000 }).catch(() => failed.push('cancel: no channel dropdown on logging'));
  const was = await drop.inputValue();
  const other = await drop.locator('option').evaluateAll((os, w) => os.map((o) => o.value).find((v) => v !== w), was);
  if (other !== undefined) await drop.selectOption(other);
  await page.click('.bar >> text=Cancel', { timeout: 5000 }).catch(() => failed.push('cancel: no Changes detected bar'));
  if ((await drop.inputValue()) !== was) failed.push(`cancel: dropdown kept ${other}, saved is ${was || 'none'}`);
  // Leaving a page while its requests run (or fail, as while the dashboard restarts) must not read disposed signals.
  for (const mode of ['slow', 'fail']) {
    errors.length = 0;
    await page.route('**/api/**', async (r) => { await new Promise((ok) => setTimeout(ok, 800)); mode === 'slow' ? r.continue() : r.abort(); });
    for (const h of links) { await page.evaluate((x) => (location.hash = x), h); await page.waitForTimeout(1200); }
    await page.waitForTimeout(2000);
    await page.unroute('**/api/**');
    const crash = errors.filter((e) => /disposed|unreachable|panicked/.test(e));
    if (crash.length) failed.push(`page switch (${mode}): ${crash[0]}`);
  }
  await browser.close();
  if (failed.length) { console.log('FAIL\n' + failed.join('\n')); process.exit(1); }
  console.log('PASS');
})();
