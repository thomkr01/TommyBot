// Deep dashboard test: every write route for every access level, then every page as every level
// (greying, info boxes, no writes from read-only pages), then the owner's controls one by one.
// Run through tests/e2e/run.sh DEEP=1, after levels.sql. Findings print as FAIL lines; screenshots go to $E2E_OUT.
const { chromium } = require('playwright');
const { execSync } = require('child_process');
const fs = require('fs');
const base = process.env.DASHBOARD_URL || 'http://127.0.0.1:8080';
const out = process.env.E2E_OUT || '/tmp/e2e-deep';
const db = process.env.DATABASE_URL;
const only = (process.env.DEEP_ONLY || 'matrix,pages,owner,extras,contrast,overflow').split(',');
fs.mkdirSync(out, { recursive: true });
const G = '100000000000000001', G2 = '100000000000000002';
const sql = (q) => execSync(`psql "${db}" -qAtc "${q.replace(/"/g, '\\"')}"`).toString().trim();
const RANK = { none: 0, viewer: 1, editor: 2, admin: 3, owner: 4 };
// code -> [user id, level, permit perms]; mirrors mock_discord.py and levels.sql
const USERS = {
  none: ['200000000000000007', 'none', []],
  viewer: ['200000000000000005', 'viewer', ['dashboard_view']],
  cases: ['200000000000000006', 'viewer', ['dashboard_view', 'cases', 'notes_view']],
  editor: ['200000000000000004', 'editor', ['dashboard_view', 'dashboard_edit']],
  ta: ['200000000000000003', 'admin', []],
  eo: ['200000000000000002', 'owner', []],
  x: ['200000000000000001', 'owner', []],
};
const fails = [], notes = [];
const fail = (area, msg) => { fails.push(`${area}: ${msg}`); console.log(`FAIL ${area}: ${msg}`); };
const note = (area, msg) => { notes.push(`${area}: ${msg}`); console.log(`NOTE ${area}: ${msg}`); };
const stepUp = (uid, on) => sql(`update dash_session set step_up_until = ${on ? "now() + interval '1 hour'" : 'null'} where user_id = ${uid}`);

async function login(browser, code, opts = {}) {
  const ctx = await browser.newContext({ viewport: { width: 1400, height: 900 }, ...opts });
  await ctx.addCookies([{ name: 'oauth_state', value: 's', url: base }]);
  const page = await ctx.newPage();
  page.errs = [];
  page.writes = [];
  page.on('pageerror', (e) => page.errs.push(`pageerror: ${e.message}`));
  // A refused request logs "Failed to load resource ... 403"; the write log below judges those.
  page.on('console', (m) => m.type() === 'error' && !/status of 4\d\d/.test(m.text()) && page.errs.push(`console: ${m.text()}`));
  page.on('crash', () => console.log(`page crashed at ${page.url()}`));
  page.on('close', () => console.log(`page closed at ${page.url()}`));
  page.on('dialog', (d) => d.accept().catch(() => {}));
  page.on('response', (r) => {
    const q = r.request();
    if (q.method() !== 'GET' && r.url().includes('/api/') && !r.url().includes('/api/appeal/notifications')) page.writes.push({ m: q.method(), url: r.url().replace(base, ''), s: r.status(), body: q.postData() });
    if (r.status() >= 500) page.errs.push(`${r.status()} ${q.method()} ${r.url().replace(base, '')}`);
  });
  await page.goto(`${base}/auth/callback?code=${code}&state=s`);
  await page.waitForTimeout(1200);
  return { ctx, page };
}

// The dashboard allows 20 requests a second per session; wait and retry on 429.
const call = (page, m, path, body) => page.evaluate(async ([m, p, b]) => {
  const go = async (f) => { for (let i = 0; ; i++) { const r = await f(); if (r.status !== 429 || i > 20) return r; await new Promise((ok) => setTimeout(ok, 300)); } };
  const me = await (await go(() => fetch('/api/me'))).json();
  const r = await go(() => fetch(p, { method: m, headers: { 'content-type': 'application/json', 'x-csrf-token': me.csrf }, body: b === undefined ? undefined : JSON.stringify(b) }));
  return { s: r.status, t: (await r.text()).slice(0, 160) };
}, [m, path, body]);
const getJson = (page, path) => page.evaluate(async (p) => {
  for (let i = 0; i < 20; i++) { const r = await fetch(p); if (r.status !== 429) return r.ok ? r.json() : null; await new Promise((ok) => setTimeout(ok, 300)); }
  return null;
}, path);

