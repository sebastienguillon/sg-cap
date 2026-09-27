// Regenerates resources/sgcap.ico from the SVG logo.
//
// Usage (from any directory):
//   npm install sharp        # once, in a scratch folder or the repo
//   node scripts/make-icon.js
//
// Small sizes (16-32 px) are rendered from a variant with heavier strokes so
// the mark stays legible in the notification area. Sizes up to 64 px are stored
// as classic 32-bit bitmaps, 128 and 256 px as PNG, as Windows expects.
const fs = require("fs");
const path = require("path");
const sharp = require("sharp");

const ROOT = path.join(__dirname, "..");
const SVG_PATH = path.join(ROOT, "resources", "sebastienguillon - logo alt.svg");
const ICO_PATH = path.join(ROOT, "resources", "sgcap.ico");
const VIEWBOX = 448;

const svgOriginal = fs.readFileSync(SVG_PATH, "utf8");
const svgBold = svgOriginal
  .replace(/stroke-width="40"/g, 'stroke-width="60"')
  .replace('fill-opacity="0.25"', 'fill-opacity="0.35"');

async function render(svg, size) {
  const density = (72 * size) / VIEWBOX;
  return sharp(Buffer.from(svg), { density })
    .resize(size, size, { fit: "contain", background: { r: 0, g: 0, b: 0, alpha: 0 } })
    .ensureAlpha();
}

// 32bpp BGRA XOR bitmap (bottom-up) followed by a 1bpp AND mask.
function bmpEntry(size, rgba) {
  const rowXor = size * 4;
  const rowAnd = Math.ceil(size / 32) * 4;
  const xor = Buffer.alloc(rowXor * size);
  const and = Buffer.alloc(rowAnd * size);
  for (let y = 0; y < size; y++) {
    const srcRow = size - 1 - y;
    for (let x = 0; x < size; x++) {
      const s = (srcRow * size + x) * 4;
      const d = y * rowXor + x * 4;
      xor[d] = rgba[s + 2];
      xor[d + 1] = rgba[s + 1];
      xor[d + 2] = rgba[s];
      xor[d + 3] = rgba[s + 3];
      if (rgba[s + 3] === 0) {
        and[y * rowAnd + (x >> 3)] |= 0x80 >> (x & 7);
      }
    }
  }
  const header = Buffer.alloc(40);
  header.writeUInt32LE(40, 0);
  header.writeInt32LE(size, 4);
  header.writeInt32LE(size * 2, 8);
  header.writeUInt16LE(1, 12);
  header.writeUInt16LE(32, 14);
  header.writeUInt32LE(0, 16);
  header.writeUInt32LE(xor.length + and.length, 20);
  return Buffer.concat([header, xor, and]);
}

async function main() {
  const bmpSizes = [16, 20, 24, 32, 40, 48, 64];
  const pngSizes = [128, 256];
  const entries = [];
  for (const size of [...bmpSizes, ...pngSizes]) {
    const img = await render(size <= 32 ? svgBold : svgOriginal, size);
    if (bmpSizes.includes(size)) {
      const { data } = await img.raw().toBuffer({ resolveWithObject: true });
      entries.push({ size, data: bmpEntry(size, data) });
    } else {
      entries.push({ size, data: await img.png({ compressionLevel: 9 }).toBuffer() });
    }
  }
  const dir = Buffer.alloc(6);
  dir.writeUInt16LE(1, 2);
  dir.writeUInt16LE(entries.length, 4);
  const table = Buffer.alloc(16 * entries.length);
  let offset = 6 + table.length;
  entries.forEach((e, i) => {
    const o = i * 16;
    table.writeUInt8(e.size >= 256 ? 0 : e.size, o);
    table.writeUInt8(e.size >= 256 ? 0 : e.size, o + 1);
    table.writeUInt16LE(1, o + 4);
    table.writeUInt16LE(32, o + 6);
    table.writeUInt32LE(e.data.length, o + 8);
    table.writeUInt32LE(offset, o + 12);
    offset += e.data.length;
  });
  fs.writeFileSync(ICO_PATH, Buffer.concat([dir, table, ...entries.map((e) => e.data)]));
  console.log(`wrote ${ICO_PATH} (${entries.length} images)`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
