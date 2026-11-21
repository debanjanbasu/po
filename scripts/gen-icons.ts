// Generate all app icons from one pixel-art design (single source of truth).
//
// The mark: the nerd from the 404 page, promoted to mascot. His two round
// glasses spell "oo" — two lenses, two devices, one identity. Palette and
// character lifted from assets/orig-content/vector-404-not-found.svg.
//
// dx 0.8.0-alpha.1 has no icon pipeline for ios/android/macos builds
// (packages/cli/src/build/{apple,android}.rs never consume [bundle].icon),
// so outputs are checked in and injected per platform:
//   - web:     assets/favicon.ico + assets/logo.svg (document::Link)
//   - iOS:     assets/icons/ios/*.png copied into the .app by
//              scripts/ios-device-deploy.sh; CFBundleIcons in native/apple/Info.ios.plist
//   - macOS:   assets/icons/icon.icns copied by scripts/fix-bundle-case.sh;
//              CFBundleIconFile in native/apple/Info.macos.plist
//   - Android: assets/icons/android/ vector drawables copied over the
//              generated Gradle res/ (see README)
//
// Requires: node, ImageMagick (magick), iconutil (macOS, for .icns).
// Usage: node scripts/gen-icons.ts
//
/* oxlint-disable max-lines -- one directly-run generator: the palette, pixel maps, emitters and layout are one pipeline; splitting the file would scatter it. */

import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { join } from "node:path";
import { crc32, deflateSync } from "node:zlib";

type PixelMap = Map<string, string>;
type PixelBounds = { px: PixelMap; w: number; h: number };
type BackgroundOptions = { bg?: string };

const ROOT = join(import.meta.dirname, "..");
const ICONS = join(ROOT, "assets", "icons");

// ---------------------------------------------------------------- palette
// Lifted from assets/orig-content/vector-404-not-found.svg (class -> hex).
// .c line work (also pupils, nose, mouth ring)
const OUTLINE = "#4c2a12";
// open-mouth cavity (darker than the line work)
const MOUTH = "#2a1408";
// upper teeth in the open mouth (warm white)
const TEETH = "#fff4e0";
// icon plate (.e — the 404 page backdrop)
const BG = "#fedfab";
// .f
const HAIR = "#5d3417";
// .d face
const SKIN = "#e79653";
// shaded skin (neck, chin, right cheek)
const SKIN_SH = "#c6631d";
// .h lens interior
const LENS = "#f6c182";
// .a
const SHIRT = "#bb4911";
// .b lit chest panel
const SHIRT_HI = "#c6631d";

// ------------------------------------------------------------- the mark
// The 404 page's nerd, promoted to mascot — his round glasses spell "oo".
// One char per pixel; edit the map, rerun, done.
const INK: Record<string, string> = {
  X: OUTLINE,
  C: MOUTH,
  T: TEETH,
  F: HAIR,
  S: SKIN,
  G: SKIN_SH,
  E: LENS,
  A: SHIRT,
  B: SHIRT_HI,
};
const FACE = [
  "......XXXXXXXXXX......",
  ".....XFFFFFFFFFFX.....",
  "....XFFFFFFFFFFFFX....",
  "...XFFFFFFFFFFFFFFX...",
  "...XFSSSSSSSSSSSSFX...",
  "...XFSSSSSSSSSSSSFX...",
  "...XSSXXSSSSSSXXSSX...",
  "...XSSSSSSSSSSSSSSX...",
  "...XSXXXXSSSSXXXXSX...",
  "...XXEEEEXXXXEEEEXX...",
  "...XXEXXEXSSXEXXEXX...",
  "...XXEXXEXSSXEXXEXX...",
  "...XXEEEEXSSXEEEEXX...",
  "...XSXXXXSSSSXXXXSX...",
  "...XSSSSSSXXSSSSSSX...",
  "...XSSSSSXTTXSSSSSX...",
  "....XSSSSXCCXSSSSX....",
  ".....XSSSSXXSSSSX.....",
  "......XXXXXXXXXX......",
  "..XXAAAAAXGGXAAAAAXX..",
  ".XAAAAAAAAXXAAAAAAAAX.",
  ".XAAAAAAABBBBAAAAAAAX.",
  "XAAAAAAAABBBBAAAAAAAAX",
  "XAAAAAAAAABBAAAAAAAAAX",
  "XAAAAAAAAAAAAAAAAAAAAX",
];