// ---------- 1. API matrix: nobody may change what their level does not allow ----------
async function matrix(browser) {
  const g = `/api/guilds/${G}`;
  const SECT = { misc: ['owner', false], antinuke: ['owner', true], panic: ['owner', true], whitelist: ['admin', true], catch: ['admin', true] };
  const sections = ['wizard', 'whitelist', 'antinuke', 'panic', 'backups', 'automod', 'filters', 'discord_automod', 'joingate', 'joinraid', 'verification', 'tickets', 'lockdown', 'catch', 'appeals', 'misc', 'logging'];
  // [method, path, body(v) or null, min level, step-up, staff perms]
  const routes = () => [
    ...sections.map((s) => ['PUT', `${g}/config/${s}`, (v, cfg) => ({ expected_version: v, value: cfg[s] ?? {} }), ...(SECT[s] || ['editor', false])]),
    ['PUT', `${g}/permits`, (v, cfg) => ({ expected_version: v, value: cfg.permits }), 'admin', true],
    // only owners change the TA list, and only the Discord owner the Extra Owners
    ['PUT', `${g}/permits`, (v, cfg) => ({ expected_version: v, value: { ...cfg.permits, ta: [...(cfg.permits?.ta || []), '999'] } }), 'owner', true],
    ['PUT', `${g}/permits`, (v, cfg) => ({ expected_version: v, value: { ...cfg.permits, eo: [...(cfg.permits?.eo || []), '999'] } }), 'discord', true],
    ['POST', `${g}/import`, (v, cfg) => ({ expected_version: v, config: { automod: cfg.automod }, apply: false }), 'editor', false],
    ['POST', `${g}/history/1/rollback`, (v) => ({ expected_version: v }), 'admin', true],
    ['POST', `${g}/wizard/auto-setup`, (v) => ({ expected_version: v }), 'admin', true],
    ['POST', `${g}/logging/setup`, (v) => ({ expected_version: v }), 'admin', true],
    ['POST', `${g}/wizard/verify-channel`, (v) => ({ expected_version: v }), 'admin', true],
    ['POST', `${g}/catch/setup`, (v) => ({ expected_version: v }), 'admin', true],
    ['POST', `${g}/tickets/setup`, (v) => ({ expected_version: v }), 'admin', true],
    ['GET', `${g}/channel-setup`, () => undefined, 'admin', false],
    ['PUT', `${g}/channel-setup`, () => ({ draft: { items: {} } }), 'owner', false],
    ['DELETE', `${g}/channel-setup`, () => undefined, 'owner', false],
    ['POST', `${g}/channel-setup/push`, () => ({ version: '' }), 'owner', true],
    ['POST', `${g}/lockdown/lock`, () => ({ kind: 'server' }), 'editor', false],
    ['POST', `${g}/lockdown/update`, () => ({ text: 'e2e' }), 'editor', false],
    ['POST', `${g}/lockdown/unlock`, () => ({}), 'editor', false],
    ['POST', `${g}/lockdown/fix`, () => ({}), 'admin', true],
    ['POST', `${g}/discord-rules`, () => ({ name: 'e2e', words: ['zzz'] }), 'editor', false],
    ['POST', `${g}/discord-rules-migrate-words`, () => ({}), 'editor', false],
    ['POST', `${g}/backups`, () => ({}), 'editor', false],
    ['POST', `${g}/backups/999999/load`, () => ({}), 'admin', true],
    ['POST', `${g}/backups/999999/delete`, () => ({}), 'admin', true],
    ['POST', `${g}/panic/start`, () => ({}), 'admin', true],
    ['POST', `${g}/panic/end`, () => ({}), 'admin', true],
    ['POST', `${g}/score/fix`, () => ({ setting: 'none' }), 'admin', true],
    ['POST', `${g}/revert`, () => ({ ids: [] }), 'admin', true],
    ['POST', `${g}/rescue/regenerate`, () => ({}), 'discord', true],
    ['POST', `${g}/cases/999999`, () => ({ reason: 'x', proof: '', active: true }), 'viewer', false, ['cases']],
    ['POST', `${g}/cases/999999/delete`, () => ({}), 'admin', false],
    // Mod stats: owners only here, since no fixture user holds View Mod Stats (trusted admins are refused)
    ['GET', `${g}/mod_stats?days=30`, () => undefined, 'owner', false],
    ['GET', `${g}/mod_stats/42`, () => undefined, 'owner', false],
    ['POST', `${g}/notes`, () => ({ user_id: '42', text: 'e2e matrix' }), 'viewer', false, ['notes_manage']],
    ['POST', `${g}/notes/999999`, () => ({ text: 'x' }), 'viewer', false, ['notes_manage']],
    ['POST', `${g}/notes/999999/delete`, () => ({}), 'viewer', false, ['notes_manage']],
    ['POST', `${g}/notes/sweep`, () => ({ text: 'zzz' }), 'viewer', false, ['notes_manage']],
    ['POST', `${g}/reports/999999/status`, () => ({ open: false }), 'viewer', false, ['reports']],
    ['POST', `${g}/reports/blacklist`, () => ({ user_id: '43' }), 'viewer', false, ['reports']],
    ['POST', `${g}/reports/blacklist/43/remove`, () => ({}), 'viewer', false, ['reports']],
    ['POST', `${g}/appeals/999999/decide`, () => ({ accept: false }), 'viewer', false, ['appeals']],
    ['POST', `${g}/appeals/blacklist`, () => ({ user_id: '44' }), 'editor', false],
    ['POST', `${g}/appeals/blacklist/44/remove`, () => ({}), 'editor', false],
    ['POST', `${g}/appeals/blacklist/purge`, () => ({}), 'admin', false],
    ['POST', `${g}/embeds`, () => ({ name: 'e2e matrix', embed: { title: 'x' }, channel: null }), 'editor', false],
    ['POST', `${g}/embeds/999999/post`, () => ({}), 'editor', false],
    ['POST', `${g}/embeds/999999/delete`, () => ({}), 'editor', false],
    // MCP tokens: the bot owner (the mock's app owner is x) after 2FA only
    ['POST', '/api/mcp-tokens', () => ({ name: 'e2e matrix' }), 'discord', true],
    ['POST', '/api/mcp-tokens/999999/revoke', () => ({}), 'discord', true],
    // last: wipes the server's data
    ['POST', `${g}/purge`, () => ({}), 'owner', true],
  ];
  const rows = [];
  for (const [code, [uid, lvl, perms]] of Object.entries(USERS)) {
    const { ctx, page } = await login(browser, code);
    for (const up of [false, true]) {
      stepUp(uid, up);
      for (const [m, path, body, min, step, staff] of routes()) {
        // the owner's purge would wipe the data the later phases use
        if (path.endsWith('/purge') && !path.includes('blacklist') && RANK[lvl] >= RANK.owner && up) continue;
        const cfgRes = await getJson(page, `${g}/config`);
        const v = cfgRes?.version ?? 0;
        const r = await call(page, m, path, body(v, cfgRes?.config || {}));
        const want = (min === 'discord' ? code === 'x' : RANK[lvl] >= RANK[min]) && (!step || up) && (!staff || RANK[lvl] >= RANK.admin || staff.some((p) => perms.includes(p)));
        const denied = r.s === 401 || r.s === 403;
        rows.push(`${code}\t${up ? 'step' : '-'}\t${m} ${path.replace(g, '')}\t${r.s}\t${want ? 'allow' : 'deny'}`);
        if (!want && !denied) (r.s < 300 ? fail : note)('matrix', `${code}${up ? '+2FA' : ''} ${m} ${path.replace(g, '')} -> ${r.s} (expected refusal) ${r.t}`);
        if (want && denied) fail('matrix', `${code}${up ? '+2FA' : ''} ${m} ${path.replace(g, '')} -> ${r.s} (expected allowed) ${r.t}`);
        // Undo what an allowed call changed for the next user.
        if (r.s < 300 && path.endsWith('/permits')) { sql('delete from trusted_admins where user_id = 999; delete from extra_owners where user_id = 999'); }
        if (r.s < 300 && path === '/api/mcp-tokens') sql('delete from mcp_tokens');
        if (r.s < 300 && path.endsWith('/lockdown/lock')) await call(page, 'POST', `${g}/lockdown/unlock`, {});
        if (r.s < 300 && path.endsWith('/panic/start')) await call(page, 'POST', `${g}/panic/end`, {});
      }
    }
    stepUp(uid, false);
    // The second server: the mock makes everyone but the owner a stranger there.
    const r = await call(page, 'PUT', `/api/guilds/${G2}/config/automod`, { expected_version: 0, value: {} });
    if (code !== 'x' && r.s < 300) fail('matrix', `${code} changed the second server (${r.s})`);
    await ctx.close();
  }
  fs.writeFileSync(`${out}/matrix.tsv`, rows.join('\n'));
}

