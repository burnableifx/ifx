import { access, readdir, readFile } from 'node:fs/promises';
import { dirname, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const websiteDir = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const outputDir = resolve(websiteDir, 'dist');

async function filesUnder(directory) {
  const entries = await readdir(directory, { withFileTypes: true });
  const files = [];

  for (const entry of entries) {
    const path = resolve(directory, entry.name);
    if (entry.isDirectory()) files.push(...await filesUnder(path));
    if (entry.isFile()) files.push(path);
  }

  return files;
}

const htmlFiles = (await filesUnder(outputDir)).filter((path) => path.endsWith('.html'));
const missing = new Set();
const appearanceIssues = new Set();

for (const htmlFile of htmlFiles) {
  const html = await readFile(htmlFile, 'utf8');
  const route = `/${relative(outputDir, htmlFile).replaceAll('\\', '/')}`;
  const pageUrl = new URL(route, 'https://ifx.invalid');

  if (!/<html[^>]+data-theme="dark"/.test(html)) {
    appearanceIssues.add(`${route} does not default to dark`);
  }
  if (html.includes('starlight-theme-select')) {
    appearanceIssues.add(`${route} contains a theme selector`);
  }

  for (const match of html.matchAll(/(?:href|src)="([^"]+)"/g)) {
    const target = match[1];
    if (/^(?:[a-z]+:|#|\/\/)/i.test(target)) continue;

    const url = new URL(target, pageUrl);
    let localPath = decodeURIComponent(url.pathname);
    if (localPath.endsWith('/')) localPath += 'index.html';

    try {
      await access(resolve(outputDir, `.${localPath}`));
    } catch {
      missing.add(`${route} -> ${target}`);
    }
  }
}

if (missing.size > 0) {
  throw new Error(`broken local links:\n${[...missing].sort().join('\n')}`);
}

if (appearanceIssues.size > 0) {
  throw new Error(`dark-only appearance violations:\n${[...appearanceIssues].sort().join('\n')}`);
}

console.log(`checked ${htmlFiles.length} generated pages`);