// The mark must stay perfectly left/right symmetric.
for (const [y, row] of FACE.entries()) {
  if (row.length !== FACE[0].length)
    throw new Error(`FACE row ${y} has width ${row.length}, expected ${FACE[0].length}`);
  if (row !== Array.from(row).reverse().join(""))
    throw new Error(`FACE row ${y} is not horizontally symmetric`);
}

function markPixels(): PixelMap {
  const px: PixelMap = new Map();
  FACE.forEach((row, y) => {
    for (let x = 0; x < row.length; x++) {
      const c = INK[row[x]];
      if (c) px.set(`${x},${y}`, c);
    }
  });
  return px;
}

function normalize(px: PixelMap): PixelBounds {
  let maxX = -Infinity, maxY = -Infinity, minX = Infinity, minY = Infinity;
  for (const k of px.keys()) {
    const [x, y] = k.split(",").map(Number);
    minX = Math.min(minX, x); maxX = Math.max(maxX, x);
    minY = Math.min(minY, y); maxY = Math.max(maxY, y);
  }
  const out: PixelMap = new Map();
  for (const [k, c] of px) {
    const [x, y] = k.split(",").map(Number);
    out.set(`${x - minX},${y - minY}`, c);
  }
  return { px: out, w: maxX - minX + 1, h: maxY - minY + 1 };
}

// ------------------------------------------------------------- SVG emit
// Merge horizontal runs of same-colored cells into single rects.
function svg(px: PixelMap, w: number, h: number, { bg }: BackgroundOptions = {}) {
  const grid = new Map(px);
  const rects = [];
  if (bg !== undefined && bg !== "") rects.push(`<rect width="${w}" height="${h}" fill="${bg}"/>`);
  for (let y = 0; y < h; y++) {
    let x = 0;
    while (x < w) {
      const c = grid.get(`${x},${y}`);
      if (c === undefined) { x++; continue; }
      let run = 1;
      while (grid.get(`${x + run},${y}`) === c) run++;
      rects.push(`<rect x="${x}" y="${y}" width="${run}" height="1" fill="${c}"/>`);
      x += run;
    }
  }
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${w} ${h}" shape-rendering="crispEdges">\n${rects.join("\n")}\n</svg>\n`;
}

// Android VectorDrawable: same runs as android:pathData, grouped per color.
function vectorDrawable(px: PixelMap, w: number, h: number, { bg }: BackgroundOptions = {}) {
  const byColor = new Map<string, string[]>();
  const grid = new Map(px);
  for (let y = 0; y < h; y++) {
    let x = 0;
    while (x < w) {
      const c = grid.get(`${x},${y}`);
      if (c === undefined) { x++; continue; }
      let run = 1;
      while (grid.get(`${x + run},${y}`) === c) run++;
      const d = byColor.get(c) ?? [];
      d.push(`M${x},${y}h${run}v1h-${run}z`);
      byColor.set(c, d);
      x += run;
    }
  }
  const paths: string[] = [];
  if (bg !== undefined && bg !== "") paths.push(`  <path android:fillColor="${bg}" android:pathData="M0,0h${w}v${h}h-${w}z"/>`);
  for (const [c, d] of byColor)
    paths.push(`  <path android:fillColor="${c}" android:pathData="${d.join("")}"/>`);
  return `<?xml version="1.0" encoding="utf-8"?>
<vector xmlns:android="http://schemas.android.com/apk/res/android"
    android:width="108dp"
    android:height="108dp"
    android:viewportWidth="${w}"
    android:viewportHeight="${h}">
${paths.join("\n")}
</vector>
`;
}

// --------------------------------------------------------------- layouts
function shift(px: PixelMap, dx: number, dy: number): PixelMap {
  const out = new Map();
  for (const [k, c] of px) {
    const [x, y] = k.split(",").map(Number);
    out.set(`${x + dx},${y + dy}`, c);
  }
  return out;
}

// 22x25; bust cropped, no bottom border
const mark = normalize(markPixels());