// ---------- 2. Every page as every level ----------
const ALL_PAGES = ['overview', 'wizard', 'misc', 'permits', 'logging', 'backups', 'channel_setup', 'lockdown', 'lockdown_settings', 'appeal_list', 'appeals', 'appeal_blacklist', 'automod', 'filters', 'discord_automod', 'whitelist', 'automod_panic', 'antinuke', 'antinuke_filters', 'antinuke_panic', 'joingate', 'joingate_log', 'joinraid', 'raid_log', 'verification', 'cases', 'mod_stats', 'notes', 'report_list', 'report_blacklist', 'tickets', 'catch', 'revert', 'history', 'embeds', 'rescue'];
const PAGES = process.env.DEEP_PAGES ? process.env.DEEP_PAGES.split(',') : ALL_PAGES;
// Buttons that only read, switch view, open a dialog or copy.
const HARMLESS = /^(refresh|show|hide|view|preview|copy|next|back|previous|prev|close|cancel|export|download|search|filter|load more|more|less|details|open|test|check|reset view|history|\d+|‹|›|«|»|<|>|←|→)$/i;

async function pages(browser) {
  for (const code of Object.keys(USERS)) {
    const [uid, lvl] = USERS[code];
    stepUp(uid, true); // so a refused write is down to the level, not 2FA
    const { ctx, page } = await login(browser, code);
    const acc = await getJson(page, `/api/guilds/${G}/access`);
    if (lvl === 'none') {
      // A member without a permit must not see the server at all.
      const list = await page.innerText('body');
      if (/E2E Server/.test(list) && !/no access|not have access|permit/i.test(list)) note('pages', `none-user sees "E2E Server" in the server list`);
    }
    for (const p of PAGES) {
      page.errs.length = 0;
      page.writes.length = 0;
      await page.goto(`${base}/#/${G}/${p}`);
      await page.waitForTimeout(900);
      const want = acc?.pages?.[p] || 'hidden';
      const main = page.locator('.shell > main, main').last();
      const text = await page.innerText('body');
      const shot = `${out}/${code}_${p}.png`;
      if (/move \||on:\w+=/i.test(text)) fail('pages', `${code} ${p}: source code shown as text`);
      const info = await page.locator('p.info[role="note"]').count();
      const ro = await page.locator('fieldset.ro[disabled]').count();
      const noAccess = /No access/i.test(text);
      if (want === 'read' && !(info && ro)) fail('pages', `${code} ${p}: read-only but no info box/greyed fieldset`);
      if (want === 'hidden' && !noAccess) fail('pages', `${code} ${p}: hidden but content shown`);
      if (want === 'full' && (info || noAccess)) fail('pages', `${code} ${p}: full access but shown as ${noAccess ? 'no access' : 'read-only'}`);
      // The sidebar link follows the access.
      const link = await page.locator(`nav a[href="#/${G}/${p}"]`).count();
      if (want === 'hidden' && link) fail('pages', `${code} ${p}: hidden page still in the menu`);
      // Read-only and hidden pages: any control still enabled is clicked, and no write may succeed.
      if (want !== 'full' || RANK[lvl] < RANK.editor) {
        const ctrls = main.locator('button:enabled, input:enabled:not([type=hidden]), select:enabled, textarea:enabled, [role=button], [contenteditable=true]');
        const n = await ctrls.count();
        const live = [];
        for (let i = 0; i < n; i++) {
          const c = ctrls.nth(i);
          if (!(await c.isVisible().catch(() => false))) continue;
          const inRo = await c.evaluate((e) => !!e.closest('fieldset.ro'));
          const label = ((await c.innerText().catch(() => '')) || (await c.getAttribute('aria-label')) || (await c.getAttribute('placeholder')) || (await c.evaluate((e) => e.tagName))).trim().slice(0, 40);
          if (!HARMLESS.test(label)) live.push(`${label}${inRo ? '' : ' (outside greyed area)'}`);
          await c.click({ timeout: 800, trial: false }).catch(() => {});
          await page.waitForTimeout(150);
          if (!page.url().includes(`/${p}`)) { await page.goto(`${base}/#/${G}/${p}`); await page.waitForTimeout(600); }
        }
        await page.waitForTimeout(500);
        if (want === 'read' && live.length) note('pages', `${code} ${p}: still clickable on a read-only page: ${[...new Set(live)].slice(0, 8).join(', ')}`);
        const done = page.writes.filter((w) => w.s < 300);
        if (done.length) fail('pages', `${code} ${p}: a write succeeded from a ${want} page: ${done.map((w) => `${w.m} ${w.url} ${w.s}`).join(', ')}`);
        const tried = page.writes.filter((w) => w.s >= 300);
        if (tried.length) note('pages', `${code} ${p}: UI sent a write the server refused: ${tried.map((w) => `${w.m} ${w.url.replace(/\/api\/guilds\/\d+/, '')} ${w.s}`).join(', ')}`);
      }
      if (page.errs.length) fail('pages', `${code} ${p}: ${[...new Set(page.errs)].slice(0, 3).join(' | ')}`);
      await page.screenshot({ path: shot, fullPage: true });
    }
    stepUp(uid, false);
    await ctx.close();
  }
}

