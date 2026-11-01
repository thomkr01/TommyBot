// Emoji tooling. SVGs in assets/icons are the source; PNGs are only made in memory.
//   node tools/emojis.mjs check [names...] [--sheet out.png]  validate SVGs, report ink bbox/weight, draw a contact sheet
//   node tools/emojis.mjs upload [--replace]                  upload missing icons as application emojis
// Needs Playwright with Chromium (preinstalled in the cloud env). upload needs DISCORD_TOKEN and DISCORD_CLIENT_ID.
import { readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
const { chromium } = createRequire(import.meta.url)('playwright');

const DIR = new URL('../assets/icons/', import.meta.url);
const STOPS = [['0', '#FFA36C'], ['0.35', '#FF6F91'], ['0.7', '#9B7BFF'], ['1', '#4FC3F7']];
const args = process.argv.slice(2);
const cmd = args.shift();
const sheetAt = args.includes('--sheet') ? args.splice(args.indexOf('--sheet'), 2)[1] : null;
const replace = args.includes('--replace');
const only = args.filter(a => !a.startsWith('--'));
const names = readdirSync(DIR).filter(f => f.endsWith('.svg')).map(f => f.slice(0, -4))
  .filter(n => !only.length || only.includes(n)).sort();
const src = n => readFileSync(new URL(`${n}.svg`, DIR), 'utf8');

function lint(n, s) {
  const bad = [];
  if (!/^[a-z0-9_]{2,32}$/.test(n)) bad.push('bad name');
  if (!s.startsWith('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 128 128" width="128" height="128" fill="none"')) bad.push('root');
  if (/<(text|image|filter|style|script|foreignObject)\b|class=|<!--|<\?xml|data:|font|inkscape|sodipodi/i.test(s)) bad.push('forbidden content');
  if (n === 'spacer') return /^<svg[^>]*(\/>|>\s*<\/svg>)$/.test(s.trim()) ? bad : [...bad, 'spacer must be empty'];
  const grads = [...s.matchAll(/<linearGradient\b[^>]*>/g)];
  if (grads.length !== 1) bad.push(`${grads.length} gradients`);
  else if (!grads[0][0].includes(`id="${n}-grad"`) || !/gradientUnits="userSpaceOnUse" x1="16" y1="16" x2="112" y2="112"/.test(grads[0][0])) bad.push('gradient attrs');
  const stops = [...s.matchAll(/<stop offset="([\d.]+)" stop-color="(#[0-9A-Fa-f]{6})"\/>/g)].map(m => [m[1], m[2].toUpperCase()]);
  if (JSON.stringify(stops) !== JSON.stringify(STOPS)) bad.push('stops');
  if (/#(?!FFA36C|FF6F91|9B7BFF|4FC3F7)[0-9a-f]{3,6}\b|fill="(?!none|url|#)|stroke="(?!none|url)|rgb\(/i.test(s.replace(/<mask[\s\S]*?<\/mask>/g, ''))) bad.push('color outside gradient');
  if (/\d\.\d{2,}/.test(s.replace(/(offset|opacity)="[\d.]+"/g, ''))) bad.push('>1 decimal');
  if (/stroke-width="([0-9.]+)"/.test(s) && [...s.matchAll(/stroke-width="([0-9.]+)"/g)].some(m => +m[1] < 10)) bad.push('stroke <10');
  if (Buffer.byteLength(s) > 2600) bad.push(`${Buffer.byteLength(s)} bytes`);
  return bad;
}

async function withPage(f) {
  const b = await chromium.launch();
  try { return await f(await b.newPage()); } finally { await b.close(); }
}

// Renders an SVG to a 128px PNG, also returning alpha bbox and mean opacity ("ink").
async function raster(page, s, px = 128) {
  return page.evaluate(async ([s, px]) => {
    const img = new Image();
    img.src = 'data:image/svg+xml;base64,' + btoa(unescape(encodeURIComponent(s)));
    await img.decode();
    const c = document.createElement('canvas'); c.width = c.height = px;
    const g = c.getContext('2d'); g.drawImage(img, 0, 0, px, px);
    const d = g.getImageData(0, 0, px, px).data;
    let x0 = px, y0 = px, x1 = -1, y1 = -1, ink = 0;
    for (let i = 0; i < px * px; i++) {
      const a = d[i * 4 + 3]; ink += a;
      if (a > 24) { const x = i % px, y = (i / px) | 0; x0 = Math.min(x0, x); y0 = Math.min(y0, y); x1 = Math.max(x1, x); y1 = Math.max(y1, y); }
    }
    return { png: c.toDataURL('image/png').split(',')[1], box: x1 < 0 ? null : [x0, y0, x1 + 1, y1 + 1], ink: Math.round(ink / 255 / (px * px) * 100) };
  }, [s, px]);
}

async function check() {
  let fails = 0;
  const rows = await withPage(async page => {
    const out = [];
    for (const n of names) {
      const s = src(n), bad = lint(n, s);
      const { box, ink } = await raster(page, s);
      if (n !== 'spacer' && !/^(bar_|branch_|line_vertical)/.test(n) && box && (box[0] < 11 || box[1] < 11 || box[2] > 117 || box[3] > 117)) bad.push(`outside safe area ${box}`);
      if (bad.length) fails++;
      out.push(`${bad.length ? 'FAIL' : 'ok  '} ${n.padEnd(22)} box=${box} ink=${ink}% ${bad.join(', ')}`);
    }
    if (sheetAt) await sheet(page);
    return out;
  });
  console.log(rows.join('\n'));
  console.log(`${names.length} icons, ${fails} failing`);
  process.exitCode = fails ? 1 : 0;
}

// Contact sheet: each icon at 22 px on Discord dark, darker, light and the four button colors, plus 48 and 128 px.
async function sheet(page) {
  const bgs = ['#313338', '#1E1F22', '#FFFFFF', '#5865F2', '#248046', '#DA373C', '#4E5058'];
  const uri = n => 'data:image/svg+xml;base64,' + Buffer.from(src(n)).toString('base64');
  const html = `<body style="margin:0;font:11px sans-serif;background:#313338;color:#ddd;width:1100px">${names.map(n => `
    <div style="display:inline-flex;align-items:center;gap:4px;width:540px;height:56px">
      <span style="width:120px;overflow:hidden">${n}</span>
      ${bgs.map(b => `<span style="background:${b};padding:3px;border-radius:3px"><img src="${uri(n)}" width="22" height="22" style="display:block"></span>`).join('')}
      <img src="${uri(n)}" width="48" height="48"></div>`).join('')}</body>`;
  await page.setContent(html);
  await page.screenshot({ path: sheetAt, fullPage: true });
  console.log(`sheet: ${sheetAt}`);
}

async function upload() {
  const { DISCORD_TOKEN: token, DISCORD_CLIENT_ID: app } = process.env;
  if (!token || !app) throw new Error('DISCORD_TOKEN and DISCORD_CLIENT_ID must be set');
  const api = async (method, path, body) => {
    for (;;) {
      const r = await fetch(`https://discord.com/api/v10/applications/${app}/emojis${path}`, {
        method, headers: { authorization: `Bot ${token}`, 'content-type': 'application/json' }, body: body && JSON.stringify(body),
      });
      if (r.status === 429) { const w = (await r.json()).retry_after ?? 1; await new Promise(f => setTimeout(f, w * 1000)); continue; }
      if (!r.ok) throw new Error(`${method} ${path}: ${r.status} ${await r.text()}`);
      return r.status === 204 ? null : r.json();
    }
  };
  const have = new Map((await api('GET', '')).items.map(e => [e.name, e.id]));
  await withPage(async page => {
    for (const n of names) {
      if (have.has(n) && !replace) continue;
      if (have.has(n)) await api('DELETE', `/${have.get(n)}`);
      const { png } = await raster(page, src(n));
      const e = await api('POST', '', { name: n, image: `data:image/png;base64,${png}` });
      console.log(`uploaded ${n} ${e.id}`);
    }
  });
  console.log('done; restart the bot so it loads the new ids');
}

await ({ check, upload }[cmd] ?? (() => { console.error('usage: node tools/emojis.mjs check|upload'); process.exit(2); }))();
