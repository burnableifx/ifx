import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import { labs } from './labs.mjs';

const site = process.env.SITE_URL;
const socialImage = site && new URL('/og.png', site).href;

export default defineConfig({
  site,
  integrations: [
    starlight({
      title: 'ifx',
      description: 'Infrastructure as one reconciled graph.',
      logo: {
        src: './src/assets/ifx-mark.png',
        alt: 'ifx',
      },
      favicon: '/assets/ifx-mark.png',
      social: [
        {
          icon: 'github',
          label: 'GitHub',
          href: 'https://github.com/burnableifx/ifx',
        },
      ],
      customCss: ['./src/styles/custom.css'],
      components: {
        ThemeProvider: './src/components/DarkThemeProvider.astro',
        ThemeSelect: './src/components/EmptyThemeSelect.astro',
      },
      head: socialImage ? [
        { tag: 'meta', attrs: { property: 'og:image', content: socialImage } },
        { tag: 'meta', attrs: { property: 'og:image:width', content: '1731' } },
        { tag: 'meta', attrs: { property: 'og:image:height', content: '909' } },
        { tag: 'meta', attrs: { name: 'twitter:card', content: 'summary_large_image' } },
        { tag: 'meta', attrs: { name: 'twitter:image', content: socialImage } },
      ] : [],
      sidebar: [
        { label: 'Home', link: '/' },
        {
          label: 'Learn',
          items: [
            { label: 'Choose a learning path', link: '/learn/' },
            { label: 'Getting started', link: '/learn/getting-started/' },
            {
              label: 'Labs',
              collapsed: false,
              items: [
                { label: 'Lab sequence', link: '/learn/labs/' },
                ...labs.map(({ label, slug }) => ({
                  label,
                  link: `/learn/labs/${slug}/`,
                })),
              ],
            },
          ],
        },
        {
          label: 'Build',
          items: [
            { label: 'Rust stacks', link: '/learn/getting-started/' },
            { label: 'CLI and configuration', link: '/guides/cli/' },
            { label: 'QEMU provider', link: '/guides/qemu/' },
            { label: 'Linode provider', link: '/guides/linode/' },
          ],
        },
        {
          label: 'Operate',
          items: [
            { label: 'ifxd', link: '/guides/ifxd/' },
            { label: 'Troubleshooting', link: '/guides/troubleshooting/' },
            { label: 'Performance', link: '/guides/performance/' },
          ],
        },
        {
          label: 'Understand',
          items: [
            { label: 'Design', link: '/concepts/design/' },
            { label: 'Provider authoring', link: '/reference/provider-authoring/' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'Resource schemas', link: '/reference/resources/' },
            { label: 'Examples', link: '/examples/' },
            { label: 'Brand', link: '/project/brand/' },
          ],
        },
      ],
    }),
  ],
});
