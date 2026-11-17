// Captures goldens from the running reference containers (see run.sh) into tests/fixtures/golden/.
// Host node >= 22, no deps. Every case is requested twice; differing bodies => nondeterministic.
// Usage: node tools/golden/capture.mjs [--dry]   (wipes and rewrites cases/, bodies/, manifest.json;
//        --dry only prints the expanded case count per env)
import { readFileSync, writeFileSync, mkdirSync, rmSync, readdirSync, statSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const matrix = JSON.parse(readFileSync(join(root, 'tools/golden/matrix.json'), 'utf8'));
const enc = JSON.parse(readFileSync(join(root, 'tests/fixtures/corpus/encoded.json'), 'utf8'));
const kseries = JSON.parse(readFileSync(join(root, 'tools/golden/corpus/k-series.json'), 'utf8'));
const out = join(root, 'tests/fixtures/golden');
const sha = (b) => createHash('sha256').update(b).digest('hex');

// ---- expand the matrix into requests ----------------------------------------------------------
function encodedOf(subject) {
  if (/^K\d+$/.test(subject)) return enc.K[subject];
  const [id, form = 'pako'] = subject.split('.');
  const v = enc.items[id]?.[form];
  if (v == null) throw new Error(`unknown subject ${subject}`);
  return v;
}
const resolvePath = (p) =>
  p.replace(/\{([^}]+)\}/g, (_, ref) => {
    if (!ref.startsWith('D.')) return encodedOf(ref);
    const d = enc.D[ref.slice(2)];
    return typeof d === 'string' ? d : d.value;
  });

const reqs = [];
const add = (r) => reqs.push({ env: 'main', method: 'GET', query: '', headers: {}, ...r });
const subj = (corpus, endpoint, query, extra = {}) =>
  add({ corpus, endpoint, query, path: `/${endpoint}/${encodedOf(corpus)}`, ...extra });

const tf = Object.keys(enc.items);
for (const id of [...Object.keys(enc.K), ...tf]) for (const ep of matrix.corpus_endpoints) subj(id, ep, '');
for (const id of tf) for (const form of ['base64', 'plain']) subj(`${id}.${form}`, 'svg', '');
for (const l of kseries.literals) {
  // the CORS tests in kitchensink send their own Origin + ACRM
  const headers = l.method === 'OPTIONS' ? { Origin: 'http://cross.origin.com', 'Access-Control-Request-Method': 'GET' } : {};
  subj(l.id, l.endpoint, l.query, { method: l.method, headers, no_origin: l.method === 'OPTIONS', source: l.source });
}
for (const g of matrix.params) for (const s of g.subjects) for (const q of g.queries) subj(s, g.endpoint, q);
for (const e of matrix.extra) subj(e.subject, e.endpoint, e.query);
for (const d of matrix.dseries) add({ corpus: d.id, name: d.name, method: d.method ?? 'GET', path: resolvePath(d.path), headers: d.headers ?? {}, no_origin: d.no_origin });
for (const e of matrix.env) {
  if (e.path) add({ env: e.instance, corpus: 'ENV', name: e.name, path: e.path });
  else subj(e.subject, e.endpoint, e.query, { env: e.instance });
}

// ---- ids: <corpus>__<endpoint>__<slug(query)>, made unique case-insensitively (Windows FS) -----
const slug = (q) => (q ? q.replace(/=/g, '-').replace(/&/g, '.').replace(/[^A-Za-z0-9.\-]/g, '_') : 'noparams');
const seen = new Map();
const cases = [];
for (const r of reqs) {
  if (!r.no_origin) r.headers = { Origin: matrix.origin, ...r.headers };
  const key = JSON.stringify([r.env, r.method, r.path, r.query, r.headers]);
  if (seen.has(key)) continue;
  seen.set(key, r);
  const ep = r.endpoint ? r.endpoint + (r.method === 'GET' ? '' : '-' + r.method.toLowerCase()) : r.name;
  r.id = r.endpoint ? `${r.corpus}__${ep}__${slug(r.query)}` : `${r.corpus}__${ep}`;
  if (r.env !== 'main') r.id += `__env-${r.env}`;
  r.key = key;
  cases.push(r);
}
const groups = Object.groupBy(cases, (c) => c.id.toLowerCase());
for (const g of Object.values(groups)) if (g.length > 1) for (const c of g) c.id += '~' + sha(c.key).slice(0, 6);

// ---- capture ---------------------------------------------------------------------------------
const EXT = { 'image/svg+xml': 'svg', 'image/png': 'png', 'image/jpeg': 'jpg', 'image/webp': 'webp', 'application/pdf': 'pdf', 'text/html': 'html' };
const DROP = new Set(['date', 'connection', 'keep-alive']);
// Chrome stamps /CreationDate and /ModDate into every PDF; mask them for a second opinion.
const maskPdf = (b) => Buffer.from(b.toString('latin1').replace(/\/(CreationDate|ModDate) ?\(D:[^)]*\)/g, '/$1()'), 'latin1');

