import { copyFile, cp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { dirname, posix, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { labs } from '../labs.mjs';

const websiteDir = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const repositoryDir = resolve(websiteDir, '..');
const generatedDir = resolve(websiteDir, 'src/content/docs/generated');

export const pages = [
  ['docs/getting-started.md', 'learn/getting-started.md'],
  ['labs/README.md', 'learn/labs/index.md'],
  ...labs.map(({ slug, source }) => [source, `learn/labs/${slug}.md`]),
  ['docs/cli.md', 'guides/cli.md'],
  ['docs/qemu.md', 'guides/qemu.md'],
  ['docs/linode.md', 'guides/linode.md'],
  ['docs/ifxd.md', 'guides/ifxd.md'],
  ['docs/troubleshooting.md', 'guides/troubleshooting.md'],
  ['docs/performance.md', 'guides/performance.md'],
  ['docs/design.md', 'concepts/design.md'],
  ['docs/resources.md', 'reference/resources.md'],
  ['docs/provider-authoring.md', 'reference/provider-authoring.md'],
  ['docs/BRAND.md', 'project/brand.md'],
  ['examples/README.md', 'examples/index.md'],
  ['examples/local-files/README.md', 'examples/local-files.md'],
  ['examples/qemu-web/README.md', 'examples/qemu-web.md'],
  ['examples/linode-web/README.md', 'examples/linode-web.md'],
];

const routes = new Map();

for (const [source, destination] of pages) {
  const route = toRoute(destination);
  routes.set(source, route);
  if (posix.basename(source) === 'README.md') {
    routes.set(posix.dirname(source), route);
  }
}

export function toRoute(destination) {
  const page = destination
    .replace(/\.(md|mdx)$/, '')
    .replace(/(^|\/)index$/, '');
  return page ? `/${page}/` : '/';
}

export function extractTitle(markdown) {
  const heading = markdown.match(/^#\s+(.+)$/m)?.[1];
  if (!heading) throw new Error('source document has no level-one heading');
  return heading.replaceAll('`', '').trim();
}

function repositoryPath(path) {
  return relative(repositoryDir, path).split(sep).join('/');
}

function publicAssetRoute(path) {
  const mappings = [
    ['docs/assets/', '/assets/'],
    ['docs/images/', '/assets/explorer/'],
    ['labs/media/', '/assets/labs/media/'],
    ['labs/casts/', '/assets/labs/casts/'],
  ];

  for (const [sourcePrefix, publicPrefix] of mappings) {
    if (path.startsWith(sourcePrefix)) {
      return `${publicPrefix}${path.slice(sourcePrefix.length)}`;
    }
  }

  return undefined;
}

function rewriteTarget(target, source, routeMap, image) {
  if (/^(?:[a-z]+:|#|\/)/i.test(target)) return target;

  const hashAt = target.indexOf('#');
  const targetPath = hashAt === -1 ? target : target.slice(0, hashAt);
  const fragment = hashAt === -1 ? '' : target.slice(hashAt);
  const resolved = resolve(repositoryDir, dirname(source), targetPath);
  const path = repositoryPath(resolved).replace(/\/$/, '');

  if (path.startsWith('../')) return target;

  const route = routeMap.get(path) ?? routeMap.get(`${path}/README.md`);
  if (route) return `${route}${fragment}`;

  if (image && /^labs\/media\/lab-\d+\.gif$/.test(path)) {
    return `/posters/${posix.basename(path, '.gif')}.webp`;
  }

  const asset = publicAssetRoute(path);
  if (asset) return `${asset}${fragment}`;

  const object = targetPath.endsWith('/') ? 'tree' : 'blob';
  return `https://github.com/burnableifx/ifx/${object}/main/${path}${fragment}`;
}

export function rewriteLinks(markdown, source, routeMap = routes) {
  const withPosterImages = markdown.replace(
    /(!\[[^\]]*\])\(([^)\s]+)(?:\s+"[^"]*")?\)/g,
    (match, label, target) => `${label}(${rewriteTarget(target, source, routeMap, true)})`,
  );

  return withPosterImages.replace(
    /\]\(([^)\s]+)(?:\s+"[^"]*")?\)/g,
    (match, target) => `](${rewriteTarget(target, source, routeMap, false)})`,
  );
}

export function renderPage(markdown, source) {
  const title = extractTitle(markdown);
  const body = markdown.replace(/^#\s+.+\r?\n+/, '');
  const frontmatter = [
    '---',
    `title: ${JSON.stringify(title)}`,
    `editUrl: ${JSON.stringify(`https://github.com/burnableifx/ifx/edit/main/${source}`)}`,
    '---',
    '',
  ].join('\n');

  return `${frontmatter}${rewriteLinks(body, source)}`;
}

async function copyAssets() {
  const fileAssets = [
    ['docs/assets/ifx-mark.png', 'src/assets/ifx-mark.png'],
    ['docs/assets/ifx-mark.png', 'public/assets/ifx-mark.png'],
  ];

  const directoryAssets = [
    ['docs/images', 'public/assets/explorer'],
    ['labs/media', 'public/assets/labs/media'],
    ['labs/casts', 'public/assets/labs/casts'],
  ];

  for (const [source, destination] of fileAssets) {
    const destinationPath = resolve(websiteDir, destination);
    await mkdir(dirname(destinationPath), { recursive: true });
    await copyFile(resolve(repositoryDir, source), destinationPath);
  }

  for (const [source, destination] of directoryAssets) {
    const destinationPath = resolve(websiteDir, destination);
    await rm(destinationPath, { recursive: true, force: true });
    await cp(resolve(repositoryDir, source), destinationPath, {
      recursive: true,
      force: true,
    });
  }
}

async function sync() {
  if (!generatedDir.startsWith(`${websiteDir}${sep}`)) {
    throw new Error('refusing to replace generated content outside the website');
  }

  await rm(generatedDir, { recursive: true, force: true });
  await mkdir(generatedDir, { recursive: true });

  for (const [source, destination] of pages) {
    const markdown = await readFile(resolve(repositoryDir, source), 'utf8');
    const destinationPath = resolve(generatedDir, destination);
    await mkdir(dirname(destinationPath), { recursive: true });
    await writeFile(destinationPath, renderPage(markdown, source));
  }

  await copyAssets();
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  await sync();
}
