#!/usr/bin/env bun
/**
 * Pins the ID64 system-address codec.
 *
 * The oracle here is not a re-transcription of the algorithm — it is the
 * algorithm. The three functions are sliced verbatim out of `market-request.ts`
 * and executed, so a fixture can never agree with a mistake this generator and
 * the Rust both made.
 *
 *   bun xtask/oracle/bless-id64.ts crates/edm-core/tests/fixtures
 */

import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";

const outDir = process.argv[2] ?? "crates/edm-core/tests/fixtures";
mkdirSync(outDir, { recursive: true });

// ---------------------------------------------------------------------------
// Load the original implementation by slicing it out of the script.
// ---------------------------------------------------------------------------

const SOURCE = "market-request.ts";
const FIRST_LINE = 2348; // `const SECTOR_SIZE`
const LAST_LINE = 2445; // closing brace of `containsCoordinates`

const lines = (await Bun.file(SOURCE).text()).split("\n");
const anchors: [number, string][] = [
  [FIRST_LINE, "const SECTOR_SIZE"],
  [2369, "function decodeSystemAddress"],
  [2404, "function encodeSystemAddress"],
  [2440, "function containsCoordinates"],
];
for (const [line, expected] of anchors) {
  if (!lines[line - 1]?.startsWith(expected)) {
    throw new Error(
      `${SOURCE}:${line} no longer starts with ${JSON.stringify(expected)} — ` +
        `the slice bounds in this generator are stale`,
    );
  }
}

const slice = lines.slice(FIRST_LINE - 1, LAST_LINE).join("\n");
const modulePath = join(tmpdir(), `edm-id64-oracle-${process.pid}.ts`);
writeFileSync(
  modulePath,
  `${slice}\nexport { decodeSystemAddress, encodeSystemAddress, containsCoordinates, SECTOR_SIZE, GALAXY_ORIGIN };\n`,
);
const ts = await import(modulePath);

// ---------------------------------------------------------------------------

function attempt<T>(f: () => T): string {
  try {
    return JSON.stringify(f());
  } catch (e) {
    return `ERR:${e instanceof Error ? e.message : String(e)}`;
  }
}

const addresses: number[] = [
  // The documented anchor: Hyades Sector NI-X a16-0.
  5378909424384,
  0, 1, 7, 8, 255, 256,
  10477373803, // Sol
  3238296097059, // Colonia
  Number.MAX_SAFE_INTEGER,
  -1, 1.5, NaN, Infinity, Number.MAX_SAFE_INTEGER + 2,
];
{
  let seed = 0x1d64;
  const r = () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = seed;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  // Every mass code, across the whole safe-integer range.
  for (let i = 0; i < 800; i++) {
    addresses.push(Math.floor(r() * Number.MAX_SAFE_INTEGER));
  }
  for (let mc = 0; mc <= 7; mc++) {
    for (let i = 0; i < 40; i++) {
      addresses.push(mc + Math.floor(r() * 2 ** 40) * 8);
    }
  }
}

const decodeRows = addresses.map((a) => `${a}\t${attempt(() => ts.decodeSystemAddress(a))}`);
writeFileSync(
  join(outDir, "id64_decode.tsv"),
  `# decodeSystemAddress — address<TAB>JSON parts, or ERR:<message>\n` +
    `# sliced verbatim from ${SOURCE}:${FIRST_LINE}-${LAST_LINE} under bun ${Bun.version}\n` +
    `${decodeRows.join("\n")}\n`,
);
console.log(`id64_decode.tsv: ${decodeRows.length} rows`);

// Encode is exercised the way `runMarkets` exercises it: decode an address,
// then re-pack it from coordinates inside its boxel. Fractional offsets matter
// — Colonia's x is -9530.5, and an integer implementation lands in the wrong
// boxel.
const encodeRows: string[] = [];
for (const a of addresses) {
  let parts: any;
  try {
    parts = ts.decodeSystemAddress(a);
  } catch {
    continue;
  }
  const offsets = [0, 0.5, parts.boxelSize - 0.5, parts.boxelSize / 3];
  for (const d of offsets) {
    const c = { x: parts.origin.x + d, y: parts.origin.y + d, z: parts.origin.z + d };
    encodeRows.push(
      [
        c.x, c.y, c.z, parts.massCode, parts.index,
        attempt(() => ts.encodeSystemAddress(c, parts.massCode, parts.index)),
        String(ts.containsCoordinates(parts, c)),
      ].join("\t"),
    );
  }
}
// Coordinates deliberately off the grid, to pin the two rejection messages.
for (const c of [
  { x: -1e9, y: 0, z: 0 },
  { x: 0, y: -1e9, z: 0 },
  { x: 0, y: 0, z: -1e9 },
  { x: 1e9, y: 0, z: 0 },
  { x: 0, y: 1e9, z: 0 },
  { x: 0, y: 0, z: 1e9 },
]) {
  for (const mc of [0, 3, 7, 8, -1, 1.5]) {
    encodeRows.push(
      [c.x, c.y, c.z, mc, 0, attempt(() => ts.encodeSystemAddress(c, mc, 0)), "false"].join("\t"),
    );
  }
}
writeFileSync(
  join(outDir, "id64_encode.tsv"),
  `# encodeSystemAddress — x<TAB>y<TAB>z<TAB>massCode<TAB>index<TAB>address or ERR<TAB>containsCoordinates\n` +
    `# sliced verbatim from ${SOURCE}:${FIRST_LINE}-${LAST_LINE} under bun ${Bun.version}\n` +
    `${encodeRows.join("\n")}\n`,
);
console.log(`id64_encode.tsv: ${encodeRows.length} rows`);
