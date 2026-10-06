# next-socks5 — landing page

Marketing landing page for [next-socks5](https://github.com/ZingerLittleBee/next-socks5),
built with [Astro](https://astro.build) as a fully static site. Managed with
[Bun](https://bun.sh).

The whole page is drawn as a TUI: Geist Mono, a near-black palette with a cyan
accent, rounded panels with titles cut into the top border, a numbered tab bar
with scrollspy, and keyboard shortcuts (`1`-`5` jump, `↑↓` select a feature,
`i` install, `g` GitHub). The Dashboard section recreates the live `--mock`
TUI dashboard.

## Develop

```bash
cd landing
bun install
bun run dev      # http://localhost:4321
```

## Build

```bash
bun run build    # outputs static files to dist/
bun run preview  # serve the production build locally
bun run og       # regenerate public/og.png (social preview)
```

## Structure

```
src/
  consts.ts                 # repo URL, install command, version (read from Cargo.toml), sections
  layouts/Layout.astro      # <head>, fonts, OG/meta, global.css
  styles/global.css         # tokens, panel + section primitives
  components/
    Logo.astro              # plug-zap brand mark
    TopBar.astro            # tab bar, scrollspy, keyboard shortcuts
    Hero.astro              # 1 Overview: README panel + neofetch readout
    Dashboard.astro         # 2 Dashboard: --mock dashboard recreation
    Features.astro          # 3 Features: select list + detail panes
    Performance.astro       # 4 Perf: headline numbers
    Install.astro           # 5 Install: install.sh + common flags
    Footer.astro            # key-hint status bar
  pages/index.astro         # assembles all sections
public/
  favicon.svg               # plug-zap mark on the cyan brand chip
```

## Deployment

The site is fully static (`dist/`) and can be served by any static host
(GitHub Pages, Cloudflare Pages, Netlify, etc.). For project-path GitHub Pages,
set `base` in `astro.config.mjs`.
