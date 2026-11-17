// Records the Node reference helpers' behaviour as JSON vectors for the Rust fork.
// Run inside the mink-golden image (see tools/golden/README.md, section "Vectors").
// Env: REPO (repo root inside the image, default /home/pptruser), OUT (default /out), LIT_ROOT (default /host),
//      GEN_DATE (stored in meta.json; set it so reruns are byte-identical).
import fs from 'node:fs';
import path from 'node:path';
import zlib from 'node:zlib';
import { pathToFileURL } from 'node:url';
import { createRequire } from 'node:module';

const REPO = process.env.REPO || '/home/pptruser';
const OUT = process.env.OUT || '/out';
// The image's test/ dir is empty (.dockerignore), so literal sources come from the host checkout mounted at /host.
const LIT_ROOT = process.env.LIT_ROOT || '/host';
const SEED = 0x5eed;
const imp = (p) => import(pathToFileURL(path.join(REPO, p)).href);

const getOptionsFromCode = (await imp('src/helpers/getOptionsFromCode.js')).default;
const U = await imp('src/helpers/utils.js');
const CK = await imp('src/helpers/createCacheKey.js');
const createCacheKey = CK.default;
const { getQueryKey } = CK;

// ---------- utilities ----------
function mulberry32(a) {
  return () => {
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
const rand = mulberry32(SEED);
const ri = (n) => Math.floor(rand() * n);
const pick = (arr) => arr[ri(arr.length)];

const uniq = (arr) => [...new Set(arr)];
const bitsHex = (n) => {
  const b = Buffer.alloc(8);
  b.writeDoubleBE(n);
  return b.toString('hex');
};
const num = (n) => (n === null ? null : { s: String(n), bits: bitsHex(n) });
const valueType = (v) =>
  v === null ? 'null' : Array.isArray(v) ? 'array' : v instanceof String ? 'string-object' : typeof v;
// Run fn; return {out} or {error} (message of whatever was thrown).
const attempt = (fn) => {
  try {
    const out = fn();
    return { out: out === undefined ? null : out };
  } catch (e) {
    return { error: e instanceof Error ? e.message : String(e) };
  }
};

// One JSON value per line inside the top-level array(s): diffable, still compact.
function write(name, header, key, rows) {
  const body = rows.map((r) => '  ' + JSON.stringify(r)).join(',\n');
  const head = JSON.stringify(header).slice(1, -1);
  const text = `{${head}${head ? ',' : ''}\n "${key}": [\n${body}\n ]\n}\n`;
  JSON.parse(text); // self-check: output is valid JSON
  fs.writeFileSync(path.join(OUT, name), text);
  console.log(`${name}: ${rows.length} entries, ${text.length} bytes`);
}

// ---------- decode.json ----------
const literals = [];
for (const [file, src] of [
  ['legacy/test/kitchensink.test.js', 'kitchensink'],
  ['legacy/src/static/index.html', 'index.html'],
]) {
  const text = fs.readFileSync(path.join(LIT_ROOT, file), 'utf8');
  const found = [
    ...[...text.matchAll(/\/(?:img|svg|pdf)\/(?:<mark>)?([A-Za-z0-9+_=:%.~-]{8,})/g)].map((m) => m[1]),
    ...[...text.matchAll(/['"`]((?:pako|base64):[A-Za-z0-9+/_=-]+)['"`]/g)].map((m) => m[1]),
  ];
  for (const f of found) literals.push({ input: decodeURIComponent(f), src });
}

const b64 = (buf, { url = false, pad = true } = {}) => {
  let s = Buffer.from(buf).toString('base64');
  if (url) s = s.replace(/\+/g, '-').replace(/\//g, '_');
  return pad ? s : s.replace(/=+$/, '');
};
const docs = [
  JSON.stringify({ code: 'graph TD\nA-->B', mermaid: '{"theme":"dark"}' }),
  JSON.stringify({ code: 'graph TD\nA-->B', mermaid: { theme: 'forest' } }),
  JSON.stringify({ code: 'graph TD\nA-->B', mermaid: null }),
  JSON.stringify({ code: 'graph TD\nA-->B', mermaid: 42 }),
  JSON.stringify({ code: 'graph TD\nA-->B', mermaid: '{bad json' }),
  JSON.stringify({ code: 'graph TD\nA-->B' }),
  JSON.stringify({ mermaid: '{}' }),
  JSON.stringify({ code: 'graph TD\nA[héllo 中文 😀] --> B[50% off]\n  C -- "a b" --> D' }),
  JSON.stringify({ code: 'pie\n "a:b" : 1', mermaid: '{}', autoSync: true, updateDiagram: false }),
  '{"code":null}',
  '{"code":1}',
  '{"code":"\\ud800 lone"}',
  '{"code":1e400}',
  '{"__proto__":{"x":1},"code":"a"}',
  '{"code":"a","code":"b"}',
  '﻿{"code":"bom"}',
  ' {"code":"ws"} ',
  '1',
  'null',
  'false',
  '[]',
  '"s"',
  '{}',
  '-0',
  'graph TD\nA-->B',
  'sequenceDiagram\nA->>B: 100% done\n  B-->>A: ok',
];
const edge = [];
for (const d of docs) {
  const raw = Buffer.from(d, 'utf8');
  for (const url of [false, true])
    for (const pad of [true, false]) {
      edge.push(b64(raw, { url, pad }));
      edge.push('base64:' + b64(raw, { url, pad }));
      edge.push('pako:' + b64(zlib.deflateSync(raw), { url, pad }));
      edge.push('pako:' + b64(zlib.gzipSync(raw), { url, pad }));
      edge.push('pako:' + b64(zlib.deflateRawSync(raw), { url, pad }));
    }
  edge.push('PAKO:' + b64(zlib.deflateSync(raw), { url: true, pad: false }));
  edge.push('pako:' + b64(zlib.deflateSync(raw, { level: 0 })));
}
const okPayload = b64(Buffer.from('{"code":"graph TD\\nA-->B"}'), { url: true, pad: false });
for (const p of ['pako', 'base64', 'PAKO', 'Base64', 'foo', 'constructor', 'toString', 'hasOwnProperty',
  'valueOf', '__proto__', 'isPrototypeOf', 'propertyIsEnumerable', 'toLocaleString', '__defineGetter__', '', ' pako']) {
  edge.push(`${p}:`, `${p}:${okPayload}`, `${p}:abc`, `${p}:{"code":"x"}`, `${p}:1`, `${p}:null`, `${p}:${okPayload}:extra`);
}
edge.push(
  '', 'a:b:', ':', '::', 'a', 'ab', 'abc', 'abcd', 'abcde', 'eyJjb', 'e', '=', '====', 'ab==cd', 'ab=cd==',
  'graph TD;A-->B', 'graph TD\nA-->B', 'A: B', '%7B%22code%22%3A%22a%22%7D', '100%', 'a b c d', 'eyJj b2Rl IjoiYSJ9',
  'eyJjb2RlIjoiYSJ9\n', ' eyJjb2RlIjoiYSJ9', 'eyJjb2RlIjoiYSJ9!!', '!!eyJjb2RlIjoiYSJ9', 'eyJjb2RlIjoiYSJ9.',
  b64(Buffer.from([0xff, 0xfe, 0xc3, 0x28, 0xa0, 0xa1])), b64(Buffer.from([0x7b, 0x22, 0x63, 0x6f, 0xff])),
  'pako:' + b64(zlib.deflateSync(Buffer.from([0x7b, 0xff, 0x7d]))),
  'pako:' + b64(zlib.deflateSync(Buffer.from('{"code":"a"}')).subarray(0, 8)),
  'pako:' + b64(Buffer.concat([zlib.deflateSync(Buffer.from('{"code":"a"}')), Buffer.from('trailing')])),
  'pako:' + b64(Buffer.concat([zlib.gzipSync(Buffer.from('{"code":"a"}')), zlib.gzipSync(Buffer.from('{"code":"b"}'))])),
  'pako:' + b64(zlib.deflateSync(Buffer.from(''))),
  'pako:eJw', 'pako:eNo', 'pako:%%%', 'pako:eNpLTEoGAAJNASc=', 'pako: eNpLTEoGAAJNASc',
);

const decodeRow = (input, src) => {
  const row = { input, src };
  try {
    const v = getOptionsFromCode(input);
    row.outcome = 'ok';
    row.value_type = valueType(v);
    const j = JSON.stringify(v);
    if (j !== undefined) row.value = JSON.parse(j);
  } catch (e) {
    row.outcome = e?.constructor?.name === 'UnknownSerdeError' ? 'unknown_serde' : 'error';
    row.error_name = e?.name ?? null;
    row.error_message = e instanceof Error ? e.message : String(e);
  }
  return row;
};

const seen = new Set();
const decodeRows = [];
const addDecode = (input, src) => {
  if (seen.has(input)) return;
  seen.add(input);
  decodeRows.push(decodeRow(input, src));
};
for (const l of literals) addDecode(l.input, l.src);
for (const e of edge) addDecode(e, 'edge');

// shortcut: fuzz bases skip strings > 600 chars to keep decode.json small; raise if coverage of long inputs matters.
const fuzzBases = uniq([...literals.map((l) => l.input), ...edge]).filter((s) => s.length <= 600);
const fuzzChars = 'ABCXYZabcxyz0189+/-_=:% \n\t.~!{}"é中😀\0';
const mutate = (s) => {
  const ops = 1 + ri(3);
  for (let k = 0; k < ops; k++) {
    const pos = ri(s.length + 1);
    switch (ri(7)) {
      case 0: s = s.slice(0, pos); break;
      case 1: s = s.slice(0, pos) + pick([...fuzzChars]) + s.slice(pos + 1); break;
      case 2: s = s.slice(0, pos) + pick([...fuzzChars]) + s.slice(pos); break;
      case 3: s = s.slice(0, pos) + s.slice(pos + 1); break;
      case 4: s += '='.repeat(1 + ri(3)); break;
      case 5: s = rand() < 0.5 ? s.replace(/\+/g, '-').replace(/\//g, '_') : s.replace(/-/g, '+').replace(/_/g, '/'); break;
      case 6: {
        const body = s.includes(':') ? s.slice(s.indexOf(':') + 1) : s;
        const pre = pick(['', 'pako:', 'base64:', 'PAKO:', 'x:', 'constructor:', 'valueOf:']);
        s = pre + body;
        break;
      }
    }
  }
  return s;
};
let fuzzCount = 0;
for (let i = 0; fuzzCount < 4000 && i < 20000; i++) {
  const before = decodeRows.length;
  addDecode(mutate(pick(fuzzBases)), 'fuzz');
  fuzzCount += decodeRows.length - before;
}
write('decode.json', { note: 'getOptionsFromCode(input); input is the already-decodeURIComponent\'d path param' }, 'entries', decodeRows);

// ---------- query.json ----------
const dimTokens = ['', '0', '1', '-1', '-0', '12abc', ' 12', '1e3', '0x10', '+5', '1.9', 'NaN', 'Infinity', '٢',
  '9'.repeat(400), '100', '150', '1000', '5000', '10000', '10001', '1,2'];
const scaleTokens = ['', '1', '1.0', '1.005', '1.0049', '1.234', '1.235', '1.555', '1.995', '2.445', '2.995', '3',
  '3.001', '0.9', '-1', 'abc', ' 1.5 ', '001.50', '1e10', 'Infinity', '.5', '1.5abc'];
const states = [undefined, { maxWidth: '10000', maxHeight: '10000' }, { maxWidth: 10000, maxHeight: 10000 },
  { maxWidth: '150' }, { maxHeight: '199' }, { maxWidth: '1005' }, { maxWidth: '0' }, { maxWidth: 'abc' }];
const numTable = [];
const numIdx = new Map();
const ni = (n) => {
  if (n === null) return null;
  const key = bitsHex(n);
  if (!numIdx.has(key)) {
    numIdx.set(key, numTable.length);
    numTable.push(num(n));
  }
  return numIdx.get(key);
};
const msgs = [];
const mi = (m) => (msgs.includes(m) ? msgs.indexOf(m) : msgs.push(m) - 1);
const queryRows = [];
for (let w = 0; w < dimTokens.length; w++)
  for (let h = 0; h < dimTokens.length; h++)
    for (let s = 0; s < scaleTokens.length; s++)
      for (let st = 0; st < states.length; st++) {
        const q = {};
        if (dimTokens[w]) q.width = dimTokens[w];
        if (dimTokens[h]) q.height = dimTokens[h];
        if (scaleTokens[s]) q.scale = scaleTokens[s];
        let v = -1;
        try {
          U.validateQuery(q, states[st]);
        } catch (e) {
          v = mi(e.message);
        }
        const d = U.extractDimension(q, states[st]);
        queryRows.push([w, h, s, st, v, ni(d.width), ni(d.height)]);
      }
write('query.json', {
  format: 'row = [width_tok_idx, height_tok_idx, scale_tok_idx, state_idx, validate (-1 = ok, else messages idx), extractDimension width (numbers idx | null), height (numbers idx | null)]; a token of "" means the key is absent; state null means undefined',
  width_tokens: dimTokens, height_tokens: dimTokens, scale_tokens: scaleTokens,
  states: states.map((s) => s ?? null), messages: msgs, numbers: numTable,
}, 'rows', queryRows);

// ---------- single-param extractors ----------
// Entry: {in} is the query param value (string, or array for a repeated param); no "in" key = param absent.
const single = (name, key, fn, inputs) => {
  const rows = [{ ...attempt(() => fn({})) }, { query_undefined: true, ...attempt(() => fn(undefined)) }];
  for (const v of inputs) rows.push({ in: v, ...attempt(() => fn({ [key]: v })) });
  write(name, { fn: name.replace('.json', ''), key }, 'entries', rows);
};
const caseMix = (s) => [...s].map((c) => (rand() < 0.5 ? c.toUpperCase() : c)).join('');
const arrays = (vals) => [[], [vals[0]], [vals[0], vals[1]], [vals[1], vals[0]]];

const bgHand = [];
const bgBodies = ['fff', 'FFF', 'ffff', 'fffff', 'ffffff', 'fffffff', 'ffffffff', 'fffffffff', 'ff', 'f', '0', '000',
  'c0c0c0', 'C0C0C0', 'FF000055', 'abcdeg', 'gabcde', 'zzzzabc', 'abczzzz', 'red', 'RED', 'Red', 'transparent',
  'red1', 'red;}', 'red;background:url(x)', 'red</style><script>', 'rèd', 'ſ', 'K', '٣٣٣', 'ＦＦＦ', '1e3', '', ' ',
  '12 34', 'abc def', 'xyz\nabc', 'abc\n', '%23fff', '#fff', 'rgb(1,2,3)', 'abcdef0123'];
for (const pre of ['', '!', '#', '!!', '! ', ' ', ' !'])
  for (const body of bgBodies) bgHand.push(pre + body);
bgHand.push('\tfff\n', ' fff ', '﻿fff', 'fff ', '​fff', '!red ', ' !red ', '! red', '!');
const bgFuzzChars = '0123456789abcdefABCDEFghzGHZ!# \t\n;:()%é';
const bgFuzz = [];
for (let i = 0; i < 800; i++) {
  let s = '';
  const len = ri(12);
  for (let k = 0; k < len; k++) s += pick([...bgFuzzChars]);
  bgFuzz.push(s);
}
single('bg_color.json', 'bgColor', U.extractBgColor,
  uniq([...bgHand, ...bgFuzz]).concat(arrays(['fff', '!red'])));

const themes = ['default', 'neutral', 'dark', 'forest', 'base', 'null', 'mc', '', ' dark', 'dark ', 'dark\n',
  'darK', 'DARK', 'nEutral', 'forest1', 'fores', 'İ', 'ı', 'ﬀ', 'default\0', 'dark,forest'];
single('theme.json', 'theme', U.extractTheme, uniq([...themes,
  ...['default', 'neutral', 'dark', 'forest'].flatMap((t) => [t.toUpperCase(), caseMix(t), caseMix(t)])])
  .concat(arrays(['dark', 'forest'])));

const papers = ['letter', 'legal', 'tabloid', 'ledger', 'a0', 'a1', 'a2', 'a3', 'a4', 'a5', 'a6'];
single('paper.json', 'paper', U.extractPaper, uniq([...papers, ...papers.flatMap((p) => [p.toUpperCase(), caseMix(p)]),
  '', ' a4', 'a4 ', 'a7', 'a10', 'b4', 'AK', 'letter\n', 'Letter', 'executive', 'ledge'])
  .concat(arrays(['a4', 'letter'])));

const types = ['jpeg', 'png', 'webp'];
single('image_type.json', 'type', U.extractImageType, uniq([...types, ...types.flatMap((t) => [t.toUpperCase(), caseMix(t)]),
  '', 'jpg', 'JPG', 'gif', 'svg', 'pdf', 'avif', ' png', 'png ', 'image/png', 'pnG\u0000', 'webP'])
  .concat(arrays(['png', 'webp'])));

const flagRows = [];
for (const [fn, key] of [['isLandscape', 'landscape'], ['isFit', 'fit']]) {
  flagRows.push({ fn, query_undefined: true, ...attempt(() => U[fn](undefined)) });
  flagRows.push({ fn, ...attempt(() => U[fn]({})) });
  for (const v of ['', 'false', 'true', '0', '1', 'no', ' ', [], ['false'], ['', '']])
    flagRows.push({ fn, in: v, ...attempt(() => U[fn]({ [key]: v })) });
}
write('flags.json', { note: 'no "in" key = param absent' }, 'entries', flagRows);

// ---------- query_key.json ----------
const supportedQuery = { bgColor: 'c0c0c0', width: '800', height: '600', scale: '1.5', theme: 'forest', type: 'png',
  paper: 'a0', landscape: '', fit: '' };
const pools = {
  bgColor: ['fff', '!red', 'zzzzabc', 'xyz', ' c0c0c0 ', '', ['fff', '000']],
  width: ['800', '0', '-1', '12abc', '10001', '1e3', '', ['1', '2']],
  height: ['600', '150', '0', 'abc', '10000', '', ['1', '2']],
  scale: ['1.5', '0', '3.001', '2.995', '1.005', 'abc', '', ['2', '3']],
  theme: ['forest', 'DARK', 'base', '', ['dark']],
  type: ['png', 'Webp', 'jpeg', 'mp4', '', ['png']],
  paper: ['a0', 'LETTER', 'b4', '', ['a4']],
  landscape: ['', 'false', 'true', ['', '']],
  fit: ['', 'false', ['x']],
};
const qkQueries = [undefined, {}, supportedQuery, { ...supportedQuery, scale: '0' }, { ...supportedQuery, scale: '3.001' }];
while (qkQueries.length < 500) {
  const q = {};
  for (const [k, pool] of Object.entries(pools)) if (rand() < 0.45) q[k] = pick(pool);
  qkQueries.push(q);
}
const qkOptions = [undefined, { maxWidth: 10000, maxHeight: 10000 }, { maxWidth: '800' }, { maxHeight: '600' }];
const qkRows = [];
qkQueries.forEach((query, i) => {
  // every query under undefined and the app default; every 4th one also under the string limits used in the JS tests
  const opts = i % 4 === 0 ? qkOptions : qkOptions.slice(0, 2);
  for (const options of opts)
    for (const assetType of ['img', 'svg', 'pdf']) {
      const row = { assetType };
      if (query !== undefined) row.query = query;
      if (options !== undefined) row.options = options;
      qkRows.push({ ...row, ...attempt(() => getQueryKey(assetType, query, options)) });
    }
});
write('query_key.json', { note: 'missing query/options key = undefined' }, 'entries', qkRows);

// ---------- cache_key.json ----------
const testCode = 'pako:eNpVkM1qw0AMhF9F6NRC_AI-BBo7zSWQQHLz5iC8SnZJ9gd5TQm2373r_ECrk9B8MwwasA2ascSLUDRwrJWHPF9NZcR2yVF3gqJYjhtO4ILn-wirj02AzoQYrb98PvnVDEE1bGeMIRnrr9NTqh7-necR6mZLMYV4-qscf8II68buTY7_rxjh7PpuzlSeqWhJoCJ5IbhAx-LI6lx-mG8Kk2HHCsu8apKrQuWnzPVRU-K1tikI5qRbxwukPoXD3bdYJun5DdWW8iPci5p-AUT3W9o';
const ckTuples = [
  { encodedCode: testCode, expect: 'bf1124a1afd44220d64ca9fbe5a25ecdc6aa233362a1f85f7ed19b16902de95e' },
  { encodedCode: testCode, query: { type: 'png' }, expect: '4ed5db750c700436186b8bf2df5110a372f6c19c79da49e987b5dd71b5a65d4d' },
  {}, { encodedCode: '' }, { encodedCode: '0' }, { assetType: '', encodedCode: testCode }, { assetType: 'img' },
];
const codes = uniq([testCode, '0', 'a', 'é', '中文😀', 'a | b', ' ', ...literals.map((l) => l.input).slice(0, 10),
  ...decodeRows.filter((r) => r.src === 'fuzz').slice(0, 20).map((r) => r.input)]);
const assetTypes = [undefined, 'img', 'svg', 'pdf', 'IMG', 'foo'];
while (ckTuples.length < 300) {
  const t = {};
  const at = pick(assetTypes);
  if (at !== undefined) t.assetType = at;
  t.encodedCode = pick(codes);
  const q = pick(qkQueries);
  if (q !== undefined) t.query = q;
  const o = pick(qkOptions);
  if (o !== undefined) t.options = o;
  ckTuples.push(t);
}
const ckRows = ckTuples.map(({ expect, ...args }) => {
  const r = attempt(() => {
    const k = Object.keys(args).length ? createCacheKey(args) : createCacheKey();
    return typeof k === 'string' ? k : k.toString('hex');
  });
  if (expect !== undefined && r.out !== expect) throw new Error(`cache key self-check failed: ${r.out} != ${expect}`);
  return { args, ...r };
});
write('cache_key.json', { note: 'createCacheKey(args) hex; args {} means createCacheKey() with no argument' }, 'entries', ckRows);

// ---------- meta.json ----------
const req = createRequire(path.join(REPO, 'package.json'));
const pkgVersion = (name) => {
  const dir = path.dirname(req.resolve(name));
  for (let d = dir; d !== path.dirname(d); d = path.dirname(d)) {
    const p = path.join(d, 'package.json');
    if (fs.existsSync(p)) {
      const j = JSON.parse(fs.readFileSync(p, 'utf8'));
      if (j.name === name) return j.version;
    }
  }
  return null;
};
const meta = {
  node: process.version,
  js_base64: pkgVersion('js-base64'),
  pako: pkgVersion('pako'),
  generated: process.env.GEN_DATE || null,
  seed: '0x5eed',
  prng: 'mulberry32',
  generator: 'tools/golden/gen-vectors.mjs',
};
fs.writeFileSync(path.join(OUT, 'meta.json'), JSON.stringify(meta, null, 1) + '\n');
console.log('meta.json', meta);
