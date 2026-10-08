// Display helpers: byte formatting, file categories and the color palette.

export interface Category {
  name: string;
  hue: number;
  sat: number;
  light: number;
}

// Index order must match `category()` in src-tauri/src/tree.rs.
export const CATEGORIES: Category[] = [
  { name: "Video", hue: 349, sat: 82, light: 62 },
  { name: "Images", hue: 42, sat: 92, light: 58 },
  { name: "Audio", hue: 276, sat: 70, light: 66 },
  { name: "Archives", hue: 18, sat: 88, light: 60 },
  { name: "Code & text", hue: 152, sat: 58, light: 52 },
  { name: "Documents", hue: 204, sat: 85, light: 60 },
  { name: "Apps & libraries", hue: 234, sat: 72, light: 68 },
  { name: "Data & caches", hue: 84, sat: 52, light: 52 },
  { name: "Other", hue: 220, sat: 14, light: 50 },
];

const EXT: Record<string, number> = {};
const groups: [number, string][] = [
  [0, "mp4 mov mkv avi m4v webm wmv flv mpg mpeg prores braw r3d mxf"],
  [1, "jpg jpeg png gif heic heif webp tiff tif raw cr2 cr3 nef arw dng psd svg bmp ico icns avif exr"],
  [2, "mp3 wav aac flac m4a aiff aif ogg opus caf alac logicx band"],
  [3, "zip gz tgz bz2 xz zst 7z rar tar dmg iso img pkg xip sparseimage sparsebundle ipsw vmdk qcow2 vdi"],
  [4, "js mjs cjs ts tsx jsx rs go py rb java kt swift c h cc cpp hpp m mm cs php html css scss json yaml yml toml xml md sh zig lua sql map wasm lock txt csv log"],
  [5, "pdf doc docx xls xlsx ppt pptx pages numbers key rtf epub odt sketch fig ai indd"],
  [6, "dylib so a o framework node exe dll bin app appex car nib metallib rlib dsym"],
  [7, "db sqlite sqlite3 realm pack idx cache dat data mlmodel safetensors gguf pt pth ckpt onnx parquet arrow npy h5 tfrecord vmem asset bundle"],
];
for (const [i, list] of groups) for (const e of list.split(" ")) EXT[e] = i;

export function categoryOf(name: string): number {
  const dot = name.lastIndexOf(".");
  if (dot <= 0) return 8;
  return EXT[name.slice(dot + 1).toLowerCase()] ?? 8;
}

/** Folder-like bundles that macOS shows as a single item. */
export function isPackage(name: string): boolean {
  return /\.(app|framework|bundle|photoslibrary|musiclibrary|xcarchive|plugin|kext|appex|logicx|fcpbundle|imovielibrary|tvlibrary|sparsebundle)$/i.test(name);
}

const SHADES = 7;
const palette: string[][] = CATEGORIES.map((c) =>
  Array.from({ length: SHADES }, (_, i) => {
    const l = c.light - 9 + i * 3;
    return `hsl(${c.hue} ${c.sat}% ${l}%)`;
  }),
);

export function color(cat: number, shade: number): string {
  return palette[cat][shade % SHADES];
}

export function swatch(cat: number): string {
  const c = CATEGORIES[cat];
  return `hsl(${c.hue} ${c.sat}% ${c.light}%)`;
}

/** Stable per-name variation so neighbouring files don't blend together. */
export function shadeOf(name: string): number {
  let h = 2166136261;
  for (let i = 0; i < name.length; i++) h = Math.imul(h ^ name.charCodeAt(i), 16777619);
  return (h >>> 0) % SHADES;
}

const UNITS = ["B", "KB", "MB", "GB", "TB", "PB"];

export function bytes(n: number): string {
  if (n < 1000) return `${n} B`;
  let i = 0;
  let v = n;
  while (v >= 1000 && i < UNITS.length - 1) {
    v /= 1000;
    i++;
  }
  return `${v >= 100 ? v.toFixed(0) : v >= 10 ? v.toFixed(1) : v.toFixed(2)} ${UNITS[i]}`;
}

export function count(n: number): string {
  return n.toLocaleString("en-US");
}

export function pct(part: number, whole: number): string {
  if (whole <= 0) return "0%";
  const p = (part / whole) * 100;
  if (p < 0.1) return "<0.1%";
  return `${p >= 10 ? p.toFixed(0) : p.toFixed(1)}%`;
}

export function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
}
