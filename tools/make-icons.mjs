// Builds the application icon and the favicon from their source artwork.
// Run with: node tools/make-icons.mjs
//
// Written by hand rather than through an image library, for the same reason make-corpus.mjs is:
// the repository carries no npm dependencies and this has to run on a clean clone.
//
// Reads  assets/icon-source.png  and  assets/favicon-source.png
// Writes assets/icon.png  assets/icon.ico  assets/favicon.png  assets/favicon.ico
//
// `crates/app/build.rs` compiles assets/icon.ico into viewer.exe through windres, and
// `crates/app/src/ui.rs` embeds assets/icon.png as the window icon.

import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { inflateSync, deflateSync } from "node:zlib";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const assets = join(root, "assets");
mkdirSync(assets, { recursive: true });

// ------------------------------------------------------------------ PNG read

const crcTable = (() => {
  const t = new Int32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c;
  }
  return t;
})();

const crc32 = (buf) => {
  let c = -1;
  for (const b of buf) c = crcTable[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ -1) >>> 0;
};

const paeth = (a, b, c) => {
  const p = a + b - c;
  const pa = Math.abs(p - a);
  const pb = Math.abs(p - b);
  const pc = Math.abs(p - c);
  return pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
};

/** Decode a non-interlaced 8-bit PNG into { width, height, rgba }. */
function decodePng(buf) {
  if (buf.readUInt32BE(0) !== 0x89504e47) throw new Error("not a PNG");
  let off = 8;
  let width = 0;
  let height = 0;
  let colourType = 0;
  const idat = [];
  let palette = null;
  let trns = null;

  while (off < buf.length) {
    const len = buf.readUInt32BE(off);
    const type = buf.toString("ascii", off + 4, off + 8);
    const data = buf.subarray(off + 8, off + 8 + len);
    if (type === "IHDR") {
      width = data.readUInt32BE(0);
      height = data.readUInt32BE(4);
      const depth = data[8];
      colourType = data[9];
      if (depth !== 8) throw new Error(`bit depth ${depth} is not supported`);
      if (data[12] !== 0) throw new Error("interlaced PNG is not supported");
    } else if (type === "PLTE") palette = Buffer.from(data);
    else if (type === "tRNS") trns = Buffer.from(data);
    else if (type === "IDAT") idat.push(Buffer.from(data));
    else if (type === "IEND") break;
    off += 12 + len;
  }

  const channels = { 0: 1, 2: 3, 3: 1, 4: 2, 6: 4 }[colourType];
  if (!channels) throw new Error(`colour type ${colourType} is not supported`);
  if (colourType === 3 && !palette) throw new Error("indexed-colour PNG is missing a PLTE chunk");

  const raw = inflateSync(Buffer.concat(idat));
  const stride = width * channels;
  const out = Buffer.alloc(stride * height);
  let prev = Buffer.alloc(stride);

  for (let y = 0; y < height; y++) {
    const filter = raw[y * (stride + 1)];
    const line = raw.subarray(y * (stride + 1) + 1, (y + 1) * (stride + 1));
    const cur = Buffer.from(line);
    for (let i = 0; i < stride; i++) {
      const a = i >= channels ? cur[i - channels] : 0;
      const b = prev[i];
      const c = i >= channels ? prev[i - channels] : 0;
      switch (filter) {
        case 0: break;
        case 1: cur[i] = (cur[i] + a) & 0xff; break;
        case 2: cur[i] = (cur[i] + b) & 0xff; break;
        case 3: cur[i] = (cur[i] + ((a + b) >> 1)) & 0xff; break;
        case 4: cur[i] = (cur[i] + paeth(a, b, c)) & 0xff; break;
        default: throw new Error(`filter ${filter} is not a PNG filter`);
      }
    }
    cur.copy(out, y * stride);
    prev = cur;
  }

  // Widen whatever arrived to RGBA8.
  const rgba = Buffer.alloc(width * height * 4);
  for (let i = 0; i < width * height; i++) {
    let r, g, b, a = 255;
    if (colourType === 0) { r = g = b = out[i]; }
    else if (colourType === 4) { r = g = b = out[i * 2]; a = out[i * 2 + 1]; }
    else if (colourType === 2) { r = out[i * 3]; g = out[i * 3 + 1]; b = out[i * 3 + 2]; }
    else if (colourType === 6) { r = out[i * 4]; g = out[i * 4 + 1]; b = out[i * 4 + 2]; a = out[i * 4 + 3]; }
    else {
      const idx = out[i];
      r = palette[idx * 3]; g = palette[idx * 3 + 1]; b = palette[idx * 3 + 2];
      if (trns && idx < trns.length) a = trns[idx];
    }
    rgba[i * 4] = r; rgba[i * 4 + 1] = g; rgba[i * 4 + 2] = b; rgba[i * 4 + 3] = a;
  }
  return { width, height, rgba };
}

// ----------------------------------------------------------------- PNG write

const chunk = (type, data) => {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
};