async function send(c) {
  const url = matrix.instances[c.env] + c.path + (c.query ? '?' + c.query : '');
  const r = await fetch(url, { method: c.method, headers: c.headers, redirect: 'manual', signal: AbortSignal.timeout(180_000) });
  return { status: r.status, headers: Object.fromEntries([...r.headers].filter(([k]) => !DROP.has(k))), body: Buffer.from(await r.arrayBuffer()) };
}

async function waitReady(env) {
  for (let i = 0; i < 180; i++) {
    try {
      if ((await fetch(matrix.instances[env] + '/')).status === 200) return;
    } catch {}
    await new Promise((r) => setTimeout(r, 1000));
  }
  throw new Error(`instance ${env} not ready`);
}

async function capture(c) {
  const a = await send(c);
  const b = await send(c);
  const type = (a.headers['content-type'] ?? '').split(';')[0];
  const ext = EXT[type] ?? 'txt';
  const body_file = a.body.length ? `bodies/${c.id}.${ext}` : null;
  if (body_file) writeFileSync(join(out, body_file), a.body);
  const rec = {
    id: c.id,
    status: a.status,
    headers: a.headers,
    body_len: a.body.length,
    body_sha256: sha(a.body),
    body_file,
    error_text: a.status >= 400 ? a.body.toString('utf8') : null,
    nondeterministic: a.status !== b.status || !a.body.equals(b.body),
  };
  if (rec.nondeterministic) rec.second = { status: b.status, body_len: b.body.length, body_sha256: sha(b.body) };
  // Wall-clock in the bytes (PDF dates, gantt "today" line) changes across runs even when a == b.
  if (ext === 'pdf' || (ext === 'svg' && a.body.includes('class="today"'))) rec.nondeterministic = true;
  if (ext === 'pdf') rec.nondeterministic_ignoring_pdf_dates = a.status !== b.status || !maskPdf(a.body).equals(maskPdf(b.body));
  writeFileSync(join(out, 'cases', `${c.id}.json`), JSON.stringify(rec, null, 2) + '\n');
  return rec;
}

if (process.argv.includes('--dry')) {
  console.log(cases.length, Object.fromEntries(Object.entries(Object.groupBy(cases, (c) => c.env)).map(([k, v]) => [k, v.length])));
  process.exit(0);
}
rmSync(join(out, 'cases'), { recursive: true, force: true });
rmSync(join(out, 'bodies'), { recursive: true, force: true });
mkdirSync(join(out, 'cases'), { recursive: true });
mkdirSync(join(out, 'bodies'), { recursive: true });
writeFileSync(
  join(out, 'manifest.json'),
  JSON.stringify(
    cases.map((c) => ({ id: c.id, env: c.env, method: c.method, path: c.path, query: c.query, headers: c.headers, corpus: c.corpus, ...(c.source && { source: c.source }) })),
    null,
    2
  ) + '\n'
);

const results = [];
for (const env of Object.keys(matrix.instances)) {
  const todo = cases.filter((c) => c.env === env);
  if (!todo.length) continue;
  await waitReady(env);
  let next = 0;
  // 2 workers = QUEUE_CONCURRENCY of the main instance; env instances are tiny, same pool is fine
  await Promise.all(
    [0, 1].map(async () => {
      while (next < todo.length) {
        const c = todo[next++];
        const rec = await capture(c).catch((e) => ({ id: c.id, status: 'ERR', error: String(e) }));
        results.push({ ...rec, env, endpoint: c.endpoint ?? c.name });
        process.stdout.write(`${results.length}/${cases.length} ${rec.status} ${c.id}\n`);
      }
    })
  );
}

// ---- summary ---------------------------------------------------------------------------------
const counts = {};
for (const r of results) {
  const k = `${r.env}:${r.endpoint}:${r.status}`;
  counts[k] = (counts[k] ?? 0) + 1;
}
const dirBytes = (d) => readdirSync(d, { withFileTypes: true }).reduce((s, e) => s + (e.isDirectory() ? dirBytes(join(d, e.name)) : statSync(join(d, e.name)).size), 0);
const summary = {
  total: results.length,
  counts,
  failed: results.filter((r) => r.status === 'ERR').map((r) => `${r.id}: ${r.error}`),
  nondeterministic: results.filter((r) => r.nondeterministic).map((r) => r.id).sort(),
  nondeterministic_ignoring_pdf_dates: results.filter((r) => r.nondeterministic_ignoring_pdf_dates).map((r) => r.id).sort(),
  bytes: dirBytes(out),
};
console.log(JSON.stringify(summary, null, 2));
if (summary.failed.length) process.exitCode = 1;