// ---------- 3. Owner: every number field, toggle, select and save on every settings page ----------
async function owner(browser) {
  const [uid] = USERS.x;
  const { ctx, page } = await login(browser, 'x');
  stepUp(uid, true); // after login: it marks existing sessions only
  for (const p of PAGES) {
    page.errs.length = 0;
    await page.goto(`${base}/#/${G}/${p}`);
    await page.waitForTimeout(1000);
    const main = page.locator('main').last();
    // Number fields: + raises the value, - lowers it again.
    const steps = main.locator('.step');
    const ns = await steps.count();
    for (let i = 0; i < ns; i++) {
      const s = steps.nth(i);
      if (!(await s.isVisible().catch(() => false))) continue;
      const inp = s.locator('input').first();
      const before = await inp.inputValue().catch(() => null);
      const plus = s.locator('button[aria-label="more"]');
      const minus = s.locator('button[aria-label="less"]');
      if (await plus.isEnabled().catch(() => false)) {
        await plus.click();
        const up = await inp.inputValue();
        if (up === before) fail('owner', `${p}: number field #${i} + did not change ${before}`);
        if (await minus.isEnabled().catch(() => false)) {
          await minus.click();
          // A field without a default stored as 0 (empty means off) may come back empty.
          const after = await inp.inputValue();
          if (after !== before && !(before === '0' && after === '')) fail('owner', `${p}: number field #${i} + then - gave ${after}, not ${before}`);
        } else fail('owner', `${p}: number field #${i} - disabled after +`);
      }
      // Typing out of range must be refused or clamped, never kept silently (first 3 fields per page).
      if (i < 3 && await inp.isEditable().catch(() => false)) {
        await inp.fill('999999999');
        await inp.press('Tab');
        await page.waitForTimeout(150);
        const v = await inp.inputValue();
        const shown = await s.locator('xpath=following-sibling::span[contains(@class,"err")]').count();
        if (v === '999999999' && !shown) note('owner', `${p}: number field #${i} kept 999999999 without a message`);
        await inp.fill(before ?? '');
        await inp.press('Tab');
      }
    }
    // The values above are back to the start: a save bar left over means the form compares badly.
    // Toggles: flip one, see the save bar, save, reload, check it stayed.
    const sw = main.locator('input.sw:enabled');
    const nsw = await sw.count();
    // Overview toggles save at once, without the save bar (by design).
    if (nsw && p !== 'overview') {
      const t = sw.first();
      const was = await t.isChecked();
      await t.click({ force: true });
      const bar = page.locator('.bar', { hasText: 'Changes detected' });
      if (!(await bar.isVisible().catch(() => false))) { fail('owner', `${p}: toggle gave no "Changes detected" bar`); }
      else {
        await bar.locator('button', { hasText: 'Save' }).click();
        await page.waitForTimeout(1200);
        const toast = await page.locator('.toast').innerText().catch(() => '');
        const bad = await page.locator('.toast.bad').count();
        if (bad) fail('owner', `${p}: saving one toggle was refused: ${toast}`);
        else {
          await page.reload();
          await page.waitForTimeout(1200);
          const now = await main.locator('input.sw:enabled').first().isChecked().catch(() => null);
          if (now === was) fail('owner', `${p}: toggle change was not kept after reload`);
          // put it back
          await main.locator('input.sw:enabled').first().click({ force: true });
          const b2 = page.locator('.bar', { hasText: 'Changes detected' });
          if (await b2.isVisible().catch(() => false)) { await b2.locator('button', { hasText: 'Save' }).click(); await page.waitForTimeout(1000); }
        }
      }
      // Cancel puts every toggle back.
      const all = await sw.evaluateAll((es) => es.map((e) => e.checked));
      for (let i = 0; i < Math.min(nsw, 6); i++) await sw.nth(i).click({ force: true }).catch(() => {});
      const bar2 = page.locator('.bar', { hasText: 'Changes detected' });
      if (await bar2.isVisible().catch(() => false)) {
        await bar2.locator('button', { hasText: 'Cancel' }).click();
        await page.waitForTimeout(700); // the bar slides out
        const back = await sw.evaluateAll((es) => es.map((e) => e.checked));
        if (JSON.stringify(back) !== JSON.stringify(all)) fail('owner', `${p}: Cancel did not restore the toggles`);
        if (await bar2.isVisible().catch(() => false)) fail('owner', `${p}: save bar2 still up after Cancel`);
      }
    }
    await page.waitForTimeout(700);
    // Leftover bar with no change = dirty check broken.
    if (await page.locator('.bar', { hasText: 'Changes detected' }).isVisible().catch(() => false)) fail('owner', `${p}: "Changes detected" with nothing changed`);
    if (page.errs.length) fail('owner', `${p}: ${[...new Set(page.errs)].slice(0, 3).join(' | ')}`);
  }
  stepUp(uid, false);
  await ctx.close();
}


