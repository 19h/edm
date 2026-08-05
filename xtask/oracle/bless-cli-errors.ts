#!/usr/bin/env bun
/**
 * The command line's failure surface, captured from the original.
 *
 * Every case here fails before any socket is opened, so the fixture is a pure
 * record of the parser and the accessors: which message, on which stream, and
 * with which exit code. Those three together are most of R38-R56.
 *
 *   bun xtask/oracle/bless-cli-errors.ts crates/edm-core/tests/fixtures
 */

import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const outDir = process.argv[2] ?? "crates/edm-core/tests/fixtures";
mkdirSync(outDir, { recursive: true });

/** Credentials, so a case can get past `loadCredentials` to the real check. */
const CREDS = [
  "--cmdr-id", "F1234567",
  "--machine-id", "machine-1",
  "--machine-token", "m".repeat(80),
  "--auth-token", "a".repeat(2024),
];

const CASES: [string, string[]][] = [
  // --- parse errors: exit 2, message + blank line on stderr, usage on stdout
  ["bare_double_dash", ["market", "--"]],
  ["underscore_negation", ["market", "--no_json"]],
  ["negate_a_value_flag", ["market", "--no-qty"]],
  ["negate_unknown", ["market", "--no-nonsense"]],
  ["value_flag_without_value", ["market", "--market"]],
  ["value_flag_before_another_flag", ["market", "--market-id", "--json"]],
  ["switch_with_bad_literal", ["market", "--json=maybe"]],
  ["unknown_option", ["market", "--nonsense"]],
  ["unknown_command", ["bogus"]],
  ["empty_token_does_not_take_the_command_slot", ["", "Colonia"]],
  ["single_dash_becomes_a_command", ["-x"]],
  ["equals_in_a_value_survives", ["market", "--item=a=b", "--nonsense"]],

  // --- accessor errors: exit 1, message alone on stderr
  ["missing_credentials", ["market", "--market-id", "1"]],
  // The alias is what the user typed; the message names the canonical flag.
  ["alias_names_the_canonical_flag", ["trade", "--market-id", "1", "--type", "buy",
    "--item", "1", "--quantity", "abc", "--no-resolve", ...CREDS]],
  ["bad_market_id", ["market", "--market-id", "abc", ...CREDS]],
  ["oversized_market_id", ["market", "--market-id", "9".repeat(30), ...CREDS]],
  ["short_machine_token", ["market", "--market-id", "1", "--cmdr-id", "F1", "--machine-id", "m",
    "--machine-token", "short", "--auth-token", "a".repeat(2024)]],
  ["non_ascii_machine_token", ["market", "--market-id", "1", "--cmdr-id", "F1", "--machine-id", "m",
    "--machine-token", "é".repeat(80), "--auth-token", "a".repeat(2024)]],
  ["bad_nonce", ["market", "--market-id", "1", "--nonce", "xyz", "--dry-run", ...CREDS]],
  ["market_needs_a_target", ["market", ...CREDS]],
  ["markets_needs_a_name", ["markets", ...CREDS]],

  // --- trade guards, all before any network under --no-resolve
  ["trade_bad_type", ["trade", "--market-id", "1", "--type", "nonsense", "--item", "1",
    "--qty", "1", "--no-resolve", ...CREDS]],
  ["trade_missing_qty", ["trade", "--market-id", "1", "--type", "buy", "--item", "1",
    "--no-resolve", ...CREDS]],
  ["trade_zero_qty", ["trade", "--market-id", "1", "--type", "buy", "--item", "1",
    "--qty", "0", "--no-resolve", ...CREDS]],
  ["trade_no_resolve_needs_numeric_item", ["trade", "--market-id", "1", "--type", "buy",
    "--item", "silver", "--qty", "1", "--no-resolve", ...CREDS]],
  ["trade_no_resolve_needs_price", ["trade", "--market-id", "1", "--type", "buy",
    "--item", "1", "--qty", "1", "--no-resolve", ...CREDS]],
  ["trade_read_order_market_id_first", ["trade", "--type", "nonsense", "--qty", "0",
    "--item", "1", "--no-resolve", ...CREDS]],
  ["batch_fill_needs_buy", ["trade", "--market-id", "1", "--type", "sell", "--item", "a,b",
    "--fill", "--cargo", "10", ...CREDS]],
  ["batch_fill_needs_cargo", ["trade", "--market-id", "1", "--type", "buy", "--item", "a,b",
    "--fill", ...CREDS]],
  ["batch_fill_rejects_no_cap", ["trade", "--market-id", "1", "--type", "buy", "--item", "a,b",
    "--fill", "--cargo", "10", "--no-cap", ...CREDS]],
  ["batch_needs_qty", ["trade", "--market-id", "1", "--type", "buy", "--item", "a,b", ...CREDS]],
  ["batch_zero_qty_under_fill", ["trade", "--market-id", "1", "--type", "buy", "--item", "a,b",
    "--fill", "--cargo", "10", "--qty", "0", ...CREDS]],
  ["batch_rejects_no_resolve", ["trade", "--market-id", "1", "--type", "buy", "--item", "a,b",
    "--qty", "1", "--no-resolve", ...CREDS]],
  ["watch_needs_a_stop", ["trade", "--market-id", "1", "--type", "buy", "--item", "a",
    "--qty", "1", "--watch", ...CREDS]],
  ["interval_out_of_range", ["trade", "--market-id", "1", "--type", "buy", "--item", "a,b",
    "--qty", "1", "--interval", "0.01", ...CREDS]],
  ["empty_item_list", ["trade", "--market-id", "1", "--type", "buy", "--item", ",,",
    "--qty", "1", ...CREDS]],

  // --- R47: a prototype key is consumed and then throws at toLowerCase
  // `--dry-run` is read by openSession, so the poison is reached immediately.
  ["poisoned_switch", ["market", "--market-id", "1", "--dry-run", "constructor", ...CREDS]],
  // The contrast: with one item `runTrade` reads `--fill` and hits the poison,
  // where two items would have short-circuited past it.
  ["poisoned_switch_reached_via_fill", ["trade", "--market-id", "1", "--type", "buy",
    "--item", "a", "--qty", "1", "--fill", "constructor", ...CREDS]],

  // --- R48: help wins over everything
  ["help_command", ["help"]],
  ["help_switch_after_bad_command", ["bogus", "--help"]],
  ["dash_h", ["-h"]],
];

const rows: string[] = [];
for (const [name, argv] of CASES) {
  const proc = Bun.spawnSync(["bun", "market-request.ts", ...argv], {
    env: { ...process.env, COLUMNS: "100", MARKET_ID: "", COMMANDER_ID: "", MACHINE_ID: "",
           MACHINE_TOKEN: "", AUTH_TOKEN: "" },
  });
  rows.push(
    [
      name,
      JSON.stringify(argv),
      String(proc.exitCode),
      JSON.stringify(proc.stdout.toString()),
      JSON.stringify(proc.stderr.toString()),
    ].join("\t"),
  );
  const first = proc.stderr.toString().split("\n")[0] ?? "";
  console.log(`${name}: exit ${proc.exitCode}  ${first.slice(0, 70)}`);
}

writeFileSync(
  join(outDir, "cli_errors.tsv"),
  `# the command line's failure surface — every case fails before any socket\n` +
    `# bun ${Bun.version}\n` +
    `# name<TAB>argv<TAB>exit<TAB>stdout<TAB>stderr (all JSON-quoted)\n${rows.join("\n")}\n`,
);
console.log(`\nwrote ${rows.length} cases`);
