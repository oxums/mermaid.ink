// Builds tools/golden/corpus/k-series.json and tests/fixtures/corpus/encoded.json.
// Host node, no deps. Re-runnable: output is a pure function of the corpus + the two K sources.
import { readFileSync, writeFileSync, readdirSync, existsSync, mkdirSync } from 'node:fs';
import { deflateSync, deflateRawSync, gzipSync } from 'node:zlib';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const corpusDir = join(root, 'tools/golden/corpus');
const outFile = join(root, 'tests/fixtures/corpus/encoded.json');

// ---- K-series: verbatim /img|/svg|/pdf literals -------------------------------------------
// Template placeholders in kitchensink are resolved to what the test sends at runtime:
// `process.env.MAX_WIDTH + 1` is NaN there (MAX_WIDTH unset), diagramSize = [1000, 800].
const TEMPLATE_VALUES = { width: 'NaN', height: 'NaN', 'diagramSize[0]': '1000', 'diagramSize[1]': '800' };

function extractK() {
  const literals = [];
  const testLines = readFileSync(join(root, 'legacy/test/kitchensink.test.js'), 'utf8').split('\n');
  let encodedPath; // last `const encodedPath =` value seen
  testLines.forEach((line, i) => {
    if (/const encodedPath\s*=/.test(line)) encodedPath = null;
    if (encodedPath === null) {
      const m = line.match(/'([^']+)'/);
      if (m) encodedPath = m[1];
    }
    const m = line.match(/(['`])(\/(?:img|svg|pdf)\/[^'`]+)\1/);
    if (!m || /\btest\(/.test(line)) return; // skip test titles like test('/img/:code')
    const raw = m[2];
    const resolved = raw.replace(/\$\{([^}]+)\}/g, (_, k) => {
      const v = k === 'encodedPath' ? encodedPath : TEMPLATE_VALUES[k];
      if (v == null) throw new Error(`unresolved template ${k} at line ${i + 1}`);
      return v;
    });
    const prev = testLines.slice(Math.max(0, i - 2), i + 1).join('\n');
    literals.push({ source: `legacy/test/kitchensink.test.js:${i + 1}`, method: /\.options\(/.test(prev) ? 'OPTIONS' : 'GET', raw, resolved });
  });
  readFileSync(join(root, 'legacy/src/static/index.html'), 'utf8').split('\n').forEach((line, i) => {
    for (const m of line.matchAll(/(?:href|src)="(\/(?:img|svg|pdf)\/[^"]+)"/g)) {
      literals.push({ source: `legacy/src/static/index.html:${i + 1}`, method: 'GET', raw: m[1], resolved: m[1].replaceAll('&amp;', '&') });
    }
  });
  const encoded = [];
  for (const l of literals) {
    const m = l.resolved.match(/^\/(img|svg|pdf)\/([^?]+)(?:\?(.*))?$/);
    l.endpoint = m[1];
    l.query = m[3] ?? '';
    let e = encoded.find((x) => x.encoded === m[2]);
    if (!e) encoded.push((e = { id: `K${String(encoded.length + 1).padStart(2, '0')}`, encoded: m[2], first_source: l.source }));
    l.id = e.id;
  }
  return { encoded, literals };
}

// ---- T/F series ------------------------------------------------------------------------------
// The live editor state is {code, mermaid: <JSON string>}; a few F items override `mermaid`.
const MERMAID_OVERRIDES = { F18: { theme: 'forest' }, F19: null, F20: '{bad json' };
const b64url = (buf) => Buffer.from(buf).toString('base64url');
const pako = (json) => 'pako:' + b64url(deflateSync(json, { level: 9 }));

function encodeItem(id) {
  const code = readFileSync(join(corpusDir, `${id}.mmd`), 'utf8');
  const cfgFile = join(corpusDir, `${id}.config.json`);
  const mermaid = id in MERMAID_OVERRIDES
    ? MERMAID_OVERRIDES[id]
    : existsSync(cfgFile) ? JSON.stringify(JSON.parse(readFileSync(cfgFile, 'utf8'))) : '{"theme":"default"}';
  const json = JSON.stringify({ code, mermaid });
  return { json, enc: { pako: pako(json), base64: 'base64:' + b64url(json), plain: b64url(code) } };
}

const k = extractK();
writeFileSync(join(corpusDir, 'k-series.json'), JSON.stringify(k, null, 2) + '\n');

const ids = readdirSync(corpusDir).filter((f) => f.endsWith('.mmd')).map((f) => f.slice(0, -4)).sort();
const items = {};
const jsons = {};
for (const id of ids) {
  const r = encodeItem(id);
  items[id] = r.enc;
  jsons[id] = r.json;
}

// ---- D-series malformed/odd encodings --------------------------------------------------------
const pick = (pred) => ids.find((id) => /^T\d+$/.test(id) && pred(deflateSync(jsons[id], { level: 9 })));
const stdId = pick((b) => /\+/.test(b.toString('base64')) && /\//.test(b.toString('base64')));
const padId = pick((b) => b.length % 3 !== 0);
const t01 = jsons.T01;
let mod1 = items.T01.plain;
while (mod1.length % 4 !== 1) mod1 += 'A';
const D = {
  pako_upper: 'PAKO:' + items.T01.pako.slice(5),
  pako_std_pct: { subject: stdId, value: 'pako:' + deflateSync(jsons[stdId], { level: 9 }).toString('base64').replace(/=+$/, '').replaceAll('+', '%2B').replaceAll('/', '%2F') },
  pako_padded: { subject: padId, value: 'pako:' + deflateSync(jsons[padId], { level: 9 }).toString('base64').replaceAll('+', '-').replaceAll('/', '_') },
  pako_truncated: items.T01.pako.slice(0, -12),
  pako_gzip: 'pako:' + b64url(gzipSync(t01, { level: 9 })),
  pako_rawdeflate: 'pako:' + b64url(deflateRawSync(t01, { level: 9 })),
  b64_len_mod4_1: mod1,
};
if (!stdId || !padId || !D.pako_padded.value.endsWith('=')) throw new Error('no T item fits a D-series variant');

mkdirSync(dirname(outFile), { recursive: true });
writeFileSync(outFile, JSON.stringify({ items, K: Object.fromEntries(k.encoded.map((e) => [e.id, e.encoded])), D }, null, 2) + '\n');
console.log(`encoded ${ids.length} T/F items, ${k.encoded.length} K encodings (${k.literals.length} literals) -> ${outFile}`);