// Text contrast of every link/button, at rest and under the mouse (WCAG ratio; under 3 is unreadable).
const contrastOf = (el) => {
  const rgb = (c) => { const m = c.match(/[\d.]+/g) || []; return m.map(Number); };
  const lum = ([r, g, b]) => { const f = (v) => { v /= 255; return v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4; }; return 0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b); };
  const cs = getComputedStyle(el);
  const fg = rgb(cs.color);
  // Blend the backgrounds from the element down to the first solid one; a gradient counts as the body colour.
  const layers = [];
  let bg = null;
  for (let e = el; e; e = e.parentElement) {
    const s = getComputedStyle(e);
    if (s.backgroundImage !== 'none' && e === el) return null;
    if (s.backgroundImage !== 'none') { bg = rgb(getComputedStyle(document.body).backgroundColor).slice(0, 3); break; }
    const c = rgb(s.backgroundColor);
    const a = c.length === 3 ? 1 : c[3];
    if (a >= 0.99) { bg = c.slice(0, 3); break; }
    if (a > 0) layers.push([c.slice(0, 3), a]);
  }
  if (!bg) bg = [255, 255, 255];
  for (const [c, a] of layers.reverse()) bg = bg.map((v, i) => v * (1 - a) + c[i] * a);
  if (!bg || !el.innerText.trim()) return null;
  const [a, b] = [lum(fg) + 0.05, lum(bg) + 0.05];
  return Math.round((Math.max(a, b) / Math.min(a, b)) * 10) / 10;
};
async function hoverContrast(page, where) {
  const bad = new Set();
  const els = page.locator('nav a, nav summary, header a, header button, header summary, main a, main button:visible, main summary, main .card');
  const n = Math.min(await els.count(), 45);
  for (let i = 0; i < n; i++) {
    const e = els.nth(i);
    if (!(await e.isVisible().catch(() => false))) continue;
    const label = (await e.innerText().catch(() => '')).trim().split('\n')[0].slice(0, 30);
    const rest = await e.evaluate(contrastOf).catch(() => null);
    await e.hover({ timeout: 500 }).catch(() => {});
    await page.waitForTimeout(220); // transitions
    const hov = await e.evaluate(contrastOf).catch(() => null);
    if (rest !== null && rest < 3) bad.add(`"${label}" ${rest}:1`);
    if (hov !== null && hov < 3) bad.add(`"${label}" on hover ${hov}:1`);
  }
  await page.mouse.move(0, 0);
  if (bad.size) fail('contrast', `${where}: ${[...bad].slice(0, 6).join(', ')}`);
}

