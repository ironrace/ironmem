# Cloudflare Pages

`ironrace.dev` should point at the static site in `site/`.

## Project Settings

- Cloudflare product: Pages
- Project name: `ironrace-dev`
- Production branch: `main`
- Build command: leave blank
- Build output directory: `site`
- Wrangler config: `wrangler.jsonc`

The site is static HTML/CSS/JS and does not need Pages Functions, Workers bindings,
environment variables, or secrets.

## Local Preview

```bash
npm install
npm run site:dev
```

For a dependency-free preview, the site also works with any static file server:

```bash
python3 -m http.server 8787 --directory site
```

## Deploy

The lowest-maintenance path is Cloudflare Pages Git integration:

1. In Cloudflare, create a Pages project from `github.com/ironrace/ironmem`.
2. Use the project settings above.
3. Deploy `main`.

For a direct upload from this checkout:

```bash
npm install
npx wrangler login
npm run site:deploy
```

Cloudflare direct upload deploys the `site/` directory as the Pages asset root.

## Custom Domain

After the first deployment:

1. Open the `ironrace-dev` Pages project.
2. Add `ironrace.dev` under Custom domains.
3. Add `www.ironrace.dev` if desired.
4. Prefer a single canonical host. If both apex and `www` are enabled, redirect
   `www.ironrace.dev` to `ironrace.dev`.

If the custom domain is attached through the API or the DNS record is not created
automatically, add this DNS record in the `ironrace.dev` Cloudflare zone:

| Type | Name | Content | Proxy |
|---|---|---|---|
| `CNAME` | `ironrace.dev` | `ironrace-dev.pages.dev` | Proxied |

Keep `site/sitemap.xml`, `site/robots.txt`, and canonical metadata aligned with
the chosen production host.
