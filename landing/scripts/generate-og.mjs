// Generate the social-preview (Open Graph) image for the landing page.
//
//   node scripts/generate-og.mjs   (or `npm run og`)
//
// Renders a 1200x630 PNG to public/og.png using @resvg/resvg-js. IBM Plex Mono
// TTFs are downloaded once into scripts/.cache (gitignored) so the brand font
// renders correctly without depending on system-installed fonts.

import { Resvg } from '@resvg/resvg-js';
import { readFileSync, writeFileSync, mkdirSync, existsSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const CACHE = resolve(__dirname, '.cache');
const PUBLIC = resolve(__dirname, '../public');

const FONTS = {
  regular: 'IBMPlexMono-Regular.ttf',
  semibold: 'IBMPlexMono-SemiBold.ttf',
  bold: 'IBMPlexMono-Bold.ttf',
};
const FONT_BASE = 'https://cdn.jsdelivr.net/gh/google/fonts@main/ofl/ibmplexmono';

async function ensureFont(file) {
  const dest = resolve(CACHE, file);
  if (existsSync(dest)) return dest;
  mkdirSync(CACHE, { recursive: true });
  const url = `${FONT_BASE}/${file}`;
  process.stdout.write(`↓ fetching ${file} … `);
  const res = await fetch(url);
  if (!res.ok) throw new Error(`failed to download ${url}: ${res.status}`);
  const buf = Buffer.from(await res.arrayBuffer());
  writeFileSync(dest, buf);
  console.log('done');
  return dest;
}

function projectVersion() {
  try {
    const toml = readFileSync(resolve(__dirname, '../../Cargo.toml'), 'utf-8');
    return toml.match(/^version\s*=\s*"([^"]+)"/m)?.[1] ?? '0.4.0';
  } catch {
    return '0.4.0';
  }
}

// Lucide "plug-zap" glyph, scaled/translated into the green brand chip.
const plugZap = (x, y, scale) => `
  <g transform="translate(${x},${y}) scale(${scale})" fill="none" stroke="#07100C"
     stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round">
    <path d="M6.3 20.3a2.4 2.4 0 0 0 3.4 0L12 18l-6-6-2.3 2.3a2.4 2.4 0 0 0 0 3.4Z"/>
    <path d="m2 22 3-3"/>
    <path d="M7.5 13.5 10 11"/>
    <path d="M10.5 16.5 13 14"/>
    <path d="m18 3-4 4h6l-4 4"/>
  </g>`;

function buildSvg(version) {
  const mono = 'IBM Plex Mono';
  return `<svg width="1200" height="630" viewBox="0 0 1200 630" xmlns="http://www.w3.org/2000/svg">
  <defs>
    <radialGradient id="glow" cx="50%" cy="-5%" r="75%">
      <stop offset="0%" stop-color="#46E08A" stop-opacity="0.13"/>
      <stop offset="45%" stop-color="#38D6D6" stop-opacity="0.05"/>
      <stop offset="72%" stop-color="#070B0A" stop-opacity="0"/>
    </radialGradient>
  </defs>

  <rect width="1200" height="630" fill="#070B0A"/>
  <rect width="1200" height="630" fill="url(#glow)"/>
  <rect x="0.5" y="0.5" width="1199" height="629" fill="none" stroke="#161F1C"/>

  <!-- decorative trend sparklines (echo of the dashboard) -->
  <polyline fill="none" stroke="#46E08A" stroke-width="2" opacity="0.55"
    points="812,96 852,84 892,90 932,72 972,80 1012,64 1052,74 1092,58 1128,66"/>
  <polyline fill="none" stroke="#38D6D6" stroke-width="2" opacity="0.45"
    points="812,118 852,110 892,114 932,102 972,108 1012,96 1052,104 1092,90 1128,98"/>

  <!-- brand lockup -->
  <rect x="72" y="64" width="54" height="54" fill="#46E08A"/>
  ${plugZap(85, 77, 1.2)}
  <text x="142" y="101" font-family="${mono}" font-size="30" font-weight="700"
    letter-spacing="0.5" fill="#E8F0EC">next-socks5</text>
  <rect x="350" y="80" width="92" height="28" fill="none" stroke="#2E4034"/>
  <text x="364" y="100" font-family="${mono}" font-size="17" fill="#6FB78F">v${version}</text>

  <!-- eyebrow -->
  <text x="72" y="210" font-family="${mono}" font-size="20" letter-spacing="1"
    fill="#6FB78F">SOCKS5 SERVER · RFC 1928 + RFC 1929 · WRITTEN IN RUST</text>

  <!-- headline -->
  <text x="70" y="296" font-family="${mono}" font-size="72" font-weight="700"
    letter-spacing="-2" fill="#EEF5F0">A SOCKS5 proxy that</text>
  <text x="70" y="378" font-family="${mono}" font-size="72" font-weight="700"
    letter-spacing="-2" fill="#46E08A">shows its work.</text>

  <!-- subtitle -->
  <text x="72" y="436" font-family="${mono}" font-size="22" fill="#9BA8A2">Live dashboard · CONNECT + UDP · secure-by-default egress · ~3.5 MB image</text>

  <!-- install command bar -->
  <rect x="72" y="470" width="1056" height="80" fill="#0A0F0D" stroke="#283330"/>
  <text x="100" y="518" font-family="${mono}" font-size="23">
    <tspan fill="#52605B">$ </tspan><tspan fill="#38D6D6">curl</tspan><tspan fill="#CBD6D0"> -fsSL </tspan><tspan fill="#E8B84B">…/install.sh</tspan><tspan fill="#6A7672"> | </tspan><tspan fill="#38D6D6">sh</tspan>
  </text>
  <path d="M1072 510 l9 9 l19 -21" fill="none" stroke="#46E08A" stroke-width="3.6"
    stroke-linecap="round" stroke-linejoin="round"/>

  <!-- footer stats -->
  <text x="72" y="596" font-family="${mono}" font-size="20" font-weight="700" fill="#46E08A">~2 GB/s<tspan dx="14" fill="#5C6863" font-weight="400" font-size="17">throughput</tspan></text>
  <text x="372" y="596" font-family="${mono}" font-size="20" font-weight="700" fill="#38D6D6">~1.6 ms<tspan dx="14" fill="#5C6863" font-weight="400" font-size="17">latency</tspan></text>
  <text x="630" y="596" font-family="${mono}" font-size="20" font-weight="700" fill="#E8F0EC">~6k/s<tspan dx="14" fill="#5C6863" font-weight="400" font-size="17">new conns</tspan></text>
  <text x="912" y="596" font-family="${mono}" font-size="20" font-weight="700" fill="#E8F0EC">0 C<tspan dx="14" fill="#5C6863" font-weight="400" font-size="17">deps · pure Rust</tspan></text>
</svg>`;
}

async function main() {
  const fontFiles = await Promise.all(Object.values(FONTS).map(ensureFont));
  const version = projectVersion();
  const svg = buildSvg(version);

  const resvg = new Resvg(svg, {
    font: { fontFiles, loadSystemFonts: false, defaultFontFamily: 'IBM Plex Mono' },
    fitTo: { mode: 'width', value: 1200 },
  });
  const png = resvg.render().asPng();

  mkdirSync(PUBLIC, { recursive: true });
  const out = resolve(PUBLIC, 'og.png');
  writeFileSync(out, png);
  console.log(`✓ wrote ${out} (1200×630, v${version})`);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