// ---------- 4. Server switcher, Auto Setup, toasts, 2FA prompt, theme, mobile ----------
async function extras(browser) {
  const [uid] = USERS.x;
  {
    const { ctx, page } = await login(browser, 'x');
    // Server switcher: the server card in the menu leads to the list, and the second server opens.
    await page.goto(`${base}/#/${G}/automod`);
    await page.waitForTimeout(1000);
    await page.locator('nav a.srv').click();
    await page.waitForTimeout(800);
    if (!page.url().endsWith('#/')) fail('switcher', `server card did not open the server list (${page.url()})`);
    const cards = await page.locator('.card', { hasText: 'Second Server' }).count();
    if (!cards) fail('switcher', 'second server missing from the list');
    else {
      await page.locator('.card', { hasText: 'Second Server' }).first().click();
      await page.waitForTimeout(1200);
      if (!page.url().includes(G2)) fail('switcher', `clicking Second Server opened ${page.url()}`);
      const name = await page.locator('nav a.srv').innerText().catch(() => '');
      if (!/Second Server/.test(name)) fail('switcher', `menu still names "${name}" after switching`);
      await page.screenshot({ path: `${out}/switch_second.png` });
    }
    // refresh button in the server card must not navigate away
    await page.goto(`${base}/#/${G}/automod`);
    await page.waitForTimeout(800);
    await page.locator('nav a.srv button.refresh').click().catch(() => fail('switcher', 'no refresh button'));
    await page.waitForTimeout(500);
    if (!page.url().includes('/automod')) fail('switcher', 'refresh server navigated away');

    // 2FA prompt: a sensitive save without step-up asks for the code.
    stepUp(uid, false);
    await page.goto(`${base}/#/${G}/antinuke`);
    await page.waitForTimeout(1200);
    const t = page.locator('main input.sw:enabled').first();
    if (await t.count()) {
      await t.click({ force: true });
      await page.locator('.bar button', { hasText: 'Save' }).click().catch(() => {});
      await page.waitForTimeout(1000);
      const dlg = await page.getByText("Confirm it's you").count();
      if (!dlg) fail('2fa', 'sensitive save without 2FA gave no code prompt');
      await page.screenshot({ path: `${out}/stepup_prompt.png` });
      await page.keyboard.press('Escape');
      await page.locator('.bar button', { hasText: 'Cancel' }).click().catch(() => {});
    }
    // Auto Setup from the wizard, stepped up: it must finish with a success toast.
    stepUp(uid, true);
    await page.goto(`${base}/#/${G}/wizard`);
    await page.waitForTimeout(1200);
    const as = page.locator('main button', { hasText: /^\s*Auto Setup\s*$/i }).first();
    if (!(await as.count())) fail('auto setup', 'no Auto Setup button on the wizard');
    else {
      page.errs.length = 0;
      await as.click();
      await page.waitForFunction(() => !/Running Auto Setup/.test(document.querySelector('.toast')?.innerText || ''), null, { timeout: 20000 }).catch(() => {});
      const toast = await page.locator('.toast').innerText().catch(() => '');
      const bad = await page.locator('.toast.bad').count();
      await page.screenshot({ path: `${out}/auto_setup.png` });
      if (bad || !toast) fail('auto setup', `result toast: "${toast}"${bad ? ' (error)' : ''}`);
      else note('auto setup', `toast: ${toast.replace(/\s+/g, ' ').slice(0, 120)}`);
      if (page.errs.length) fail('auto setup', page.errs.join(' | '));
      // Toast: click closes it.
      if (toast) {
        await page.locator('.toast').click();
        await page.waitForTimeout(900);
        if (await page.locator('.toast:not(.out)').isVisible().catch(() => false)) fail('toast', 'click did not close the toast');
      }
    }
    // Error toast stays until clicked (PITFALLS 6).
    await page.goto(`${base}/#/${G}/notes`);
    await page.waitForTimeout(800);
    await page.fill('input[aria-label="user ID"]', 'abc');
    await page.fill('input[aria-label="new note"]', 'x');
    await page.click('text=Add note');
    await page.waitForTimeout(6500);
    if (!(await page.locator('.toast.bad').isVisible().catch(() => false))) note('toast', 'an error toast for a bad user ID was gone after 6.5 s (or never shown)');
    await page.screenshot({ path: `${out}/toast_error.png` });
    // Theme: toggle flips data-theme and survives a reload.
    const theme = () => page.evaluate(() => document.documentElement.getAttribute('data-theme'));
    const t0 = await theme();
    await page.click('button[aria-label="toggle light and dark theme"]');
    const t1 = await theme();
    if (t0 === t1) fail('theme', `toggle did not change data-theme (${t0})`);
    await page.reload();
    await page.waitForTimeout(800);
    if ((await theme()) !== t1) fail('theme', 'theme not kept after reload');
    await ctx.close();
  }
  // Mobile: no sideways scroll on any page, the menu button opens and closes the menu.
  {
    const { ctx, page } = await login(browser, 'x', { viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });
    stepUp(uid, true);
    const wide = [];
    for (const p of PAGES) {
      await page.goto(`${base}/#/${G}/${p}`);
      await page.waitForTimeout(700);
      const [sw, w] = await page.evaluate(() => [document.documentElement.scrollWidth, window.innerWidth]);
      if (sw > w + 1) {
        const culprit = await page.evaluate(() => { const w = window.innerWidth; const e = [...document.querySelectorAll('main *')].find((x) => x.getBoundingClientRect().right > w + 1); return e ? `${e.tagName.toLowerCase()}.${e.className}`.slice(0, 60) : '?'; });
        wide.push(`${p} (${sw}px, ${culprit})`);
        await page.screenshot({ path: `${out}/mobile_wide_${p}.png`, fullPage: true });
      }
    }
    if (wide.length) fail('mobile', `sideways scroll at 390px: ${wide.join(', ')}`);
    await page.goto(`${base}/#/${G}/automod`);
    await page.waitForTimeout(800);
    await page.click('button.menu-btn');
    await page.waitForTimeout(300);
    if (!(await page.locator('nav.open').count())) fail('mobile', 'menu button did not open the menu');
    await page.screenshot({ path: `${out}/mobile_menu.png` });
    await page.locator('nav.open a[href$="/filters"]').click().catch(() => {});
    await page.waitForTimeout(500);
    if (await page.locator('nav.open').count()) fail('mobile', 'menu stayed open after picking a page');
    if (!page.url().endsWith('/filters')) fail('mobile', 'menu link did not open the page');
    await ctx.close();
  }
}

