// Runs INSIDE the reference image (WORKDIR = the repo); prints tests/fixtures/golden/meta.json to stdout.
// run.sh passes IMAGE / IMAGE_ID from the host.
import { readFileSync, readdirSync } from 'node:fs';
import { execSync } from 'node:child_process';
import { homedir } from 'node:os';

const version = (dir) => JSON.parse(readFileSync(`${dir}/package.json`, 'utf8')).version;
const pnpm = (pkg) => {
  const d = readdirSync('node_modules/.pnpm').find((n) => n.startsWith(pkg.replace('/', '+') + '@'));
  return d ? version(`node_modules/.pnpm/${d}/node_modules/${pkg}`) : null;
};
const sh = (cmd) => execSync(cmd, { encoding: 'utf8' }).trim();
const chromeDir = `${homedir()}/.cache/puppeteer/chrome`;
const chromeBuild = readdirSync(chromeDir)[0];

console.log(
  JSON.stringify(
    {
      image: process.env.IMAGE ?? null,
      image_id: process.env.IMAGE_ID ?? null,
      node: process.version,
      chrome: sh(`${chromeDir}/${chromeBuild}/chrome-linux64/chrome --version 2>/dev/null`),
      versions: {
        mermaid: version('node_modules/mermaid'),
        elkjs: pnpm('elkjs'),
        '@zenuml/core': pnpm('@zenuml/core'),
        '@mermaid-js/mermaid-zenuml': version('node_modules/@mermaid-js/mermaid-zenuml'),
        '@mermaid-js/layout-tidy-tree': version('node_modules/@mermaid-js/layout-tidy-tree'),
        '@fortawesome/fontawesome-free': version('node_modules/@fortawesome/fontawesome-free'),
        puppeteer: version('node_modules/puppeteer'),
      },
      fc_match: Object.fromEntries(['trebuchet ms', 'sans-serif', 'serif', 'monospace'].map((f) => [f, sh(`fc-match '${f}'`)])),
    },
    null,
    2
  )
);
