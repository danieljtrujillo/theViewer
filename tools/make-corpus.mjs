// Generates a small test corpus under corpus/ for manual checks.
// Run with: node tools/make-corpus.mjs

import { mkdirSync, writeFileSync } from "node:fs";
import { deflateSync } from "node:zlib";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const zlibSync = (buf) => deflateSync(buf, { level: 6 });

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const out = join(root, "corpus");
mkdirSync(out, { recursive: true });

// ---------------------------------------------------------------- PNG writer
// Written by hand so the corpus needs no image library, and shared by every fixture below.

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

const chunk = (type, data) => {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
};

// An uncompressed XMP packet, which is where GPano metadata lives in a PNG.
// Layout: keyword NUL, compression flag, compression method, language NUL,
// translated keyword NUL, then the text.
const xmpChunk = (xml) =>
  chunk(
    "iTXt",
    Buffer.concat([
      Buffer.from("XML:com.adobe.xmp\0", "latin1"),
      Buffer.from([0, 0]),
      Buffer.from("\0\0", "latin1"),
      Buffer.from(xml, "utf8"),
    ]),
  );

/// `pixel(x, y)` returns [r, g, b].
function writePng(path, width, height, pixel, extra = []) {
  const raw = Buffer.alloc((width * 3 + 1) * height);
  let o = 0;
  for (let y = 0; y < height; y++) {
    raw[o++] = 0; // filter type: none
    for (let x = 0; x < width; x++) {
      const [r, g, b] = pixel(x, y);
      raw[o++] = r & 255;
      raw[o++] = g & 255;
      raw[o++] = b & 255;
    }
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; ihdr[9] = 2; ihdr[10] = 0; ihdr[11] = 0; ihdr[12] = 0;
  writeFileSync(
    path,
    Buffer.concat([
      Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
      chunk("IHDR", ihdr),
      ...extra,
      chunk("IDAT", zlibSync(raw)),
      chunk("IEND", Buffer.alloc(0)),
    ]),
  );
}

// Deterministic generator, so regenerating the corpus does not change what is under test.
let seed = 0x2545f491;
function rand() {
  seed ^= seed << 13;
  seed >>>= 0;
  seed ^= seed >>> 17;
  seed ^= seed << 5;
  seed >>>= 0;
  return seed / 0xffffffff;
}

// ---------------------------------------------------------------- cube.ply
{
  const v = [
    [-1, -1, -1], [1, -1, -1], [1, 1, -1], [-1, 1, -1],
    [-1, -1, 1], [1, -1, 1], [1, 1, 1], [-1, 1, 1],
  ];
  const f = [
    [0, 3, 2], [0, 2, 1], [4, 5, 6], [4, 6, 7],
    [0, 1, 5], [0, 5, 4], [2, 3, 7], [2, 7, 6],
    [1, 2, 6], [1, 6, 5], [0, 4, 7], [0, 7, 3],
  ];
  let s = `ply\nformat ascii 1.0\nelement vertex ${v.length}\n`;
  s += "property float x\nproperty float y\nproperty float z\n";
  s += "property uchar red\nproperty uchar green\nproperty uchar blue\n";
  s += `element face ${f.length}\nproperty list uchar int vertex_indices\nend_header\n`;
  for (const [x, y, z] of v) {
    const c = [(x + 1) * 110 + 20, (y + 1) * 110 + 20, (z + 1) * 110 + 20].map(Math.round);
    s += `${x} ${y} ${z} ${c[0]} ${c[1]} ${c[2]}\n`;
  }
  for (const t of f) s += `3 ${t.join(" ")}\n`;
  writeFileSync(join(out, "cube.ply"), s);
}

// ---------------------------------------------------------------- sphere.obj
{
  const rings = 48, sectors = 96;
  let s = "# generated\no sphere\n";
  const idx = (r, c) => r * (sectors + 1) + c + 1;
  for (let r = 0; r <= rings; r++) {
    const phi = (Math.PI * r) / rings;
    for (let c = 0; c <= sectors; c++) {
      const theta = (2 * Math.PI * c) / sectors;
      const x = Math.sin(phi) * Math.cos(theta);
      const y = Math.cos(phi);
      const z = Math.sin(phi) * Math.sin(theta);
      s += `v ${x.toFixed(5)} ${y.toFixed(5)} ${z.toFixed(5)}\n`;
      s += `vn ${x.toFixed(5)} ${y.toFixed(5)} ${z.toFixed(5)}\n`;
    }
  }
  for (let r = 0; r < rings; r++) {
    for (let c = 0; c < sectors; c++) {
      const a = idx(r, c), b = idx(r, c + 1), d = idx(r + 1, c), e = idx(r + 1, c + 1);
      s += `f ${a}//${a} ${d}//${d} ${e}//${e}\n`;
      s += `f ${a}//${a} ${e}//${e} ${b}//${b}\n`;
    }
  }
  writeFileSync(join(out, "sphere.obj"), s);
}

// ---------------------------------------------------------------- cube.stl (binary)
{
  const tris = [];
  const q = [
    [[-1, -1, -1], [-1, 1, -1], [1, 1, -1]], [[-1, -1, -1], [1, 1, -1], [1, -1, -1]],
    [[-1, -1, 1], [1, -1, 1], [1, 1, 1]], [[-1, -1, 1], [1, 1, 1], [-1, 1, 1]],
    [[-1, -1, -1], [1, -1, -1], [1, -1, 1]], [[-1, -1, -1], [1, -1, 1], [-1, -1, 1]],
    [[1, 1, -1], [-1, 1, -1], [-1, 1, 1]], [[1, 1, -1], [-1, 1, 1], [1, 1, 1]],
    [[1, -1, -1], [1, 1, -1], [1, 1, 1]], [[1, -1, -1], [1, 1, 1], [1, -1, 1]],
    [[-1, -1, -1], [-1, -1, 1], [-1, 1, 1]], [[-1, -1, -1], [-1, 1, 1], [-1, 1, -1]],
  ];
  for (const t of q) tris.push(t);
  const buf = Buffer.alloc(84 + tris.length * 50);
  buf.write("binary stl", 0);
  buf.writeUInt32LE(tris.length, 80);
  let o = 84;
  for (const t of tris) {
    o += 12;
    for (const v of t) for (const c of v) { buf.writeFloatLE(c, o); o += 4; }
    o += 2;
  }
  writeFileSync(join(out, "cube.stl"), buf);
}

// ---------------------------------------------------------------- splats.ply (3DGS)
// A torus of gaussians, degree 0, in the INRIA reference schema.
{
  const n = 60000;
  const props = [
    "x", "y", "z", "nx", "ny", "nz",
    "f_dc_0", "f_dc_1", "f_dc_2",
    "opacity", "scale_0", "scale_1", "scale_2",
    "rot_0", "rot_1", "rot_2", "rot_3",
  ];
  let head = `ply\nformat binary_little_endian 1.0\nelement vertex ${n}\n`;
  for (const p of props) head += `property float ${p}\n`;
  head += "end_header\n";

  const body = Buffer.alloc(n * props.length * 4);
  let o = 0;
  const put = (v) => { body.writeFloatLE(v, o); o += 4; };
  const C0 = 0.28209479177387814;
  for (let i = 0; i < n; i++) {
    const u = rand() * Math.PI * 2;
    const v = rand() * Math.PI * 2;
    const R = 1.0, r = 0.36;
    const x = (R + r * Math.cos(v)) * Math.cos(u);
    const y = r * Math.sin(v);
    const z = (R + r * Math.cos(v)) * Math.sin(u);
    put(x); put(y); put(z);
    put(0); put(0); put(0);
    // Hue around the ring, converted back through the SH degree-0 basis.
    const rgb = [
      0.5 + 0.5 * Math.cos(u),
      0.5 + 0.5 * Math.cos(u + 2.1),
      0.5 + 0.5 * Math.cos(u + 4.2),
    ];
    for (const c of rgb) put((c - 0.5) / C0);
    put(4.0);                       // logit, sigmoid -> 0.982
    const s = Math.log(0.016);      // stored as natural log
    put(s); put(s); put(s);
    put(1); put(0); put(0); put(0); // identity quaternion, w first
  }
  writeFileSync(join(out, "splats.ply"), Buffer.concat([Buffer.from(head, "ascii"), body]));
}

// ---------------------------------------------------------------- cube.glb
// Two nested nodes, so the importer's transform flattening is exercised: the second cube is
// translated by its parent and must land beside the first rather than on top of it.
{
  const positions = [], normals = [], indices = [];
  const faces = [
    { n: [0, 0, 1], c: [[-1, -1, 1], [1, -1, 1], [1, 1, 1], [-1, 1, 1]] },
    { n: [0, 0, -1], c: [[1, -1, -1], [-1, -1, -1], [-1, 1, -1], [1, 1, -1]] },
    { n: [1, 0, 0], c: [[1, -1, 1], [1, -1, -1], [1, 1, -1], [1, 1, 1]] },
    { n: [-1, 0, 0], c: [[-1, -1, -1], [-1, -1, 1], [-1, 1, 1], [-1, 1, -1]] },
    { n: [0, 1, 0], c: [[-1, 1, 1], [1, 1, 1], [1, 1, -1], [-1, 1, -1]] },
    { n: [0, -1, 0], c: [[-1, -1, -1], [1, -1, -1], [1, -1, 1], [-1, -1, 1]] },
  ];
  for (const f of faces) {
    const base = positions.length / 3;
    for (const c of f.c) { positions.push(...c); normals.push(...f.n); }
    indices.push(base, base + 1, base + 2, base, base + 2, base + 3);
  }

  const posBuf = Buffer.alloc(positions.length * 4);
  positions.forEach((v, i) => posBuf.writeFloatLE(v, i * 4));
  const nrmBuf = Buffer.alloc(normals.length * 4);
  normals.forEach((v, i) => nrmBuf.writeFloatLE(v, i * 4));
  const idxBuf = Buffer.alloc(indices.length * 2);
  indices.forEach((v, i) => idxBuf.writeUInt16LE(v, i * 2));

  const pad4 = (b) => (b.length % 4 === 0 ? b : Buffer.concat([b, Buffer.alloc(4 - (b.length % 4))]));
  const bin = Buffer.concat([pad4(posBuf), pad4(nrmBuf), pad4(idxBuf)]);
  const posLen = pad4(posBuf).length, nrmLen = pad4(nrmBuf).length;

  const min = [-1, -1, -1], max = [1, 1, 1];
  const json = {
    asset: { version: "2.0", generator: "make-corpus" },
    scene: 0,
    scenes: [{ nodes: [0, 1] }],
    nodes: [
      { mesh: 0, name: "cube_a" },
      { name: "offset", translation: [3, 0, 0], children: [2] },
      { mesh: 0, name: "cube_b" },
    ],
    meshes: [{
      name: "cube",
      primitives: [{ attributes: { POSITION: 0, NORMAL: 1 }, indices: 2, material: 0, mode: 4 }],
    }],
    materials: [{
      name: "base",
      pbrMetallicRoughness: { baseColorFactor: [0.29, 0.56, 0.85, 1.0], metallicFactor: 0.0, roughnessFactor: 0.7 },
    }],
    accessors: [
      { bufferView: 0, componentType: 5126, count: positions.length / 3, type: "VEC3", min, max },
      { bufferView: 1, componentType: 5126, count: normals.length / 3, type: "VEC3" },
      { bufferView: 2, componentType: 5123, count: indices.length, type: "SCALAR" },
    ],
    bufferViews: [
      { buffer: 0, byteOffset: 0, byteLength: posBuf.length, target: 34962 },
      { buffer: 0, byteOffset: posLen, byteLength: nrmBuf.length, target: 34962 },
      { buffer: 0, byteOffset: posLen + nrmLen, byteLength: idxBuf.length, target: 34963 },
    ],
    buffers: [{ byteLength: bin.length }],
  };

  let jsonBuf = Buffer.from(JSON.stringify(json), "utf8");
  while (jsonBuf.length % 4 !== 0) jsonBuf = Buffer.concat([jsonBuf, Buffer.from(" ")]);

  const header = Buffer.alloc(12);
  header.write("glTF", 0, "ascii");
  header.writeUInt32LE(2, 4);
  header.writeUInt32LE(12 + 8 + jsonBuf.length + 8 + bin.length, 8);
  const jsonHdr = Buffer.alloc(8);
  jsonHdr.writeUInt32LE(jsonBuf.length, 0);
  jsonHdr.write("JSON", 4, "ascii");
  const binHdr = Buffer.alloc(8);
  binHdr.writeUInt32LE(bin.length, 0);
  binHdr.write("BIN\0", 4, "ascii");

  writeFileSync(join(out, "cube.glb"), Buffer.concat([header, jsonHdr, jsonBuf, binHdr, bin]));
}

// ------------------------------------------------- textured.obj + .mtl + textures/checker.png
// Exercises mtllib loading, usemtl splitting, and texture resolution through a subdirectory.
{
  mkdirSync(join(out, "textures"), { recursive: true });

  const W = 64, H = 64;
  writePng(join(out, "textures", "checker.png"), W, H, (x, y) => {
    const on = ((x >> 3) + (y >> 3)) % 2 === 0;
    const hue = (x / W) * 255;
    return on
      ? [235, Math.round(70 + hue * 0.6), 90]
      : [Math.round(hue * 0.5), 40, Math.round(200 - hue * 0.5)];
  });

  // Two quads: one textured, one plain, so the usemtl split is visible.
  const obj = `mtllib textured.mtl
o panels
v -1.2 -1 0
v 0.0 -1 0
v 0.0 1 0
v -1.2 1 0
v 0.1 -1 0
v 1.3 -1 0
v 1.3 1 0
v 0.1 1 0
vt 0 0
vt 1 0
vt 1 1
vt 0 1
vn 0 0 1
usemtl checker
f 1/1/1 2/2/1 3/3/1
f 1/1/1 3/3/1 4/4/1
usemtl plain
f 5/1/1 6/2/1 7/3/1
f 5/1/1 7/3/1 8/4/1
`;
  writeFileSync(join(out, "textured.obj"), obj);

  // The map line carries options and a Windows absolute path, which is what real exporters write.
  const mtl = `newmtl checker
Kd 1 1 1
Ns 120
map_Kd -s 1 1 1 D:\\SomeOldMachine\\project\\textures\\checker.png

newmtl plain
Kd 0.85 0.45 0.25
Ns 8
`;
  writeFileSync(join(out, "textured.mtl"), mtl);
}

console.log("corpus written to", out);

// ------------------------------------------------- panorama360.png + panorama-flat.png
// An equirectangular test pattern. Every feature of it is there to make a wrong panorama
// obviously wrong rather than merely odd:
//
//   - Four coloured discs on the equator at 0, 90, 180 and 270 degrees of longitude. Looking
//     straight ahead must centre the RED one; if the sign of the longitude is flipped, GREEN and
//     BLUE swap sides.
//   - White at the north pole, near-black at the south. A vertical flip is instant to see.
//   - A grid every 15 degrees, with heavier lines on the equator and the prime meridian, so a
//     wrong vertical arc shows up as squashed or stretched squares.
//   - A hue ramp across longitude, so even a featureless region tells you which way you are facing.
//
// Written twice: once carrying GPano metadata, once without, so both the "declared" and the
// "guessed from a 2:1 shape" paths have a fixture.
{
  const W = 4096, H = 2048;

  const discs = [
    { lon: 0, rgb: [230, 60, 60] },     // front
    { lon: 90, rgb: [70, 210, 90] },    // right
    { lon: 180, rgb: [70, 120, 235] },  // back
    { lon: 270, rgb: [240, 205, 60] },  // left
  ];

  const pixel = (x, y) => {
    const lon = (x / W) * 360;          // 0 at the left edge, which is longitude -180
    const lat = 90 - (y / H) * 180;

    for (const d of discs) {
      // Longitude 0 sits at the centre of the image, which is the equirectangular convention.
      const centreX = ((d.lon + 180) % 360) / 360 * W;
      const dx = Math.min(Math.abs(x - centreX), W - Math.abs(x - centreX));
      const dy = y - H / 2;
      if (dx * dx + dy * dy < 70 * 70) return d.rgb;
    }

    // Poles: a cap either end, so up and down are never in doubt.
    if (lat > 75) return [245, 245, 245];
    if (lat < -75) return [18, 18, 22];

    const onMeridian = Math.abs(x - W / 2) < 3;
    const onEquator = Math.abs(y - H / 2) < 3;
    if (onMeridian || onEquator) return [255, 255, 255];
    if (x % Math.round(W / 24) < 2 || y % Math.round(H / 12) < 2) return [150, 158, 170];

    // A hue ramp around the horizon, dimmed towards the poles.
    const t = lon / 360;
    const shade = 0.35 + 0.45 * Math.cos((lat * Math.PI) / 180);
    return [
      Math.round(255 * shade * (0.5 + 0.5 * Math.cos(2 * Math.PI * t))),
      Math.round(255 * shade * (0.5 + 0.5 * Math.cos(2 * Math.PI * (t - 1 / 3)))),
      Math.round(255 * shade * (0.5 + 0.5 * Math.cos(2 * Math.PI * (t - 2 / 3)))),
    ];
  };

  const gpano = `<?xpacket begin="\ufeff" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:GPano="http://ns.google.com/photos/1.0/panorama/"
    GPano:UsePanoramaViewer="True"
    GPano:ProjectionType="equirectangular"
    GPano:FullPanoWidthPixels="${W}"
    GPano:FullPanoHeightPixels="${H}"
    GPano:CroppedAreaImageWidthPixels="${W}"
    GPano:CroppedAreaImageHeightPixels="${H}"
    GPano:CroppedAreaLeftPixels="0"
    GPano:CroppedAreaTopPixels="0"/>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>`;

  writePng(join(out, "panorama360.png"), W, H, pixel, [xmpChunk(gpano)]);
  // The same pixels with nothing declared, under a name that says nothing either. This one has
  // to be recognized from its 2:1 shape alone, which is the weakest of the three signals and the
  // one most likely to regress.
  writePng(join(out, "unlabelled-2to1.png"), W, H, pixel);
}

// ------------------------------------------------- points.xyz + survey.pts
// Two ASCII point clouds, exercising the two column layouts that actually turn up and the
// coordinate magnitude that breaks a naive reader.
{
  const N = 4000;
  let xyz = "# a helix, x y z r g b\n";
  for (let i = 0; i < N; i++) {
    const t = (i / N) * Math.PI * 12;
    const r = 1 - i / N;
    const x = Math.cos(t) * r;
    const y = (i / N) * 2 - 1;
    const z = Math.sin(t) * r;
    const cr = Math.round(127 + 127 * Math.cos(t));
    const cg = Math.round(127 + 127 * Math.sin(t * 0.5));
    const cb = Math.round(255 * (i / N));
    xyz += `${x.toFixed(5)} ${y.toFixed(5)} ${z.toFixed(5)} ${cr} ${cg} ${cb}\n`;
  }
  writeFileSync(join(out, "points.xyz"), xyz);

  // Leica's layout, with a leading count and real UTM eastings. Nine significant digits with a
  // centimetre step: the whole point is that f32 cannot hold this without recentring first.
  const M = 2000;
  let pts = `${M}\n`;
  for (let i = 0; i < M; i++) {
    const a = (i / M) * Math.PI * 2;
    const e = 500000 + Math.cos(a) * 3;
    const n = 4600000 + Math.sin(a) * 3;
    const h = 100 + Math.sin(a * 4) * 0.5;
    pts += `${e.toFixed(3)} ${n.toFixed(3)} ${h.toFixed(3)} -1500 ${(i * 7) % 256} ${(i * 13) % 256} ${(i * 29) % 256}\n`;
  }
  writeFileSync(join(out, "survey.pts"), pts);
}

// ------------------------------------------------- torus.off
// Quad faces, so the fan triangulation is exercised rather than assumed.
{
  const RINGS = 40;
  const SIDES = 20;
  const R = 1.0;
  const r = 0.35;
  const verts = [];
  for (let i = 0; i < RINGS; i++) {
    const u = (i / RINGS) * Math.PI * 2;
    for (let j = 0; j < SIDES; j++) {
      const v = (j / SIDES) * Math.PI * 2;
      verts.push([
        (R + r * Math.cos(v)) * Math.cos(u),
        r * Math.sin(v),
        (R + r * Math.cos(v)) * Math.sin(u),
      ]);
    }
  }
  const faces = [];
  for (let i = 0; i < RINGS; i++) {
    for (let j = 0; j < SIDES; j++) {
      const a = i * SIDES + j;
      const b = ((i + 1) % RINGS) * SIDES + j;
      const c = ((i + 1) % RINGS) * SIDES + ((j + 1) % SIDES);
      const d = i * SIDES + ((j + 1) % SIDES);
      faces.push([a, b, c, d]);
    }
  }
  let off = `OFF\n# a torus, quad faces\n${verts.length} ${faces.length} 0\n`;
  for (const p of verts) off += `${p[0].toFixed(6)} ${p[1].toFixed(6)} ${p[2].toFixed(6)}\n`;
  for (const f of faces) off += `4 ${f.join(" ")}\n`;
  writeFileSync(join(out, "torus.off"), off);
}

// ------------------------------------------------- volume.mhd + volume.raw, volume.mha
// A 64-cubed scalar field, written twice: split with a raw sidecar, and inline and compressed.
{
  const D = 64;
  const voxels = Buffer.alloc(D * D * D * 2);
  for (let z = 0; z < D; z++) {
    for (let y = 0; y < D; y++) {
      for (let x = 0; x < D; x++) {
        const dx = x / D - 0.5;
        const dy = y / D - 0.5;
        const dz = z / D - 0.5;
        const d = Math.sqrt(dx * dx + dy * dy + dz * dz);
        // A shell, so a raymarch has something with an inside and an outside.
        const shell = Math.max(0, 1 - Math.abs(d - 0.32) * 12);
        const value = Math.round(shell * 3000) - 1000;
        voxels.writeInt16LE(value, ((z * D + y) * D + x) * 2);
      }
    }
  }

  const header = (dataFile, compressed, size) =>
    `ObjectType = Image\nNDims = 3\nBinaryData = True\n` +
    `BinaryDataByteOrderMSB = False\nCompressedData = ${compressed ? "True" : "False"}\n` +
    (compressed ? `CompressedDataSize = ${size}\n` : "") +
    `TransformMatrix = 1 0 0 0 1 0 0 0 1\nOffset = 0 0 0\n` +
    `ElementSpacing = 0.5 0.5 1.0\nDimSize = ${D} ${D} ${D}\n` +
    `ElementType = MET_SHORT\nElementDataFile = ${dataFile}\n`;

  writeFileSync(join(out, "volume.raw"), voxels);
  writeFileSync(join(out, "volume.mhd"), header("volume.raw", false, 0));

  const packed = zlibSync(voxels);
  writeFileSync(
    join(out, "volume.mha"),
    Buffer.concat([Buffer.from(header("LOCAL", true, packed.length), "ascii"), packed]),
  );
}