async function contrast(browser) {
  stepUp(USERS.x[0], true);
  const { ctx, page } = await login(browser, 'x');
  await page.goto(`${base}/#/${G}/overview`);
  await page.waitForTimeout(800);
    for (const th of ['light', 'dark']) {
      for (const h of ['#/update', '#/commands', '#/']) {
        await page.goto(`${base}/${h}`);
        await page.waitForTimeout(700);
        await page.evaluate((t) => document.documentElement.setAttribute('data-theme', t), th);
        await hoverContrast(page, `${th} ${h}`);
      }
      await page.evaluate((t) => document.documentElement.setAttribute('data-theme', t), th);
      for (const p of PAGES) {
        await page.goto(`${base}/#/${G}/${p}`);
        await page.waitForTimeout(700);
        await page.evaluate((t) => document.documentElement.setAttribute('data-theme', t), th);
        await page.screenshot({ path: `${out}/theme_${th}_${p}.png` });
        await hoverContrast(page, `${th} ${p}`);
      }
    }
  await ctx.close();
}

// Text that leaves its box, is cut off, or lies on top of other text. Returns one line per problem.
const findOverflow = () => {
  const vis = (el) => el.checkVisibility({ opacityProperty: true, visibilityProperty: true });
  const isBox = (el) => {
    const c = getComputedStyle(el);
    const bg = c.backgroundColor !== 'rgba(0, 0, 0, 0)' && c.backgroundColor !== 'transparent';
    return el === document.body || bg || parseFloat(c.borderTopWidth) > 0 || parseFloat(c.borderLeftWidth) > 0 || c.backgroundImage !== 'none';
  };
  const name = (el) => `${el.tagName.toLowerCase()}${el.className && typeof el.className === 'string' ? '.' + el.className.trim().split(/\s+/).join('.') : ''}`;
  const bad = [], seen = [], rects = [];
  const w = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  for (let n; (n = w.nextNode());) {
    const t = n.textContent.trim();
    const el = n.parentElement;
    if (!t || !el || el.closest('script,style,svg,option,select,[aria-hidden=true]') || !vis(el)) continue;
    const r = document.createRange(); r.selectNodeContents(n);
    const rs = [...r.getClientRects()].filter((x) => x.width > 1 && x.height > 1);
    // skip text that is off screen on purpose (closed mobile menu) or scrolled inside a scroll box
    if (!rs.length || rs.every((x) => x.right <= 0 || x.left >= innerWidth)) continue;
    let box = el; while (box !== document.body && !isBox(box)) box = box.parentElement;
    const b = box.getBoundingClientRect();
    const out = rs.some((x) => x.left < b.left - 1 || x.right > b.right + 1 || x.top < b.top - 2 || x.bottom > b.bottom + 2);
    const scrolls = (e) => /auto|scroll/.test(getComputedStyle(e).overflowY + getComputedStyle(e).overflowX);
    if (out && box !== document.body && !scrolls(box)) bad.push(`out of ${name(box)}: "${t.slice(0, 40)}"`);
    if (rs.some((x) => x.right > innerWidth + 1)) bad.push(`past screen edge: "${t.slice(0, 40)}"`);
    // cut off by an ancestor with overflow hidden (no ellipsis on purpose)
    for (let a = el; a && a !== document.body; a = a.parentElement) {
      const c = getComputedStyle(a);
      if (c.overflowX !== 'visible' && c.overflowX !== 'auto' && c.overflowX !== 'scroll' && c.textOverflow !== 'ellipsis') {
        const ab = a.getBoundingClientRect();
        if (rs.some((x) => x.right > ab.right + 1 || x.left < ab.left - 1)) { bad.push(`cut off by ${name(a)}: "${t.slice(0, 40)}"`); break; }
      }
    }
    // squeezed: text wrapped into a thin column of many lines inside a much wider box
    if (rs.length >= 4 && box !== document.body && el.getBoundingClientRect().width < 0.3 * b.width && t.split(/\s+/).length > rs.length) bad.push(`squeezed in ${name(box)}: "${t.slice(0, 40)}"`);
    rs.forEach((x) => rects.push({ x, t, el }));
  }
  // boxes (buttons, fields, cards) that stick out of the box around them
  for (const el of document.querySelectorAll('main *')) {
    if (!vis(el) || !isBox(el) || /fixed|absolute|sticky/.test(getComputedStyle(el).position)) continue;
    const r = el.getBoundingClientRect();
    if (r.width < 2 || r.right <= 0 || r.left >= innerWidth) continue;
    let p = el.parentElement; while (p && p !== document.body && !isBox(p)) p = p.parentElement;
    if (!p || p === document.body || /auto|scroll/.test(getComputedStyle(p).overflowX)) continue;
    const pr = p.getBoundingClientRect();
    if (r.left < pr.left - 1 || r.right > pr.right + 1) bad.push(`${name(el)} sticks out of ${name(p)}${el.textContent.trim() ? ` ("${el.textContent.trim().slice(0, 30)}")` : ''}`);
  }
  // text on top of other text
  for (let i = 0; i < rects.length; i++) for (let j = i + 1; j < rects.length; j++) {
    const a = rects[i], c = rects[j];
    if (a.el === c.el || a.el.contains(c.el) || c.el.contains(a.el)) continue;
    const ox = Math.min(a.x.right, c.x.right) - Math.max(a.x.left, c.x.left), oy = Math.min(a.x.bottom, c.x.bottom) - Math.max(a.x.top, c.x.top);
    // line boxes include leading, so only count a real overlap of the glyphs
    if (ox > 3 && oy > 0.4 * Math.min(a.x.height, c.x.height)) bad.push(`overlaps: "${a.t.slice(0, 30)}" / "${c.t.slice(0, 30)}"`);
  }
  return [...new Set(bad)];
};

