// @ts-check
import { defineConfig } from 'astro/config';

// Static marketing site for next-socks5.
// `site` powers absolute URLs (OG tags, sitemap); set it to the production host.
// If deploying to a project GitHub Pages path, also set `base: '/next-socks5'`.
export default defineConfig({
  site: 'https://zingerlittlebee.github.io',
  build: {
    inlineStylesheets: 'auto',
  },
});
