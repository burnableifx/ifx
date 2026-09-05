# ifx documentation website

Requires Node.js 22.12 or newer and npm.

The site presents the repository handbook, labs, examples, and schema-generated
resource reference through Astro and Starlight. Source Markdown remains authoritative;
`scripts/sync-content.mjs` adds site metadata, rewrites internal links, and copies visual
assets into generated directories before development and production builds.

```console
$ npm ci
$ npm run dev
```

Run the complete local check with:

```console
$ npm run check
```

Set `SITE_URL` to the production origin when building for deployment. This enables the
sitemap, canonical URLs, and absolute social-preview metadata:

```console
$ SITE_URL=https://docs.example.com npm run build
```

The site is dark-only and targets the root of a dedicated documentation origin.
Subpath hosting is intentionally unsupported.

Generated content under `src/content/docs/generated/`, copied logos, lab recordings,
and Explorer screenshots are ignored by Git. Change their repository sources instead.
