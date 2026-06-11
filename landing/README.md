# next-socks5 — landing page

Marketing landing page for [next-socks5](https://github.com/ZingerLittleBee/next-socks5),
built with [Astro](https://astro.build) as a fully static site.

Terminal / ratatui-inspired aesthetic: IBM Plex Mono, a near-black green-tinted
palette, and sharp-cornered panels with titles cut into the top border. The hero
showcases a faithful recreation of the live `--mock` TUI dashboard.

## Develop

```bash
cd landing
npm install
npm run dev      # http://localhost:4321
```

## Build

```bash
npm run build    # outputs static files to dist/
npm run preview  # serve the production build locally
```

## Structure

```
src/
  layouts/Layout.astro      # <head>, fonts, OG/meta, global.css
  styles/global.css         # base styles, keyframes, hover utilities, responsive rules
  components/
    Logo.astro              # plug-zap brand mark (nav + footer)
    Nav.astro
    Hero.astro              # headline + one-command terminal session + stat strip
    Dashboard.astro         # ratatui --mock dashboard recreation
    Features.astro          # 0x01–0x06 feature grid
    Install.astro           # four install methods + usage line
    Config.astro            # config.toml showcase
    Performance.astro       # headline numbers
    Footer.astro            # CTA + copy-to-clipboard install command
  pages/index.astro         # assembles all sections
public/
  favicon.svg               # plug-zap mark on the green brand chip
```

## Deployment

The site is fully static (`dist/`) and can be served by any static host
(GitHub Pages, Cloudflare Pages, Netlify, etc.). For project-path GitHub Pages,
set `base` in `astro.config.mjs`.
