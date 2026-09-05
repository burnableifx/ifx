import assert from 'node:assert/strict';
import test from 'node:test';

import { extractTitle, renderPage, rewriteLinks, toRoute } from './sync-content.mjs';

test('destination paths become stable documentation routes', () => {
  assert.equal(toRoute('learn/labs/index.md'), '/learn/labs/');
  assert.equal(toRoute('guides/qemu.md'), '/guides/qemu/');
});

test('titles come from the source document heading', () => {
  assert.equal(extractTitle('# `ifxd`\n\nBody'), 'ifxd');
});

test('repository links become site routes or source links', () => {
  const routes = new Map([
    ['labs/02-next/README.md', '/learn/labs/02-next/'],
  ]);
  const markdown = '[Next](../02-next/README.md) and [`src/main.rs`](src/main.rs).';
  const rendered = rewriteLinks(markdown, 'labs/01-first/README.md', routes);

  assert.equal(
    rendered,
    '[Next](/learn/labs/02-next/) and [`src/main.rs`](https://github.com/burnableifx/ifx/blob/main/labs/01-first/src/main.rs).',
  );
});

test('embedded lab recordings use still posters but GIF links remain available', () => {
  const markdown = '![Preview](../media/lab-8.gif) [GIF](../media/lab-8.gif)';
  const rendered = rewriteLinks(markdown, 'labs/08-qemu/README.md');

  assert.equal(
    rendered,
    '![Preview](/posters/lab-8.webp) [GIF](/assets/labs/media/lab-8.gif)',
  );
});

test('rendered pages add metadata and do not repeat the heading', () => {
  const rendered = renderPage('# Example\n\nBody.\n', 'docs/example.md');

  assert.match(rendered, /title: "Example"/);
  assert.match(rendered, /edit\/main\/docs\/example\.md/);
  assert.doesNotMatch(rendered, /^# Example/m);
  assert.match(rendered, /Body\./);
});