// Icon layout: mark centered on a square canvas.
function iconPixels(canvas: number): PixelMap {
  const dx = Math.floor((canvas - mark.w) / 2);
  const dy = Math.floor((canvas - mark.h) / 2);
  return shift(mark.px, dx, dy);
}

// ----------------------------------------------------------------- write
mkdirSync(join(ICONS, "ios"), { recursive: true });
mkdirSync(join(ICONS, "android"), { recursive: true });

// Master mark (transparent, tight) — in-app / web / README use.
writeFileSync(join(ROOT, "assets", "logo.svg"), svg(mark.px, mark.w, mark.h));

// App icon (full-bleed square) — source for all platform rasters.
const ICON_GRID = 30;
const iconPx = iconPixels(ICON_GRID);

// Android adaptive layers: foreground must sit inside the centre 66% safe
// zone (66/108 of the viewport) -> 44 units keeps the 26-tall mark at ~59%.
const FG_GRID = 44;
writeFileSync(
  join(ICONS, "android", "ic_launcher_foreground.xml"),
  vectorDrawable(iconPixels(FG_GRID), FG_GRID, FG_GRID),
);
writeFileSync(
  join(ICONS, "android", "ic_launcher_background.xml"),
  vectorDrawable(new Map(), FG_GRID, FG_GRID, { bg: BG }),
);

// ------------------------------------------------------- PNG base (exact)
// magick's SVG rasterizer rounds rect edges inconsistently (off-by-one
// columns that broke the mark's left/right symmetry), so every raster output
// derives from a 1px-per-cell PNG written directly from the pixel map.
// magick then only point-resizes rasters, which is exact at integer scales.
function pngChunk(type: string, data: Buffer): Buffer {
  const out = Buffer.alloc(8 + data.length + 4);
  out.writeUInt32BE(data.length, 0);
  out.write(type, 4, "ascii");
  data.copy(out, 8);
  out.writeUInt32BE(crc32(out.subarray(4, 8 + data.length)), 8 + data.length);
  return out;
}

function png(px: PixelMap, w: number, h: number, { bg }: BackgroundOptions = {}): Buffer {
  const rgb = (c: string) => [1, 3, 5].map((i) => parseInt(c.slice(i, i + 2), 16));
  const bgPx = bg === undefined || bg === "" ? [0, 0, 0, 0] : [...rgb(bg), 255];
  // filter byte 0 + RGBA per row
  const raw = Buffer.alloc(h * (1 + w * 4));
  for (let y = 0; y < h; y++) {
    const row = y * (1 + w * 4);
    for (let x = 0; x < w; x++) {
      const c = px.get(`${x},${y}`);
      raw.set(c === undefined ? bgPx : [...rgb(c), 255], row + 1 + x * 4);
    }
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  // bit depth
  ihdr[8] = 8;
  // color type: RGBA
  ihdr[9] = 6;
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    pngChunk("IHDR", ihdr),
    pngChunk("IDAT", deflateSync(raw)),
    pngChunk("IEND", Buffer.alloc(0)),
  ]);
}

const iconBasePath = join(ROOT, "assets", ".icon-base.png");
writeFileSync(iconBasePath, png(iconPx, ICON_GRID, ICON_GRID, { bg: BG }));

const magick = (args: string[]): void => {
  execFileSync("magick", args, { stdio: "inherit" });
};
// Rasterize pixel art sharp: point-sample at the largest integer multiple of
// the grid, then PAD with the plate colour (never stretch) up to the exact
// target, so every art pixel stays a uniform hard-edged square.
const render = (baseFile: string, grid: number, size: number, out: string, { extra = [] }: { extra?: string[] } = {}): void => {
  const k = Math.max(1, Math.floor(size / grid));
  const args = ["-background", "none", baseFile, "-filter", "point", "-resize", `${grid * k}x${grid * k}`];
  if (grid * k !== size)
    args.push("-background", BG, "-gravity", "center", "-extent", `${size}x${size}`);
  magick([...args, ...extra, out]);
};
// iOS launcher icons must be opaque; keep them plain truecolor PNGs.
const OPAQUE = ["-alpha", "off", "-type", "TrueColor"];

