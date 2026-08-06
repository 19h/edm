#!/usr/bin/env bun
/**
 * Pins the Ardent client against the TypeScript module the original
 * `import()`s at runtime.
 *
 * Divergence C1 replaced a dynamic import with a compiled-in port. This is what
 * keeps that honest: the real `ardent.ts` is executed here and its four
 * load-bearing exports recorded, so the Rust cannot drift from the module it
 * replaced without a test failing.
 *
 *   bun xtask/oracle/bless-ardent.ts crates/edm-core/tests/fixtures
 *
 * Set ARDENT_MODULE to point at a different checkout.
 */

import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const outDir = process.argv[2] ?? "crates/edm-core/tests/fixtures";
const modulePath = process.env["ARDENT_MODULE"]?.trim() || "/models/dev/edtrade/src/ardent.ts";
mkdirSync(outDir, { recursive: true });

const ardent = (await import(modulePath)) as {
  BASE_URL: string;
  systemUrl: (name: string) => string;
  stationSearchUrl: (name: string) => string;
  parseSystem: (u: unknown) => unknown;
  parseStationMatches: (u: unknown) => unknown;
};

const names = [
  "Sol", "Colonia", "Jaques Station", "Hyades Sector NI-X a16-0",
  "Eol Prou LRL-C d22-215", "Ohm City", "Hutton Orbital", "Böthold",
  "Ånderson's Rest", "Nuñez Terminal", "Łódź Port", "Δ Velorum",
  "a-_.!~*'()", "/?&=#+%", "with  double  spaces", " leading", "trailing ",
  "", "100% Proof", "Smith & Sons", "\"quoted\"", "back\\slash",
  "emoji 🚀 station", "tab\there", "new\nline", "半人马座", "Ω Carinae",
];

const systemPayloads: unknown[] = [
  { systemName: "Sol", systemAddress: 10477373803, systemX: 0, systemY: 0, systemZ: 0 },
  { systemName: "Colonia", systemAddress: 3238296097059, systemX: -9530.5, systemY: -910.28125, systemZ: 19808.125 },
  // Every way the parser is allowed to refuse.
  { systemName: "Sol", systemAddress: 1, systemX: 0, systemY: 0 },
  { systemName: 42, systemAddress: 1, systemX: 0, systemY: 0, systemZ: 0 },
  { systemName: "Sol", systemAddress: null, systemX: 0, systemY: 0, systemZ: 0 },
  { systemName: "Sol", systemAddress: "1", systemX: 0, systemY: 0, systemZ: 0 },
  { systemName: "Sol", systemAddress: Infinity, systemX: 0, systemY: 0, systemZ: 0 },
  { systemName: "", systemAddress: 1, systemX: 0, systemY: 0, systemZ: 0 },
  {},
  [],
  null,
  "not an object",
  // Precision: an address past 2^53.
  { systemName: "Big", systemAddress: 9007199254740993, systemX: 1.5, systemY: -2.25, systemZ: 0 },
];

const stationPayloads: unknown[] = [
  [
    { stationName: "Jaques Station", systemName: "Colonia", stationType: "Coriolis", maxLandingPadSize: 3 },
    { stationName: "Ohm City", systemName: "Colonia", stationType: null, maxLandingPadSize: 2 },
  ],
  [{ stationName: "Only Name", systemName: "Somewhere" }],
  // Rows that must be skipped rather than fail the whole parse.
  [{ stationName: 1, systemName: "X" }, { stationName: "Y", systemName: 2 }, { stationName: "Z", systemName: "W" }],
  [null, "string", 42, { stationName: "Survivor", systemName: "S" }],
  [],
  {},
  null,
];

const rows: string[] = [`BASE_URL\t${ardent.BASE_URL}`];
for (const n of names) {
  rows.push(`systemUrl\t${JSON.stringify(n)}\t${ardent.systemUrl(n)}`);
  rows.push(`stationSearchUrl\t${JSON.stringify(n)}\t${ardent.stationSearchUrl(n)}`);
}
for (const p of systemPayloads) {
  rows.push(
    `parseSystem\t${JSON.stringify(p)}\t${JSON.stringify(ardent.parseSystem(p) ?? null)}`,
  );
}
for (const p of stationPayloads) {
  rows.push(
    `parseStationMatches\t${JSON.stringify(p)}\t${JSON.stringify(ardent.parseStationMatches(p))}`,
  );
}

writeFileSync(
  join(outDir, "ardent_contract.tsv"),
  `# the four ardent.ts exports game-internal-api.ts duck-types, recorded from the real module\n` +
    `# source: ${modulePath}\n# bun ${Bun.version}\n` +
    `# kind<TAB>input (JSON)<TAB>output\n${rows.join("\n")}\n`,
);
console.log(`ardent_contract.tsv: ${rows.length} rows from ${modulePath}`);
