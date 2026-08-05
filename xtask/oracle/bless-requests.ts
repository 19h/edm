#!/usr/bin/env bun
/**
 * Golden request envelopes.
 *
 * With `--nonce`, `--f-time` and `--request-time` pinned, the encrypted query is
 * a pure function of the envelope — so `--dry-run --json` produces a complete,
 * offline, byte-exact reference for the whole request-building path: envelope
 * order, number stringification, ChaCha20, base64, and the header set.
 *
 * This is the strongest check available without a network, because the query is
 * ciphertext: a single wrong byte anywhere upstream changes all of it.
 *
 *   bun xtask/oracle/bless-requests.ts crates/edm/tests/fixtures
 */

import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const outDir = process.argv[2] ?? "crates/edm/tests/fixtures";
mkdirSync(outDir, { recursive: true });

const CREDENTIALS = [
  "--cmdr-id", "F1234567",
  "--machine-id", "machine-1",
  "--machine-token", "m".repeat(80),
  "--auth-token", "a".repeat(2024),
];

const STAMP = [
  "--nonce", "0123456789ab",
  "--f-time", "1700000000",
  "--request-time", "12345",
];

/** Each case is a name and the argv that produces a request with no network. */
const CASES: [string, string[]][] = [
  [
    "trade_buy",
    ["trade", "--market-id", "4306502403", "--type", "buy", "--item", "128049204",
     "--qty", "7", "--unit-price", "517", "--no-resolve"],
  ],
  [
    "trade_sell_stolen",
    ["trade", "--market-id", "128667761", "--type", "sell", "--item", "128049152",
     "--qty", "13", "--final-qty", "130", "--unit-price", "3340", "--stolen", "--no-resolve"],
  ],
  [
    "trade_leading_zero_market",
    // R53: `trade` never parses --market-id, so the zeros survive to the wire.
    ["trade", "--market-id", "0004306502403", "--type", "buy", "--item", "1",
     "--qty", "1", "--unit-price", "1", "--no-resolve"],
  ],
  [
    "markets_by_address",
    ["markets", "--address", "5378909424384"],
  ],
  [
    "markets_language",
    // R65: --language is the one unvalidated field, so a non-ASCII value
    // changes the plaintext's byte length and therefore the ciphertext.
    ["markets", "--address", "10477373803", "--language", "fr-Ø"],
  ],
  [
    "markets_method_override",
    ["markets", "--address", "10477373803", "--method", "put"],
  ],
];

const rows: string[] = [];
for (const [name, argv] of CASES) {
  const proc = Bun.spawnSync([
    "bun", "market-request.ts", ...argv, ...STAMP, ...CREDENTIALS, "--dry-run", "--json",
  ]);
  const stdout = proc.stdout.toString();
  if (proc.exitCode !== 0) {
    throw new Error(`${name} exited ${proc.exitCode}: ${proc.stderr.toString()}`);
  }
  const parsed = JSON.parse(stdout) as { request: Record<string, unknown> };
  const request = parsed.request;
  rows.push(
    [
      name,
      JSON.stringify(argv),
      request["method"],
      request["url"],
      JSON.stringify(request["headers"]),
      JSON.stringify(request["envelope"]),
      String(request["plaintextLength"]),
    ].join("\t"),
  );
  console.log(`${name}: ${String(request["url"]).length} chars of URL`);
}

writeFileSync(
  join(outDir, "requests.tsv"),
  `# golden request envelopes, --dry-run --json with a pinned stamp\n` +
    `# bun ${Bun.version}\n` +
    `# name<TAB>argv<TAB>method<TAB>url<TAB>headers<TAB>envelope<TAB>plaintextLength\n` +
    `${rows.join("\n")}\n`,
);
console.log(`\nwrote ${rows.length} golden requests`);