// Web favicon (legacy fallback; the SVG favicon does the real work):
// 48/32 pad the 1:1 plate render, 16 downscales smoothly from a big render.
{
  const tmp = join(ROOT, "assets", ".favicon-tmp");
  mkdirSync(tmp, { recursive: true });
  render(iconBasePath, ICON_GRID, 48, join(tmp, "f48.png"));
  render(iconBasePath, ICON_GRID, 32, join(tmp, "f32.png"));
  const big = join(tmp, "big.png");
  render(iconBasePath, ICON_GRID, 480, big);
  magick([big, "-resize", "16x16", join(tmp, "f16.png")]);
  magick([join(tmp, "f16.png"), join(tmp, "f32.png"), join(tmp, "f48.png"), join(ROOT, "assets", "favicon.ico")]);
  rmSync(tmp, { recursive: true });
}

// iOS launcher PNGs (opaque; SpringBoard applies the corner mask).
// 120 and 180 are exact 4x/6x multiples of the 30-cell plate.
for (const [name, size] of [
  ["AppIcon60x60@2x.png", 120],
  ["AppIcon60x60@3x.png", 180],
  ["AppIcon76x76@2x.png", 152],
  ["AppIcon83.5x83.5@2x.png", 167],
  ["AppIcon1024x1024.png", 1024],
] as const) render(iconBasePath, ICON_GRID, size, join(ICONS, "ios", name), { extra: OPAQUE });

// Web app / PWA touch icon.
render(iconBasePath, ICON_GRID, 180, join(ICONS, "apple-touch-icon.png"), { extra: OPAQUE });

// macOS .icns: Apple's Big Sur template — content on 824/1024 of the canvas
// with pre-rounded corners (macOS does not mask icons itself).
{
  const tmp = join(ROOT, "assets", ".icns-tmp");
  const iconset = join(tmp, "icon.iconset");
  mkdirSync(iconset, { recursive: true });
  const full = join(tmp, "full.png");
  render(iconBasePath, ICON_GRID, 824, full);
  const rounded = join(tmp, "rounded.png");
  magick([full, "(", "+clone", "-alpha", "extract",
    "-draw", "fill black polygon 0,0 0,186 186,0 fill white circle 186,186 186,0",
    "(", "+clone", "-flip", ")", "-compose", "Multiply", "-composite",
    "(", "+clone", "-flop", ")", "-compose", "Multiply", "-composite",
    ")", "-alpha", "off", "-compose", "CopyOpacity", "-composite", rounded]);
  const padded = join(tmp, "padded.png");
  magick([rounded, "-background", "none", "-gravity", "center", "-extent", "1024x1024", padded]);
  for (const s of [16, 32, 64, 128, 256, 512]) {
    magick([padded, "-resize", `${s}x${s}`, join(iconset, `icon_${s}x${s}.png`)]);
    magick([padded, "-resize", `${s * 2}x${s * 2}`, join(iconset, `icon_${s}x${s}@2x.png`)]);
  }
  execFileSync("iconutil", ["-c", "icns", iconset, "-o", join(ICONS, "icon.icns")], { stdio: "inherit" });
  rmSync(tmp, { recursive: true });
}

// Landing site (separate Astro app) uses the same brand assets.
const LANDING = join(ROOT, "landing", "public");
writeFileSync(join(LANDING, "logo.svg"), svg(mark.px, mark.w, mark.h));
copyFileSync(join(ROOT, "assets", "favicon.ico"), join(LANDING, "favicon.ico"));
copyFileSync(join(ICONS, "apple-touch-icon.png"), join(LANDING, "apple-touch-icon.png"));

if (process.env.DEBUG_ASCII !== undefined && process.env.DEBUG_ASCII !== "") {
  const names = new Map(Object.entries(INK).map(([name, color]) => [color, name]));
  for (let y = 0; y < mark.h; y++) {
    let row = "";
    for (let x = 0; x < mark.w; x++) row += names.get(mark.px.get(`${x},${y}`) ?? "") ?? ".";
    console.log(row);
  }
}

rmSync(iconBasePath);
console.log("icons generated: assets/logo.svg, assets/favicon.ico, assets/icons/");