async function overflow(browser) {
  stepUp(USERS.x[0], true);
  for (const [vw, vh] of [[1400, 900], [1024, 800], [390, 844]]) {
    const { ctx, page } = await login(browser, 'x', { viewport: { width: vw, height: vh } });
    for (const h of ['#/', '#/update', '#/commands', ...PAGES.map((p) => `#/${G}/${p}`)]) {
      await page.goto(`${base}/${h}`);
      await page.waitForTimeout(800);
      // open collapsed parts so their text is measured too
      await page.evaluate(() => document.querySelectorAll('main details:not(.ico):not(header details)').forEach((d) => (d.open = true)));
      await page.waitForTimeout(150);
      const bad = await page.evaluate(findOverflow);
      // Every info card opens on focus and stays inside the page area and the screen.
      const tips = page.locator('main .tip');
      for (let i = 0, n = await tips.count(); i < n; i++) {
        const t = tips.nth(i);
        if (!(await t.isVisible().catch(() => false))) continue;
        await t.focus();
        await page.waitForTimeout(260);
        const r = await t.evaluate((e) => { const b = e.querySelector('.bubble').getBoundingClientRect(), m = e.closest('main').getBoundingClientRect(); return { ok: +getComputedStyle(e.querySelector('.bubble')).opacity > 0.9 && b.left >= m.left - 1 && b.right <= Math.min(m.right, innerWidth) + 1 && b.top >= 0, text: e.textContent.trim().slice(0, 30) }; });
        if (!r.ok) bad.push(`info card off the page or not shown: "${r.text}"`);
      }
      if (bad.length) {
        fail('overflow', `${vw}px ${h}: ${bad.slice(0, 8).join(' | ')}${bad.length > 8 ? ` (+${bad.length - 8})` : ''}`);
        await page.screenshot({ path: `${out}/overflow_${vw}_${h.replace(/\W+/g, '_')}.png`, fullPage: true });
      }
    }
    await ctx.close();
  }
}

(async () => {
  const browser = await chromium.launch();
  for (const [n, f] of Object.entries({ matrix, pages, owner, extras, contrast, overflow })) {
    if (!only.includes(n)) continue;
    console.log(`== ${n}`);
    try { await f(browser); } catch (e) { fail(n, `crashed: ${e.stack.split('\n').slice(0, 3).join(' ')}`); }
  }
  await browser.close();
  fs.writeFileSync(`${out}/result.txt`, [...fails.map((f) => `FAIL ${f}`), ...notes.map((n) => `NOTE ${n}`)].join('\n'));
  console.log(`${fails.length} FAIL, ${notes.length} NOTE`);
  process.exit(fails.length ? 1 : 0);
})();