function encodePng({ width, height, rgba }) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8;
  ihdr[9] = 6;
  const stride = width * 4;
  const raw = Buffer.alloc((stride + 1) * height);
  for (let y = 0; y < height; y++) {
    raw[y * (stride + 1)] = 0;
    rgba.copy(raw, y * (stride + 1) + 1, y * stride, (y + 1) * stride);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

// ------------------------------------------------------------------- resize

/**
 * Box-filter downsample, averaging in premultiplied alpha.
 *
 * Averaging straight RGBA darkens the edge of a shape against transparent pixels, because a fully
 * transparent pixel still contributes its colour. The rounded corners of this artwork are exactly
 * that case.
 */
function resize(src, size) {
  const out = Buffer.alloc(size * size * 4);
  const scale = src.width / size;
  for (let y = 0; y < size; y++) {
    const y0 = Math.floor(y * scale);
    const y1 = Math.max(y0 + 1, Math.floor((y + 1) * scale));
    for (let x = 0; x < size; x++) {
      const x0 = Math.floor(x * scale);
      const x1 = Math.max(x0 + 1, Math.floor((x + 1) * scale));
      let r = 0, g = 0, b = 0, a = 0, n = 0;
      for (let sy = y0; sy < y1; sy++) {
        for (let sx = x0; sx < x1; sx++) {
          const i = (sy * src.width + sx) * 4;
          const alpha = src.rgba[i + 3] / 255;
          r += src.rgba[i] * alpha;
          g += src.rgba[i + 1] * alpha;
          b += src.rgba[i + 2] * alpha;
          a += src.rgba[i + 3];
          n++;
        }
      }
      const alpha = a / n;
      const un = alpha > 0 ? 255 / a : 0;
      const o = (y * size + x) * 4;
      out[o] = Math.min(255, Math.round(r * un));
      out[o + 1] = Math.min(255, Math.round(g * un));
      out[o + 2] = Math.min(255, Math.round(b * un));
      out[o + 3] = Math.round(alpha);
    }
  }
  return { width: size, height: size, rgba: out };
}

// ---------------------------------------------------------------- ICO writer

/**
 * A 32-bit BMP entry, which is what Windows wants below 256 pixels.
 *
 * The header declares twice the real height because an icon DIB carries a colour bitmap and an AND
 * mask stacked in one image. The mask is redundant for 32-bit entries and every shell still
 * expects the bytes to be there.
 */
function bmpEntry({ width, height, rgba }) {
  const header = Buffer.alloc(40);
  header.writeUInt32LE(40, 0);
  header.writeInt32LE(width, 4);
  header.writeInt32LE(height * 2, 8);
  header.writeUInt16LE(1, 12);
  header.writeUInt16LE(32, 14);

  const pixels = Buffer.alloc(width * height * 4);
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      const s = ((height - 1 - y) * width + x) * 4;
      const d = (y * width + x) * 4;
      pixels[d] = rgba[s + 2];
      pixels[d + 1] = rgba[s + 1];
      pixels[d + 2] = rgba[s];
      pixels[d + 3] = rgba[s + 3];
    }
  }

  const maskStride = Math.ceil(width / 32) * 4;
  const mask = Buffer.alloc(maskStride * height); // all zero: nothing masked out
  return Buffer.concat([header, pixels, mask]);
}

function encodeIco(images) {
  const entries = images.map((img) =>
    img.width >= 256 ? encodePng(img) : bmpEntry(img),
  );
  const dir = Buffer.alloc(6 + 16 * images.length);
  dir.writeUInt16LE(0, 0);
  dir.writeUInt16LE(1, 2);
  dir.writeUInt16LE(images.length, 4);

  let offset = dir.length;
  images.forEach((img, i) => {
    const at = 6 + i * 16;
    dir[at] = img.width >= 256 ? 0 : img.width;
    dir[at + 1] = img.height >= 256 ? 0 : img.height;
    dir[at + 2] = 0;
    dir[at + 3] = 0;
    dir.writeUInt16LE(1, at + 4);
    dir.writeUInt16LE(32, at + 6);
    dir.writeUInt32LE(entries[i].length, at + 8);
    dir.writeUInt32LE(offset, at + 12);
    offset += entries[i].length;
  });
  return Buffer.concat([dir, ...entries]);
}

// --------------------------------------------------------------------- build

const ICO_SIZES = [16, 24, 32, 48, 64, 128, 256];

function build(sourceName, stem, pngSize) {
  const source = decodePng(readFileSync(join(assets, sourceName)));
  if (source.width !== source.height) {
    throw new Error(`${sourceName} is ${source.width}x${source.height}, expected a square`);
  }

  const png = source.width === pngSize ? source : resize(source, pngSize);
  writeFileSync(join(assets, `${stem}.png`), encodePng(png));

  const ico = encodeIco(ICO_SIZES.map((s) => (s === source.width ? source : resize(source, s))));
  writeFileSync(join(assets, `${stem}.ico`), ico);

  console.log(
    `${stem}.png ${pngSize}x${pngSize}, ${stem}.ico ${ICO_SIZES.join("/")} (${ico.length} bytes)`,
  );
}

build("icon-source.png", "icon", 256);
build("favicon-source.png", "favicon", 512);
