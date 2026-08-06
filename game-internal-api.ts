#!/usr/bin/env bun

import { writeFileSync } from "node:fs";
import { uptime } from "node:os";
import { randomBytes } from "node:crypto";

const API_ORIGIN = "https://api.orerve.net";

interface Endpoint {
  readonly path: string;
  readonly method: string;
}

/**
 * Verbs come from the game's own `methodCode` values: 1 = GET, 3 = PUT.
 * trade answers `Allow: PUT, OPTIONS` — a GET there is rejected with 405.
 */
const MARKET_LIST: Endpoint = { path: "/2.0/elite/market/list", method: "GET" };
const MARKET_TRADE: Endpoint = { path: "/2.0/elite/market/trade", method: "PUT" };
const STARSYSTEM: Endpoint = { path: "/2.0/elite/starsystem", method: "GET" };
const MARKET_LIST_PATH = MARKET_LIST.path;
const MARKET_TRADE_PATH = MARKET_TRADE.path;

/** Where the Ardent endpoint definitions live; override with ARDENT_MODULE. */
const ARDENT_MODULE = "/home/null/dev/edtrade/src/ardent.ts";
/** Not in ardent.ts, but live: it is the only route that maps a market id to its names. */
const ARDENT_MARKET_URL = (marketId: number): string => `https://api.ardent-insight.com/v2/market/${marketId}`;

/** docs/Developers.md:117 — note the non-standard port and the required trailing slash. */
const EDDN_UPLOAD_URL = "https://eddn.edcd.io:4430/upload/";
const EDDN_SCHEMA = "https://eddn.edcd.io/schemas/commodity/3";
const EDDN_SOFTWARE_NAME = "int-market-sync";
/** MUST be incremented whenever the content of the messages we send changes. */
const EDDN_SOFTWARE_VERSION = "1.0.0";
/** docs/Developers.md:263-265 — commodity data taken from a live game-internal API market endpoint. */
const EDDN_GAME_VERSION = "GameInternal-Live-market";
/** Sweeps run as a pool of workers pulling from one queue, not at a fixed rate. */
const DEFAULT_CONCURRENCY = 5;
const MAX_CONCURRENCY = 16;
const DEFAULT_TIMEOUT_SECONDS = 10;
const DEFAULT_REQUEUES = 3;
const CHACHA_KEY = new TextEncoder().encode("52381239578582178380088936356181");
const encoder = new TextEncoder();

/** Long-lived session identity; the same values are reused by every endpoint. */
interface Credentials {
  readonly commanderId: string;
  readonly machineId: string;
  readonly machineToken: string;
  readonly authToken: string;
}

function parseUnsignedInteger(name: string, value: string): number {
  if (!/^\d+$/.test(value)) throw new Error(`${name} must be an unsigned decimal integer`);
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed)) throw new Error(`${name} is outside the safe integer range`);
  return parsed;
}

function validateAscii(name: string, value: string): string {
  if (!/^[\x20-\x7e]+$/.test(value)) throw new Error(`${name} must contain printable ASCII only`);
  return value;
}

function validateExactLength(name: string, value: string, expected: number): string {
  if (value.length !== expected) {
    throw new Error(`${name} must be exactly ${expected} characters; received ${value.length}`);
  }
  return value;
}

function validateNonce(name: string, value: string): string {
  const nonce = value.toLowerCase();
  if (!/^[0-9a-f]{12}$/.test(nonce)) throw new Error(`${name} must be exactly 12 hexadecimal characters`);
  return nonce;
}

function loadCredentials(args: ParsedArguments): Credentials {
  return {
    commanderId: validateAscii("cmdrId", requireValue(args, "cmdrid", "COMMANDER_ID")),
    machineId: validateAscii("machineId", requireValue(args, "machineid", "MACHINE_ID")),
    machineToken: validateExactLength(
      "machineToken",
      validateAscii("machineToken", requireValue(args, "machinetoken", "MACHINE_TOKEN")),
      80,
    ),
    authToken: validateExactLength(
      "authToken",
      validateAscii("authToken", requireValue(args, "authtoken", "AUTH_TOKEN")),
      2024,
    ),
  };
}

/** Values the game regenerates for every single request. */
interface RequestStamp {
  readonly nonce: string;
  readonly frontierTime: number;
  readonly requestTime: number;
}

function nextStamp(args: ParsedArguments): RequestStamp {
  const nonce = optionalValue(args, "nonce", "NONCE");
  const frontierTime = optionalValue(args, "ftime", "F_TIME");
  const requestTime = optionalValue(args, "requesttime", "REQUEST_TIME");
  return {
    nonce: nonce ? validateNonce("nonce", nonce) : randomBytes(6).toString("hex"),
    frontierTime: frontierTime ? parseUnsignedInteger("fTime", frontierTime) : Math.floor(Date.now() / 1_000),
    // The game uses a wrapping 32-bit millisecond uptime value for Request-Time.
    requestTime: (requestTime ? parseUnsignedInteger("requestTime", requestTime) : Math.floor(uptime() * 1_000)) >>> 0,
  };
}

function rotateLeft(value: number, count: number): number {
  return ((value << count) | (value >>> (32 - count))) >>> 0;
}

function quarterRound(state: Uint32Array, a: number, b: number, c: number, d: number): void {
  state[a] = (state[a]! + state[b]!) >>> 0;
  state[d] = rotateLeft(state[d]! ^ state[a]!, 16);
  state[c] = (state[c]! + state[d]!) >>> 0;
  state[b] = rotateLeft(state[b]! ^ state[c]!, 12);
  state[a] = (state[a]! + state[b]!) >>> 0;
  state[d] = rotateLeft(state[d]! ^ state[a]!, 8);
  state[c] = (state[c]! + state[d]!) >>> 0;
  state[b] = rotateLeft(state[b]! ^ state[c]!, 7);
}

function readUint32LE(bytes: Uint8Array, offset: number): number {
  return (
    bytes[offset]! |
    (bytes[offset + 1]! << 8) |
    (bytes[offset + 2]! << 16) |
    (bytes[offset + 3]! << 24)
  ) >>> 0;
}

function writeUint32LE(bytes: Uint8Array, offset: number, value: number): void {
  bytes[offset] = value;
  bytes[offset + 1] = value >>> 8;
  bytes[offset + 2] = value >>> 16;
  bytes[offset + 3] = value >>> 24;
}

function chacha20Block(key: Uint8Array, counter: number, nonce: Uint8Array): Uint8Array {
  if (key.length !== 32) throw new Error("ChaCha20 key must be 32 bytes");
  if (nonce.length !== 12) throw new Error("IETF ChaCha20 nonce must be 12 bytes");

  const initial = new Uint32Array(16);
  initial.set([0x61707865, 0x3320646e, 0x79622d32, 0x6b206574]);
  for (let index = 0; index < 8; index++) initial[4 + index] = readUint32LE(key, index * 4);
  initial[12] = counter >>> 0;
  initial[13] = readUint32LE(nonce, 0);
  initial[14] = readUint32LE(nonce, 4);
  initial[15] = readUint32LE(nonce, 8);

  const working = initial.slice();
  for (let round = 0; round < 10; round++) {
    quarterRound(working, 0, 4, 8, 12);
    quarterRound(working, 1, 5, 9, 13);
    quarterRound(working, 2, 6, 10, 14);
    quarterRound(working, 3, 7, 11, 15);
    quarterRound(working, 0, 5, 10, 15);
    quarterRound(working, 1, 6, 11, 12);
    quarterRound(working, 2, 7, 8, 13);
    quarterRound(working, 3, 4, 9, 14);
  }

  const block = new Uint8Array(64);
  for (let index = 0; index < 16; index++) {
    writeUint32LE(block, index * 4, (working[index]! + initial[index]!) >>> 0);
  }
  return block;
}

function chacha20(input: Uint8Array, key: Uint8Array, nonce: Uint8Array): Uint8Array {
  const output = new Uint8Array(input.length);
  let counter = 0;

  for (let offset = 0; offset < input.length; offset += 64) {
    const block = chacha20Block(key, counter, nonce);
    const length = Math.min(64, input.length - offset);
    for (let index = 0; index < length; index++) {
      output[offset + index] = input[offset + index]! ^ block[index]!;
    }
    counter = (counter + 1) >>> 0;
    if (counter === 0 && offset + length < input.length) throw new Error("ChaCha20 counter exhausted");
  }

  return output;
}

/** One envelope field: the wire value plus how it should appear on screen (tokens are masked). */
interface EnvelopeField {
  readonly name: string;
  readonly value: string | number;
  readonly display?: string;
}

function secretField(name: string, value: string): EnvelopeField {
  return { name, value, display: `${value.length} chars (hidden)` };
}

function serializeEnvelope(fields: readonly EnvelopeField[]): string {
  // The game concatenates these values directly; it does not percent-encode them here.
  return fields.map((field) => `${field.name}=${field.value}`).join("&");
}

function credentialFields(credentials: Credentials, frontierTime: number): readonly EnvelopeField[] {
  return [
    { name: "fTime", value: frontierTime },
    { name: "machineId", value: credentials.machineId },
    secretField("machineToken", credentials.machineToken),
    secretField("authToken", credentials.authToken),
  ];
}

function listEnvelopeFields(
  marketId: string,
  credentials: Credentials,
  frontierTime: number,
): readonly EnvelopeField[] {
  return [
    { name: "marketId", value: marketId },
    { name: "cmdrId", value: credentials.commanderId },
    ...credentialFields(credentials, frontierTime),
  ];
}

function starsystemEnvelopeFields(
  systemAddress: number,
  language: string,
  cachedTimeStamp: number,
  credentials: Credentials,
  frontierTime: number,
): readonly EnvelopeField[] {
  return [
    { name: "cmdrId", value: credentials.commanderId },
    { name: "language", value: language },
    { name: "systemAddr", value: systemAddress },
    { name: "cachedTimeStamp", value: cachedTimeStamp },
    ...credentialFields(credentials, frontierTime),
  ];
}

function tradeEnvelopeFields(
  plan: TradePlan,
  credentials: Credentials,
  frontierTime: number,
): readonly EnvelopeField[] {
  return [
    { name: "cmdrId", value: credentials.commanderId },
    { name: "marketId", value: plan.marketId },
    { name: "transactionType", value: plan.transactionType },
    { name: "commodityId", value: plan.commodityId },
    { name: "blackMarket", value: plan.blackMarket ? 1 : 0 },
    { name: "stolen", value: plan.stolen ? 1 : 0 },
    { name: "unitPrice", value: plan.unitPrice },
    { name: "qty", value: plan.qty },
    { name: "finalQty", value: plan.finalQty },
    ...credentialFields(credentials, frontierTime),
  ];
}

function encryptEnvelope(plaintext: string, nonceText: string): string {
  const nonce = encoder.encode(nonceText); // The 12 ASCII hex characters themselves are the nonce.
  const ciphertext = chacha20(encoder.encode(plaintext), CHACHA_KEY, nonce);
  return Buffer.from(ciphertext).toString("base64");
}

function decompressLz4Block(input: Uint8Array, expectedSize: number): Uint8Array {
  const output = new Uint8Array(expectedSize);
  let source = 0;
  let destination = 0;

  const readExtendedLength = (initial: number): number => {
    let length = initial;
    if (initial === 15) {
      let extension: number;
      do {
        if (source >= input.length) throw new Error("Truncated LZ4 length");
        extension = input[source++]!;
        length += extension;
      } while (extension === 255);
    }
    return length;
  };

  while (source < input.length) {
    const token = input[source++]!;
    const literalLength = readExtendedLength(token >>> 4);
    if (source + literalLength > input.length || destination + literalLength > output.length) {
      throw new Error("Invalid LZ4 literal length");
    }
    output.set(input.subarray(source, source + literalLength), destination);
    source += literalLength;
    destination += literalLength;

    if (source === input.length) break;
    if (source + 2 > input.length) throw new Error("Truncated LZ4 match offset");

    const offset = input[source]! | (input[source + 1]! << 8);
    source += 2;
    if (offset === 0 || offset > destination) throw new Error("Invalid LZ4 match offset");

    const matchLength = readExtendedLength(token & 0x0f) + 4;
    if (destination + matchLength > output.length) throw new Error("Invalid LZ4 match length");
    for (let index = 0; index < matchLength; index++) {
      output[destination] = output[destination - offset]!;
      destination++;
    }
  }

  if (destination !== expectedSize) {
    throw new Error(`LZ4 size mismatch: expected ${expectedSize}, produced ${destination}`);
  }
  return output;
}

function decryptResponse(encodedBody: string, nonceText: string, expectedSize: number): string {
  const compact = encodedBody.trim();
  if (!/^[A-Za-z0-9+/]*={0,2}$/.test(compact) || compact.length % 4 !== 0) {
    throw new Error("Response is not valid standard Base64");
  }

  const ciphertext = new Uint8Array(Buffer.from(compact, "base64"));
  const decrypted = chacha20(ciphertext, CHACHA_KEY, encoder.encode(nonceText));
  if (decrypted.length < 8 || decrypted[0] !== 0x45 || decrypted[1] !== 0x44 ||
      decrypted[2] !== 0x44 || decrypted[3] !== 0x45) {
    throw new Error("Decrypted response lacks the EDDE compression header");
  }

  const plaintext = decompressLz4Block(decrypted.subarray(8), expectedSize);
  return new TextDecoder("utf-8", { fatal: true }).decode(plaintext);
}

// ---------------------------------------------------------------------------
// Terminal rendering
// ---------------------------------------------------------------------------

type CellAlignment = "left" | "right";

interface TableColumn {
  readonly key: string;
  readonly header: string;
  readonly align?: CellAlignment;
  /** Columns with a higher priority are dropped first when the table is too wide. */
  readonly priority?: number;
  readonly minWidth?: number;
  readonly maxWidth?: number;
}

type TableRow =
  | { readonly kind: "data"; readonly cells: Readonly<Record<string, string>> }
  | { readonly kind: "band"; readonly text: string }
  | { readonly kind: "rule" };

const TRUNCATION_MARK = "~";

const TERMINAL_WIDTH = ((): number => {
  const override = process.env.COLUMNS?.trim();
  if (override && /^\d+$/.test(override)) return Math.max(48, Number(override));
  const columns = process.stdout.columns;
  return Math.max(48, Number.isInteger(columns) && columns > 0 ? columns : 100);
})();

function clampText(text: string, width: number): string {
  if (width <= 0) return "";
  if (text.length <= width) return text;
  if (width === 1) return TRUNCATION_MARK;
  return `${text.slice(0, width - 1)}${TRUNCATION_MARK}`;
}

function padCell(text: string, width: number, align: CellAlignment): string {
  const clamped = clampText(text, width);
  const padding = " ".repeat(width - clamped.length);
  return align === "right" ? `${padding}${clamped}` : `${clamped}${padding}`;
}

function measureColumns(columns: readonly TableColumn[], rows: readonly TableRow[]): number[] {
  return columns.map((column) => {
    let width = column.header.length;
    for (const row of rows) {
      if (row.kind !== "data") continue;
      width = Math.max(width, (row.cells[column.key] ?? "").length);
    }
    if (column.maxWidth !== undefined) width = Math.min(width, column.maxWidth);
    return Math.max(width, column.minWidth ?? 1);
  });
}

/** Outer width of the frame: each column costs its width plus "| " and " ". */
function frameWidth(widths: readonly number[]): number {
  return widths.reduce((total, width) => total + width + 3, 1);
}

interface RenderedTable {
  readonly lines: readonly string[];
  readonly omitted: readonly string[];
}

function renderTable(
  columns: readonly TableColumn[],
  rows: readonly TableRow[],
  available: number = TERMINAL_WIDTH,
): RenderedTable {
  let active = [...columns];
  let widths = measureColumns(active, rows);
  const omitted: string[] = [];

  while (frameWidth(widths) > available) {
    // Prefer squeezing the widest shrinkable column before losing a column entirely.
    const shrinkable = active
      .map((column, index) => ({ column, index, slack: widths[index]! - (column.minWidth ?? widths[index]!) }))
      .filter((candidate) => candidate.slack > 0)
      .sort((left, right) => right.slack - left.slack)[0];
    if (shrinkable) {
      const excess = frameWidth(widths) - available;
      widths[shrinkable.index] = widths[shrinkable.index]! - Math.min(excess, shrinkable.slack);
      continue;
    }

    const droppable = active.filter((column) => (column.priority ?? 0) > 0);
    if (droppable.length === 0) break;
    const victim = droppable.reduce((worst, column) =>
      (column.priority ?? 0) > (worst.priority ?? 0) ? column : worst,
    );
    omitted.push(victim.header);
    active = active.filter((column) => column !== victim);
    widths = measureColumns(active, rows);
  }

  const dashRule = `+${widths.map((width) => "-".repeat(width + 2)).join("+")}+`;
  const headerRule = `+${widths.map((width) => "=".repeat(width + 2)).join("+")}+`;
  const bandWidth = Math.max(1, frameWidth(widths) - 4);

  const lines: string[] = [dashRule];
  lines.push(`| ${active.map((column, index) => padCell(column.header, widths[index]!, column.align ?? "left")).join(" | ")} |`);
  lines.push(headerRule);
  let previousWasRule = true;

  for (const row of rows) {
    if (row.kind === "rule") {
      if (!previousWasRule) lines.push(dashRule);
      previousWasRule = true;
      continue;
    }
    if (row.kind === "band") {
      if (!previousWasRule) lines.push(dashRule);
      lines.push(`| ${padCell(row.text, bandWidth, "left")} |`);
      lines.push(dashRule);
      previousWasRule = true;
      continue;
    }
    lines.push(
      `| ${active
        .map((column, index) => padCell(row.cells[column.key] ?? "", widths[index]!, column.align ?? "left"))
        .join(" | ")} |`,
    );
    previousWasRule = false;
  }

  if (!previousWasRule) lines.push(dashRule);
  return { lines, omitted };
}

function heading(title: string): string {
  const label = `== ${title} `;
  return label.length >= TERMINAL_WIDTH ? label : label.padEnd(TERMINAL_WIDTH, "=");
}

/** Word-wrapped, indented commentary that never pushes the terminal into a horizontal scroll. */
function emitNote(text: string): void {
  const indent = "   ";
  const limit = Math.max(20, TERMINAL_WIDTH - indent.length);
  let line = "";
  for (const word of text.split(" ")) {
    if (line === "") line = word;
    else if (line.length + 1 + word.length <= limit) line += ` ${word}`;
    else {
      console.log(`${indent}${line}`);
      line = word;
    }
  }
  if (line !== "") console.log(`${indent}${line}`);
}

function emitTable(title: string, columns: readonly TableColumn[], rows: readonly TableRow[]): void {
  console.log(heading(title));
  const { lines, omitted } = renderTable(columns, rows);
  for (const line of lines) console.log(line);
  if (omitted.length > 0) {
    emitNote(`columns hidden to fit ${TERMINAL_WIDTH} cols: ${omitted.join(", ")}`);
  }
}

const FIELD_COLUMNS: readonly TableColumn[] = [
  { key: "field", header: "Field", minWidth: 8, maxWidth: 22 },
  { key: "value", header: "Value", minWidth: 12 },
];

function bandRow(text: string): TableRow {
  return { kind: "band", text };
}

function fieldRow(field: string, value: string | number): TableRow {
  return { kind: "data", cells: { field, value: String(value) } };
}

function headerRows(headers: Headers): TableRow[] {
  const entries: [string, string][] = [];
  headers.forEach((value, name) => entries.push([name, value]));
  return entries
    .sort(([left], [right]) => left.localeCompare(right))
    .map(([name, value]) => fieldRow(name, value));
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

function formatInteger(value: number): string {
  return Number.isFinite(value) ? Math.trunc(value).toLocaleString("en-US") : "?";
}

/** Zeroes dominate a market table, so they render as a placeholder instead of digits. */
function formatQuantity(value: number): string {
  return value === 0 ? "-" : formatInteger(value);
}

function formatBracketMeter(level: number): string {
  const filled = Math.max(0, Math.min(3, Math.trunc(level)));
  return `${"#".repeat(filled)}${".".repeat(3 - filled)}`;
}

function formatFlag(enabled: boolean, symbol: string): string {
  return enabled ? symbol : ".";
}

function formatUnixSeconds(seconds: number): string {
  const date = new Date(seconds * 1_000);
  return Number.isFinite(date.getTime()) ? `${seconds} (${date.toISOString()})` : String(seconds);
}

function formatMilliseconds(milliseconds: number): string {
  const totalSeconds = Math.floor(milliseconds / 1_000);
  const hours = Math.floor(totalSeconds / 3_600);
  const minutes = Math.floor((totalSeconds % 3_600) / 60);
  const seconds = totalSeconds % 60;
  const clock = `${String(hours).padStart(2, "0")}:${String(minutes).padStart(2, "0")}:${String(seconds).padStart(2, "0")}`;
  return `${formatInteger(milliseconds)} ms (uptime ${clock})`;
}

function elide(text: string, head: number, tail: number): string {
  return text.length <= head + tail + 3 ? text : `${text.slice(0, head)}...${text.slice(-tail)}`;
}

// ---------------------------------------------------------------------------
// Market payload rendering
// ---------------------------------------------------------------------------

function asRecord(value: unknown): Record<string, unknown> | null {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function readNumber(source: Record<string, unknown>, key: string): number {
  const value = source[key];
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function readString(source: Record<string, unknown>, key: string): string {
  const value = source[key];
  return typeof value === "string" ? value : "";
}

function readBoolean(source: Record<string, unknown>, key: string): boolean {
  return source[key] === true || source[key] === 1;
}

interface Commodity {
  readonly id: number;
  readonly name: string;
  readonly category: string;
  readonly stock: number;
  readonly stockBracket: number;
  readonly buyPrice: number;
  readonly sellPrice: number;
  readonly fencePrice: number;
  readonly demand: number;
  readonly demandBracket: number;
  readonly meanPrice: number;
  readonly consumer: boolean;
  readonly producer: boolean;
  readonly rare: boolean;
  readonly illegal: boolean;
}

function toCommodity(key: string, source: Record<string, unknown>): Commodity {
  const legality = readString(source, "legality");
  return {
    id: readNumber(source, "id") || Number(key) || 0,
    name: readString(source, "name") || key,
    category: readString(source, "categoryname").trim() || "Uncategorised",
    stock: readNumber(source, "stock"),
    stockBracket: readNumber(source, "stockBracket"),
    buyPrice: readNumber(source, "buyPrice"),
    sellPrice: readNumber(source, "sellPrice"),
    fencePrice: readNumber(source, "fencePrice"),
    demand: readNumber(source, "demand"),
    demandBracket: readNumber(source, "demandBracket"),
    meanPrice: readNumber(source, "meanPrice"),
    consumer: readNumber(source, "consumer") > 0,
    producer: readNumber(source, "producer") > 0,
    rare: readNumber(source, "rare") > 0,
    illegal: legality !== "",
  };
}

const COMMODITY_COLUMNS: readonly TableColumn[] = [
  { key: "id", header: "ID", align: "right", priority: 4 },
  { key: "name", header: "Commodity", minWidth: 12, maxWidth: 30 },
  { key: "stock", header: "Stock", align: "right", priority: 1 },
  { key: "stockMeter", header: "Stk", priority: 2 },
  { key: "buyPrice", header: "Buy", align: "right" },
  { key: "sellPrice", header: "Sell", align: "right" },
  { key: "fencePrice", header: "Fence", align: "right", priority: 4 },
  { key: "demand", header: "Demand", align: "right", priority: 1 },
  { key: "demandMeter", header: "Dmd", priority: 2 },
  { key: "meanPrice", header: "Mean", align: "right", priority: 3 },
  { key: "flags", header: "CPRI", priority: 1 },
];

function commodityRow(commodity: Commodity): TableRow {
  return {
    kind: "data",
    cells: {
      id: String(commodity.id),
      name: commodity.name,
      stock: formatQuantity(commodity.stock),
      stockMeter: formatBracketMeter(commodity.stockBracket),
      buyPrice: formatQuantity(commodity.buyPrice),
      sellPrice: formatQuantity(commodity.sellPrice),
      fencePrice: formatQuantity(commodity.fencePrice),
      demand: formatQuantity(commodity.demand),
      demandMeter: formatBracketMeter(commodity.demandBracket),
      meanPrice: formatQuantity(commodity.meanPrice),
      flags:
        formatFlag(commodity.consumer, "C") +
        formatFlag(commodity.producer, "P") +
        formatFlag(commodity.rare, "R") +
        formatFlag(commodity.illegal, "I"),
    },
  };
}

function emitCommodityTable(commodities: readonly Commodity[]): void {
  const categories = new Map<string, Commodity[]>();
  for (const commodity of commodities) {
    const bucket = categories.get(commodity.category);
    if (bucket) bucket.push(commodity);
    else categories.set(commodity.category, [commodity]);
  }

  const rows: TableRow[] = [];
  for (const name of [...categories.keys()].sort((left, right) => left.localeCompare(right))) {
    const bucket = categories.get(name)!.sort((left, right) => left.name.localeCompare(right.name));
    const supplied = bucket.filter((commodity) => commodity.stock > 0).length;
    const wanted = bucket.filter((commodity) => commodity.demand > 0).length;
    rows.push(bandRow(`${name.toUpperCase()}  ${bucket.length} items | ${supplied} supplied | ${wanted} in demand`));
    for (const commodity of bucket) rows.push(commodityRow(commodity));
  }

  emitTable(`COMMODITIES  ${commodities.length} entries in ${categories.size} categories`, COMMODITY_COLUMNS, rows);
  emitNote(
    "legend: '-' zero | CPRI = Consumer/Producer/Rare/Illegal | Stk,Dmd meters '###' bracket 3 .. '...' bracket 0 | '~' truncated",
  );
}

const INVENTORY_COLUMNS: readonly TableColumn[] = [
  { key: "commodity", header: "Commodity", minWidth: 10, maxWidth: 30 },
  { key: "qty", header: "Qty", align: "right" },
  { key: "value", header: "Value", align: "right" },
  { key: "stolen", header: "S" },
  { key: "marked", header: "Marked", align: "right", priority: 3 },
  { key: "owner", header: "Owner", align: "right", priority: 2 },
  { key: "origin", header: "Origin", align: "right", priority: 2 },
  { key: "position", header: "Position (x / y / z)", priority: 1 },
];

function emitInventoryTable(inventory: readonly unknown[]): void {
  if (inventory.length === 0) {
    console.log(heading("INVENTORY  empty"));
    return;
  }

  const rows: TableRow[] = inventory.map((entry) => {
    const item = asRecord(entry) ?? {};
    const position = asRecord(item.xyz);
    const coordinates = position
      ? [readNumber(position, "x"), readNumber(position, "y"), readNumber(position, "z")]
          .map((value) => value.toFixed(1))
          .join(" / ")
      : "-";
    return {
      kind: "data" as const,
      cells: {
        commodity: readString(item, "commodity") || "?",
        qty: formatQuantity(readNumber(item, "qty")),
        value: formatQuantity(readNumber(item, "value")),
        stolen: formatFlag(readBoolean(item, "stolen"), "S"),
        marked: formatQuantity(readNumber(item, "marked")),
        owner: String(readNumber(item, "owner")),
        origin: String(readNumber(item, "origin")),
        position: coordinates,
      },
    };
  });

  emitTable(`INVENTORY  ${inventory.length} items`, INVENTORY_COLUMNS, rows);
}

function emitMarketSummary(snapshot: MarketSnapshot, title: string): void {
  const { commodities, inventory, payload } = snapshot;
  const count = (predicate: (commodity: Commodity) => boolean): number => commodities.filter(predicate).length;
  const categories = new Set(commodities.map((commodity) => commodity.category));
  const rows: TableRow[] = [];

  // Only the trade endpoint returns the balance and a modification timestamp.
  if ("credits" in payload) rows.push(fieldRow("credits", `${formatInteger(readNumber(payload, "credits"))} cr`));
  if ("debt" in payload) rows.push(fieldRow("debt", `${formatInteger(readNumber(payload, "debt"))} cr`));
  const lastModified = asRecord(payload.lastModified);
  if (lastModified) rows.push(fieldRow("lastModified", formatUnixSeconds(readNumber(lastModified, "sec"))));

  rows.push(
    fieldRow("commodities", `${commodities.length} in ${categories.size} categories`),
    fieldRow("supplied (stock > 0)", count((commodity) => commodity.stock > 0)),
    fieldRow("in demand", count((commodity) => commodity.demand > 0)),
    fieldRow("consumers / producers", `${count((c) => c.consumer)} / ${count((c) => c.producer)}`),
    fieldRow("rare / illegal", `${count((c) => c.rare)} / ${count((c) => c.illegal)}`),
  );
  if ("allowsDumping" in payload) {
    rows.push(fieldRow("allowsDumping", readBoolean(payload, "allowsDumping") ? "yes" : "no"));
  }
  rows.push(fieldRow("inventory items", inventory.length));

  emitTable(title, FIELD_COLUMNS, rows);
}

interface MarketSnapshot {
  readonly payload: Record<string, unknown>;
  readonly commodities: readonly Commodity[];
  readonly inventory: readonly unknown[];
}

/** Returns null when the payload is not a market listing, so the caller can fall back to raw output. */
function parseMarketSnapshot(decrypted: string): MarketSnapshot | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(decrypted);
  } catch {
    return null;
  }

  const payload = asRecord(parsed);
  const rawCommodities = payload && asRecord(payload.commodities);
  if (!payload || !rawCommodities) return null;

  const commodities = Object.entries(rawCommodities)
    .map(([key, value]) => {
      const record = asRecord(value);
      return record ? toCommodity(key, record) : null;
    })
    .filter((commodity): commodity is Commodity => commodity !== null);
  if (commodities.length === 0) return null;

  return { payload, commodities, inventory: Array.isArray(payload.inventory) ? payload.inventory : [] };
}

function emitMarketSnapshot(snapshot: MarketSnapshot, title = "MARKET SUMMARY"): void {
  emitMarketSummary(snapshot, title);
  emitInventoryTable(snapshot.inventory);
  emitCommodityTable(snapshot.commodities);
}


// ---------------------------------------------------------------------------
// Command line parsing
// ---------------------------------------------------------------------------

interface ParsedArguments {
  readonly command: string;
  readonly flags: ReadonlyMap<string, string | boolean>;
  /** Bare words after the command, e.g. the system name for `markets`. */
  readonly positionals: readonly string[];
}

/** `--market-id`, `--marketId` and `--MARKET_ID` all normalise to the same key. */
function normalizeFlag(name: string): string {
  return name.replace(/[-_]/g, "").toLowerCase();
}

const FLAG_ALIASES: Readonly<Record<string, string>> = {
  commanderid: "cmdrid",
  cmdr: "cmdrid",
  market: "marketid",
  transactiontype: "type",
  commodity: "item",
  commodityid: "item",
  items: "item",
  commodities: "item",
  quantity: "qty",
  price: "unitprice",
  capacity: "cargo",
  rate: "concurrency",
  workers: "concurrency",
  jobs: "concurrency",
  parallel: "concurrency",
  systemaddr: "address",
  systemaddress: "address",
  id64: "address",
  lang: "language",
  hold: "cargo",
  retry: "watch",
  loop: "watch",
  every: "interval",
  rounds: "attempts",
};

/** Flags that consume a value; anything else is a boolean switch. */
const VALUE_FLAGS: ReadonlySet<string> = new Set([
  "marketid",
  "cmdrid",
  "machineid",
  "machinetoken",
  "authtoken",
  "nonce",
  "ftime",
  "requesttime",
  "fdevsemver",
  "fdevseason",
  "useragent",
  "method",
  "type",
  "item",
  "qty",
  "finalqty",
  "unitprice",
  "cargo",
  "interval",
  "attempts",
  "credits",
  "system",
  "station",
  "address",
  "language",
  "cachedtimestamp",
  "dump",
  "uploader",
  "gameversion",
  "gamebuild",
  "softwarename",
  "softwareversion",
  "stationtype",
  "concurrency",
  "timeout",
  "requeue",
]);

const BOOLEAN_FLAGS: ReadonlySet<string> = new Set([
  "dryrun",
  "fullurl",
  "json",
  "blackmarket",
  "stolen",
  "resolve",
  "cap",
  "fullmarket",
  "fill",
  "watch",
  "carriers",
  "trading",
  "eddn",
  "eddntest",
  "horizons",
  "odyssey",
  "detail",
  "allmarkets",
  "help",
]);

const BOOLEAN_LITERALS: Readonly<Record<string, boolean>> = {
  "1": true, "0": false, true: true, false: false, yes: true, no: false, on: true, off: false,
};

function parseArguments(argv: readonly string[]): ParsedArguments {
  const flags = new Map<string, string | boolean>();
  const positionals: string[] = [];
  let command = "";

  for (let index = 0; index < argv.length; index++) {
    const token = argv[index]!;

    if (token === "-h") {
      flags.set("help", true);
      continue;
    }
    if (!token.startsWith("--")) {
      if (command === "") command = token.toLowerCase();
      else positionals.push(token);
      continue;
    }

    const body = token.slice(2);
    const equals = body.indexOf("=");
    const rawName = equals >= 0 ? body.slice(0, equals) : body;
    const negated = /^no-/i.test(rawName);
    const canonical = ((name) => FLAG_ALIASES[name] ?? name)(normalizeFlag(negated ? rawName.slice(3) : rawName));

    if (negated) {
      if (!BOOLEAN_FLAGS.has(canonical)) throw new Error(`--no- may only negate a switch, not --${rawName}`);
      flags.set(canonical, false);
      continue;
    }

    if (VALUE_FLAGS.has(canonical)) {
      if (equals >= 0) {
        flags.set(canonical, body.slice(equals + 1));
        continue;
      }
      const value = argv[index + 1];
      if (value === undefined || value.startsWith("--")) throw new Error(`--${rawName} requires a value`);
      flags.set(canonical, value);
      index++;
      continue;
    }

    if (!BOOLEAN_FLAGS.has(canonical)) throw new Error(`Unknown option --${rawName}`);
    if (equals >= 0) {
      const literal = BOOLEAN_LITERALS[body.slice(equals + 1).toLowerCase()];
      if (literal === undefined) throw new Error(`--${rawName} expects true or false`);
      flags.set(canonical, literal);
      continue;
    }
    // A bare switch may still be followed by an explicit boolean literal.
    const next = argv[index + 1];
    const literal = next === undefined ? undefined : BOOLEAN_LITERALS[next.toLowerCase()];
    if (literal !== undefined) {
      flags.set(canonical, literal);
      index++;
    } else {
      flags.set(canonical, true);
    }
  }

  return { command: command || "market", flags, positionals };
}

/** Canonical keys are collapsed ("unitprice"); messages should show the documented spelling. */
const FLAG_DISPLAY: Readonly<Record<string, string>> = {
  marketid: "market-id", cmdrid: "cmdr-id", machineid: "machine-id", machinetoken: "machine-token",
  authtoken: "auth-token", ftime: "f-time", requesttime: "request-time", fdevsemver: "fdev-semver",
  fdevseason: "fdev-season", useragent: "user-agent", unitprice: "unit-price", finalqty: "final-qty",
  dryrun: "dry-run",
  cachedtimestamp: "cached-timestamp",
  eddntest: "eddn-test",
  allmarkets: "all-markets",
  stationtype: "station-type",
  gameversion: "game-version",
  gamebuild: "game-build",
  softwarename: "software-name",
  softwareversion: "software-version", fullurl: "full-url", blackmarket: "black-market", fullmarket: "full-market",
};

function flagName(flag: string): string {
  return `--${FLAG_DISPLAY[flag] ?? flag}`;
}

function optionalValue(args: ParsedArguments, flag: string, environment?: string): string | undefined {
  const value = args.flags.get(flag);
  if (typeof value === "string") {
    const trimmed = value.trim();
    if (trimmed !== "") return trimmed;
  }
  if (typeof value === "boolean") throw new Error(`${flagName(flag)} requires a value`);
  return environment ? process.env[environment]?.trim() || undefined : undefined;
}

function requireValue(args: ParsedArguments, flag: string, environment?: string): string {
  const value = optionalValue(args, flag, environment);
  if (!value) {
    throw new Error(
      environment
        ? `Missing ${flagName(flag)} (or ${environment} in the environment)`
        : `Missing required option ${flagName(flag)}`,
    );
  }
  return value;
}

function optionalNumber(args: ParsedArguments, flag: string): number | undefined {
  const value = optionalValue(args, flag);
  return value === undefined ? undefined : parseUnsignedInteger(flagName(flag), value);
}

/** For values like --interval that may be fractional. */
function optionalDecimal(args: ParsedArguments, flag: string): number | undefined {
  const raw = optionalValue(args, flag);
  if (raw === undefined) return undefined;
  const value = Number(raw);
  if (!Number.isFinite(value) || value <= 0) throw new Error(`${flagName(flag)} must be a positive number`);
  return value;
}

function optionalSwitch(args: ParsedArguments, flag: string): boolean | undefined {
  const value = args.flags.get(flag);
  if (value === undefined) return undefined;
  if (typeof value === "boolean") return value;
  const literal = BOOLEAN_LITERALS[value.toLowerCase()];
  if (literal === undefined) throw new Error(`${flagName(flag)} expects true or false`);
  return literal;
}

function switchValue(args: ParsedArguments, flag: string, fallback: boolean): boolean {
  return optionalSwitch(args, flag) ?? fallback;
}

const USAGE = `game-internal-api.ts — Elite Dangerous game-internal API client

Usage
  bun game-internal-api.ts [command] [options]

Commands
  market [name]            ${MARKET_LIST.method} ${MARKET_LIST_PATH} — one market's commodity listing, or every
                           market in a system when given a system name (default command)
  list                     alias for market
  trade                    ${MARKET_TRADE.method} ${MARKET_TRADE_PATH} — buy or sell one commodity
  markets <name>           ${STARSYSTEM.method} ${STARSYSTEM.path} — resolve a system or station name
                           through Ardent and list the market ids in that system
  help                     this text

Credentials (option, else environment)
  --cmdr-id       COMMANDER_ID    --machine-id     MACHINE_ID
  --machine-token MACHINE_TOKEN   --auth-token     AUTH_TOKEN   (80 / 2024 chars)

Shared options
  --market-id <id>         market to talk to (else MARKET_ID)
  --nonce <hex12>          fixed 12-hex nonce instead of a fresh random one per request
  --f-time <unix>          override fTime          --request-time <ms>  override Request-Time
  --fdev-semver <v>        default 4.4.0.3         --fdev-season <n>    default 4
  --user-agent <ua>        default EDGame/11.0/Win64
  --method <verb>          override the verb (list uses ${MARKET_LIST.method}, trade uses ${MARKET_TRADE.method})
  --dry-run                resolve and show the request without sending it. For trade the
                           read-only price lookup still runs; add --no-resolve to stay offline
  --full-url               print the encrypted query in full
  --json                   emit JSON instead of tables (for piping)

trade options
  --type buy|sell          required
  --item <id|name>[,...]   one commodity, or a comma-separated list worked in the order given
  --qty <n>                units per commodity (required unless --fill)
  --cargo <n>              hold capacity; buys are clamped to the space left
  --fill                   buy until the hold is full, spending down the --item list in order
  --watch                  repeat until the hold is full (needs --fill or --attempts)
  --interval <seconds>     delay between rounds, default 1
  --attempts <n>           stop after n rounds; 0 (default) means only --fill stops the loop
  --credits <n>            starting balance, so the first buy can be sized to it; otherwise
                           the balance is only known after the first trade replies
  --unit-price <n>         price per unit; taken from the market when omitted
  --final-qty <n>          defaults to --qty (single trades only)
  --black-market           force the black-market flag (default: on for stolen or illegal goods)
  --stolen                 mark the goods as stolen (default off)
  --no-resolve             never prefetch ${MARKET_LIST_PATH}; requires numeric --item and --unit-price
  --no-cap                 send --qty verbatim instead of clamping it to stock / holdings
  --full-market            also print the whole commodity table from the trade response

market options
  [name]                   system name: sweeps every trading market in it
  --market-id <id>         a single market instead (else MARKET_ID)
  --concurrency <n>        parallel workers for a sweep, default ${DEFAULT_CONCURRENCY}, max ${MAX_CONCURRENCY}
  --timeout <seconds>      per-attempt timeout, default ${DEFAULT_TIMEOUT_SECONDS}
  --requeue <n>            requeue a timed-out or transient failure this many times,
                           default ${DEFAULT_REQUEUES} (EDDN posts are never retried in-run)
  --detail                 print the full commodity table for every market in a sweep
  --all-markets            include markets with nothing listed as imported or exported
  --carriers               include fleet carriers
  --eddn                   publish each market to EDDN (${EDDN_UPLOAD_URL})
  --eddn-test              same, but against the /test schema, which is not relayed onward
  --uploader <name>        EDDN uploaderID; defaults to the commander id
  --game-version <v>       default ${EDDN_GAME_VERSION}    --game-build <v>  default empty
  --software-name <n>      default ${EDDN_SOFTWARE_NAME}   --software-version <v>  default ${EDDN_SOFTWARE_VERSION}
  --horizons / --odyssey   set only if you know them; omitted entirely otherwise
  --system <name> --station <name> --station-type <t>
                           name a single --market-id for EDDN when Ardent cannot

markets options
  <name>                   system or station name; quote anything with spaces
  --system <name>          treat the name as a system only
  --station <name>         treat the name as a station and use its system
  --address <id64>         skip Ardent and use this systemAddress
  --language <code>        default en          --cached-timestamp <n>  default 0
  --carriers               include fleet carriers (hidden by default; there are often hundreds)
  --trading                only markets that actually buy or sell commodities
  --dump <file>            write the decoded starsystem payload for inspection

Examples
  bun game-internal-api.ts market --market-id 4306502403
  bun game-internal-api.ts market Colonia --eddn
  bun game-internal-api.ts market --market-id 128667761 --eddn-test
  bun game-internal-api.ts markets "Hyades Sector NI-X a16-0"
  bun game-internal-api.ts markets --station "Jaques Station"
  bun game-internal-api.ts list --market-id 4306502403
  bun game-internal-api.ts trade --market-id 4306502403 --type buy --item silver --qty 10
  bun game-internal-api.ts trade --type sell --item 128049155 --qty 5 --unit-price 3340 --stolen
  bun game-internal-api.ts trade --type buy --item palladium,gold --cargo 1232 --fill
  bun game-internal-api.ts trade --type buy --item palladium,gold --cargo 1232 --fill --watch`;

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

interface Session {
  readonly args: ParsedArguments;
  readonly credentials: Credentials;
  /** Set by --method; otherwise each endpoint uses its own verb. */
  readonly methodOverride: string | undefined;
  readonly dryRun: boolean;
  readonly fullUrl: boolean;
  readonly json: boolean;
}

function openSession(args: ParsedArguments): Session {
  return {
    args,
    credentials: loadCredentials(args),
    methodOverride: optionalValue(args, "method")?.toUpperCase(),
    dryRun: switchValue(args, "dryrun", false),
    fullUrl: switchValue(args, "fullurl", false),
    json: switchValue(args, "json", false),
  };
}

interface PreparedRequest {
  readonly path: string;
  readonly method: string;
  readonly url: string;
  readonly headers: Headers;
  readonly stamp: RequestStamp;
  readonly plaintext: string;
  readonly fields: readonly EnvelopeField[];
}

function prepareRequest(
  session: Session,
  endpoint: Endpoint,
  fields: readonly EnvelopeField[],
  stamp: RequestStamp,
): PreparedRequest {
  const { path } = endpoint;
  const plaintext = serializeEnvelope(fields);
  const url = `${API_ORIGIN}${path}?${encryptEnvelope(plaintext, stamp.nonce)}`;
  const headers = new Headers({
    "Request-Time": String(stamp.requestTime),
    "Fdev-Retry": "0/2",
    "Fdev-Semver": optionalValue(session.args, "fdevsemver", "FDEV_SEMVER") ?? "4.4.0.3",
    "User-Agent": optionalValue(session.args, "useragent", "USER_AGENT") ?? "EDGame/11.0/Win64",
    "Fdev-Season": optionalValue(session.args, "fdevseason", "FDEV_SEASON") ?? "4",
    Encrypted: "1",
    Nonce: stamp.nonce,
  });
  return { path, method: session.methodOverride ?? endpoint.method, url, headers, stamp, plaintext, fields };
}

function emitRequest(request: PreparedRequest, fullUrl: boolean): void {
  const query = request.url.slice(request.url.indexOf("?") + 1);

  emitTable(`REQUEST  ${request.method} ${request.path}`, FIELD_COLUMNS, [
    bandRow("TARGET"),
    fieldRow("method", request.method),
    fieldRow("endpoint", `${API_ORIGIN}${request.path}`),
    fieldRow("query", `${formatInteger(query.length)} chars base64 ${elide(query, 20, 12)}`),
    bandRow("HEADERS"),
    ...headerRows(request.headers),
    bandRow("ENVELOPE"),
    ...request.fields.map((field) => fieldRow(field.name, field.display ?? String(field.value))),
    fieldRow("plaintext", `${formatInteger(encoder.encode(request.plaintext).length)} bytes`),
    fieldRow("nonce", request.stamp.nonce),
    fieldRow("fTime", formatUnixSeconds(request.stamp.frontierTime)),
    fieldRow("requestTime", formatMilliseconds(request.stamp.requestTime)),
  ]);

  if (fullUrl) {
    console.log(heading("REQUEST URL"));
    console.log(request.url);
  } else {
    emitNote("pass --full-url to print the encrypted query in full");
  }
}

function emitResponse(response: Response): void {
  emitTable(`RESPONSE  HTTP ${response.status} ${response.statusText}`, FIELD_COLUMNS, [
    bandRow("HEADERS"),
    ...headerRows(response.headers),
  ]);
}

interface Exchange {
  readonly response: Response;
  /** Decrypted body, or null when the response could not be decoded. */
  readonly decrypted: string | null;
  readonly raw: string;
}

interface SendOptions {
  /** Suppress the request and response tables — used for the trade command's price lookup. */
  readonly quiet?: boolean;
  /** Send even under --dry-run; only safe for read-only lookups. */
  readonly ignoreDryRun?: boolean;
  /** Aborting this closes the socket, so a hung request cannot pin a worker. */
  readonly signal?: AbortSignal;
}

async function send(session: Session, request: PreparedRequest, options: SendOptions = {}): Promise<Exchange | null> {
  const quiet = options.quiet ?? false;
  if (!quiet && !session.json) emitRequest(request, session.fullUrl);
  if (session.dryRun && !options.ignoreDryRun) return null;

  const response = await fetch(request.url, {
    method: request.method,
    headers: request.headers,
    // A body-bearing verb needs an explicit empty body so Content-Length: 0 is sent.
    body: request.method === "GET" || request.method === "HEAD" ? undefined : "",
    redirect: "manual",
    signal: options.signal,
  });
  const raw = await response.text();
  if (!quiet && !session.json) emitResponse(response);

  if (!response.ok) {
    process.exitCode = 1;
    // Headers carry the diagnosis (Allow, nonce), so show them even in a quiet batch round.
    if (quiet && !session.json) emitResponse(response);
    const allowed = response.headers.get("allow");
    console.error(`${request.method} ${request.path} failed: HTTP ${response.status} ${response.statusText}`);
    if (response.status === 405 && allowed) {
      const verbs = allowed.split(",").map((verb) => verb.trim().toUpperCase()).filter((verb) => verb !== "");
      console.error(
        verbs.includes(request.method)
          ? `The server reports it accepts ${allowed}, so the verb is not what it rejected`
          : `This endpoint accepts ${allowed} — retry with --method ${verbs[0]}`,
      );
    }

    // Failure bodies are encrypted too, so decode what we can rather than dumping base64.
    const failureNonce = response.headers.get("nonce");
    const decoded = failureNonce ? decodeOpaqueBody(raw, failureNonce, response.headers.get("uncompressedsize")) : null;
    if (decoded !== null) {
      console.log(heading("ERROR PAYLOAD"));
      console.log(decoded);
    } else if (raw.trim() !== "") {
      console.log(raw);
    }
    return { response, decrypted: null, raw };
  }

  const responseNonce = response.headers.get("nonce");
  if (!responseNonce || !/^[0-9a-fA-F]{12}$/.test(responseNonce)) {
    console.error(`Missing or invalid response Nonce header: ${JSON.stringify(responseNonce)}`);
    console.log(raw);
    process.exitCode = 1;
    return { response, decrypted: null, raw };
  }

  const uncompressedSize = Number(response.headers.get("uncompressedsize"));
  if (!Number.isSafeInteger(uncompressedSize) || uncompressedSize <= 0) {
    console.error(`Missing or invalid uncompressedSize header: ${response.headers.get("uncompressedsize")}`);
    process.exitCode = 1;
    return { response, decrypted: null, raw };
  }

  try {
    return { response, decrypted: decryptResponse(raw, responseNonce, uncompressedSize), raw };
  } catch (error) {
    console.error(`Could not decrypt response: ${error instanceof Error ? error.message : String(error)}`);
    console.log(raw);
    process.exitCode = 1;
    return { response, decrypted: null, raw };
  }
}

/** Prints a decoded body that is not a market snapshot (an error envelope, typically). */
function emitOpaquePayload(decrypted: string): void {
  console.log(heading("PAYLOAD"));
  try {
    console.log(JSON.stringify(JSON.parse(decrypted), null, 2));
  } catch {
    console.log(decrypted);
  }
}

function emitJson(request: PreparedRequest, exchange: Exchange | null, extra: Record<string, unknown> = {}): void {
  const payload = exchange?.decrypted;
  let parsed: unknown = null;
  if (payload !== null && payload !== undefined) {
    try {
      parsed = JSON.parse(payload);
    } catch {
      parsed = payload;
    }
  }
  console.log(JSON.stringify({
    request: {
      method: request.method,
      endpoint: `${API_ORIGIN}${request.path}`,
      url: request.url,
      headers: Object.fromEntries(request.headers as unknown as Iterable<[string, string]>),
      envelope: Object.fromEntries(request.fields.map((field) => [field.name, field.display ?? field.value])),
      plaintextLength: encoder.encode(request.plaintext).length,
      ...request.stamp,
    },
    ...extra,
    status: exchange?.response.status ?? null,
    payload: parsed,
  }, null, 2));
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

async function fetchMarket(
  session: Session,
  marketId: string,
  options: SendOptions,
): Promise<{ request: PreparedRequest; exchange: Exchange | null }> {
  const stamp = nextStamp(session.args);
  const request = prepareRequest(
    session,
    MARKET_LIST,
    listEnvelopeFields(marketId, session.credentials, stamp.frontierTime),
    stamp,
  );
  return { request, exchange: await send(session, request, options) };
}

/** One market's worth of results, whether it came from a single poll or a system sweep. */
interface MarketVisit {
  readonly marketId: number;
  readonly name: string;
  readonly status: number | null;
  readonly snapshot: MarketSnapshot | null;
  readonly eddn: EddnResult | null;
  readonly attempts?: number;
  readonly failure?: string | null;
}

const SWEEP_COLUMNS: readonly TableColumn[] = [
  { key: "marketId", header: "Market ID", align: "right" },
  { key: "name", header: "Name", minWidth: 12, maxWidth: 32 },
  { key: "status", header: "HTTP", align: "right" },
  { key: "commodities", header: "Comm", align: "right" },
  { key: "supplied", header: "Sup", align: "right", priority: 2 },
  { key: "demanded", header: "Dem", align: "right", priority: 2 },
  { key: "eddn", header: "EDDN", priority: 1 },
  { key: "attempts", header: "Try", align: "right", priority: 3 },
];

function emitSweepSummary(visits: readonly MarketVisit[], title: string): void {
  emitTable(title, SWEEP_COLUMNS, visits.map((visit) => ({
    kind: "data" as const,
    cells: {
      marketId: String(visit.marketId),
      name: visit.name,
      status: visit.status === null ? "-" : String(visit.status),
      commodities: visit.snapshot === null ? "-" : formatInteger(visit.snapshot.commodities.length),
      supplied: visit.snapshot === null
        ? "-"
        : formatQuantity(visit.snapshot.commodities.filter((commodity) => commodity.stock > 0).length),
      demanded: visit.snapshot === null
        ? "-"
        : formatQuantity(visit.snapshot.commodities.filter((commodity) => commodity.demand > 0).length),
      eddn: visit.eddn === null ? "-" : visit.eddn.ok ? "sent" : clampText(visit.eddn.detail, 24),
      attempts: String(visit.attempts ?? 1),
    },
  })));
}

/** Polls one market id and optionally forwards it to EDDN. */
async function visitMarket(
  session: Session,
  marketId: number,
  name: string,
  station: EddnStation | null,
  eddn: EddnOptions | null,
  quiet: boolean,
  signal?: AbortSignal,
): Promise<MarketVisit> {
  const { exchange } = await fetchMarket(session, String(marketId), { quiet, signal });
  const snapshot = exchange?.decrypted ? parseMarketSnapshot(exchange.decrypted) : null;
  if (exchange?.decrypted && !snapshot && !quiet) emitOpaquePayload(exchange.decrypted);

  let result: EddnResult | null = null;
  if (eddn !== null && snapshot !== null && station !== null) {
    const { payload, count } = buildEddnMessage(station, marketId, snapshot.commodities, new Date().toISOString(), eddn);
    result = session.dryRun
      ? { ok: true, status: null, detail: `dry-run: ${count} commodities ready`, commodities: count }
      : await submitToEddn(payload, count, signal);
  }
  return { marketId, name, status: exchange?.response.status ?? null, snapshot, eddn: result };
}

// ---------------------------------------------------------------------------
// Sweep worker pool
// ---------------------------------------------------------------------------

interface SweepSettings {
  readonly workers: number;
  readonly timeoutMs: number;
  /** Total tries per market: one initial attempt plus this many requeues. */
  readonly requeues: number;
  readonly quiet: boolean;
  readonly detail: boolean;
}

interface SweepJob {
  readonly target: MarketPoint;
  attempts: number;
}

/**
 * Races the work against a timer and aborts the signal when it expires. The race matters as
 * much as the abort: it guarantees the attempt settles even if something downstream ignores
 * cancellation, which is what lets one hung market stop pinning a worker.
 */
async function withTimeout<T>(milliseconds: number, work: (signal: AbortSignal) => Promise<T>): Promise<T> {
  const controller = new AbortController();
  let timer: ReturnType<typeof setTimeout> | undefined;
  const expiry = new Promise<never>((_, reject) => {
    timer = setTimeout(() => {
      controller.abort();
      reject(new Error(`timed out after ${formatInteger(milliseconds)} ms`));
    }, milliseconds);
  });
  try {
    return await Promise.race([work(controller.signal), expiry]);
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }
}

/**
 * A 4xx means the request itself is wrong — retrying it three times just repeats the
 * mistake. Timeouts, rate limits and 5xx are the transient cases worth requeueing.
 */
function isTransientStatus(status: number | null): boolean {
  if (status === null) return true;
  return status === 408 || status === 429 || status >= 500;
}

function describeFailure(error: unknown): string {
  if (error instanceof Error) {
    return error.name === "AbortError" ? "aborted (timeout)" : error.message;
  }
  return String(error);
}

/**
 * Workers pull from one shared queue, so a free worker immediately takes the next market
 * rather than waiting on a slow neighbour. A failed market goes to the BACK of the queue,
 * which keeps one bad id from being retried head-first while others wait.
 *
 * Requeueing covers the Frontier poll only. An EDDN submission is attempted at most once per
 * market: the spec requires a minimum one-minute wait before retrying any failed message and
 * forbids retrying 400 or 426 at all, so a fast requeue would breach it.
 */
async function sweepMarkets(
  session: Session,
  targets: readonly MarketPoint[],
  systemName: string,
  eddn: EddnOptions | null,
  settings: SweepSettings,
): Promise<MarketVisit[]> {
  const queue: SweepJob[] = targets.map((target) => ({ target, attempts: 0 }));
  const finished = new Map<number, MarketVisit>();
  const maxAttempts = settings.requeues + 1;
  let outstanding = targets.length;
  let completed = 0;

  const worker = async (): Promise<void> => {
    while (outstanding > 0) {
      const job = queue.shift();
      if (job === undefined) {
        // Nothing to steal right now, but a peer may still requeue something.
        await sleep(25);
        continue;
      }
      job.attempts++;

      const station: EddnStation = {
        systemName,
        stationName: job.target.name,
        stationType: job.target.type === "carrier" ? "FleetCarrier" : null,
        economies: null,
      };

      let visit: MarketVisit;
      let failure: string | null = null;
      try {
        visit = await withTimeout(settings.timeoutMs, (signal) =>
          visitMarket(session, job.target.marketId, job.target.name, eddn === null ? null : station, eddn, true, signal));
        if (visit.snapshot === null && !session.dryRun) {
          failure = visit.status === null ? "no response" : `HTTP ${visit.status}`;
        }
      } catch (error) {
        failure = describeFailure(error);
        visit = { marketId: job.target.marketId, name: job.target.name, status: null, snapshot: null, eddn: null };
      }

      const canRetry = failure !== null && visit.snapshot === null && isTransientStatus(visit.status);
      if (canRetry && job.attempts < maxAttempts) {
        queue.push(job);
        if (!settings.quiet) {
          emitProgressLine(
            `[requeue ${job.attempts}/${settings.requeues}] ${job.target.name} (${job.target.marketId}): ${failure}`,
          );
        }
        continue;
      }

      outstanding--;
      completed++;
      finished.set(job.target.marketId, { ...visit, attempts: job.attempts, failure });

      if (!settings.quiet) {
        const outcome = visit.snapshot === null
          ? failure ?? "no data"
          : `${formatInteger(visit.snapshot.commodities.length)} commodities`;
        emitProgressLine(
          `[${completed}/${targets.length}] ${job.target.name} (${job.target.marketId})  ` +
            `HTTP ${visit.status ?? "-"}  ${outcome}` +
            `${job.attempts > 1 ? `  after ${job.attempts} attempts` : ""}` +
            `${visit.eddn === null ? "" : `  eddn ${visit.eddn.ok ? "sent" : visit.eddn.detail}`}`,
        );
      }
      if (settings.detail && visit.snapshot !== null && !settings.quiet) {
        emitMarketSnapshot(visit.snapshot, `MARKET  ${job.target.name} (${job.target.marketId})`);
      }
    }
  };

  await Promise.all(Array.from({ length: settings.workers }, () => worker()));
  // Report in the order the markets were listed, not the order they happened to finish.
  return targets.map((target) => finished.get(target.marketId)).filter((visit): visit is MarketVisit => visit !== undefined);
}

async function runMarketSingle(session: Session, marketId: number): Promise<void> {
  const wantsEddn = switchValue(session.args, "eddn", false) || switchValue(session.args, "eddntest", false);
  const eddn = wantsEddn ? loadEddnOptions(session.args, session.credentials) : null;

  let station: EddnStation | null = null;
  if (eddn !== null) {
    const systemName = optionalValue(session.args, "system");
    const stationName = optionalValue(session.args, "station");
    if (systemName !== undefined && stationName !== undefined) {
      station = { systemName, stationName, stationType: optionalValue(session.args, "stationtype") ?? null, economies: null };
    } else {
      if (!session.json) emitNote(`resolving market ${marketId} through Ardent for the names EDDN requires...`);
      station = await resolveStationByMarketId(marketId);
    }
    if (station === null) {
      throw new Error(
        `EDDN needs a system and station name, and Ardent does not know market ${marketId}. ` +
          "Pass --system and --station, or sweep the whole system instead.",
      );
    }
  }

  const visit = await visitMarket(session, marketId, station?.stationName ?? `market ${marketId}`, station, eddn, session.json);

  if (session.json) {
    console.log(JSON.stringify({
      marketId,
      station,
      status: visit.status,
      eddn: visit.eddn,
      payload: visit.snapshot?.payload ?? null,
    }, null, 2));
    return;
  }
  if (visit.snapshot === null) return;

  emitMarketSnapshot(
    visit.snapshot,
    `MARKET SUMMARY  ${station ? `${station.stationName}, ${station.systemName}` : `market ${marketId}`}`,
  );
  if (visit.eddn !== null) {
    emitNote(`EDDN: ${visit.eddn.detail} (${formatInteger(visit.eddn.commodities)} commodities${eddn?.test ? ", test schema" : ""})`);
  }
}

async function runMarketSweep(session: Session, name: string): Promise<void> {
  const args = session.args;
  const wantsEddn = switchValue(args, "eddn", false) || switchValue(args, "eddntest", false);
  const eddn = wantsEddn ? loadEddnOptions(args, session.credentials) : null;

  if (!session.json) emitNote(`resolving "${name}" through Ardent...`);
  const resolved = await resolveLocation(name, optionalValue(args, "station") !== undefined ? "station" : "auto");

  if (!session.json) emitNote(`reading ${STARSYSTEM.path} for ${resolved.name} to find its markets...`);
  const stamp = nextStamp(args);
  const request = prepareRequest(
    session,
    STARSYSTEM,
    starsystemEnvelopeFields(resolved.address, optionalValue(args, "language") ?? "en", 0, session.credentials, stamp.frontierTime),
    stamp,
  );
  const exchange = await send(session, request, { quiet: true, ignoreDryRun: true });
  if (!exchange?.decrypted) throw new Error("Could not read the star system; try `markets` first to see what is there");

  const payload = asRecord(JSON.parse(exchange.decrypted));
  const all = payload ? readMarketPoints(payload) : [];
  if (all.length === 0) throw new Error("No markets found in that system; run `markets --dump <file>` to inspect the payload");

  const includeCarriers = switchValue(args, "carriers", false);
  const includeIdle = switchValue(args, "allmarkets", false);
  let targets = all;
  const skippedCarriers = includeCarriers ? 0 : targets.filter((point) => point.type === "carrier").length;
  if (!includeCarriers) targets = targets.filter((point) => point.type !== "carrier");
  const skippedIdle = includeIdle ? 0 : targets.filter((point) => point.imports === 0 && point.exports === 0).length;
  if (!includeIdle) targets = targets.filter((point) => point.imports > 0 || point.exports > 0);

  if (targets.length === 0) throw new Error("Every market was filtered out; add --all-markets or --carriers");

  const settings: SweepSettings = {
    workers: Math.max(1, Math.min(MAX_CONCURRENCY, optionalNumber(args, "concurrency") ?? DEFAULT_CONCURRENCY)),
    timeoutMs: Math.round((optionalDecimal(args, "timeout") ?? DEFAULT_TIMEOUT_SECONDS) * 1_000),
    requeues: optionalNumber(args, "requeue") ?? DEFAULT_REQUEUES,
    quiet: session.json,
    detail: switchValue(args, "detail", false),
  };

  if (!session.json) {
    emitTable("SWEEP", FIELD_COLUMNS, [
      fieldRow("system", `${resolved.name} (${resolved.address})`),
      fieldRow("markets", `${targets.length} of ${all.length}`),
      fieldRow("workers", `${settings.workers} pulling from one queue`),
      fieldRow("timeout", `${settings.timeoutMs / 1_000}s per attempt, up to ${settings.requeues} requeues`),
      fieldRow("eddn", eddn === null ? "off" : eddn.test ? "test schema" : "live"),
      ...(skippedCarriers === 0 ? [] : [fieldRow("carriers skipped", skippedCarriers)]),
      ...(skippedIdle === 0 ? [] : [fieldRow("no-market skipped", skippedIdle)]),
    ]);
  }

  const visits = await sweepMarkets(session, targets, resolved.name, eddn, settings);

  if (session.json) {
    console.log(JSON.stringify({
      system: resolved,
      markets: visits.map((visit) => ({
        marketId: visit.marketId,
        name: visit.name,
        status: visit.status,
        eddn: visit.eddn,
        payload: visit.snapshot?.payload ?? null,
      })),
    }, null, 2));
    return;
  }

  emitSweepSummary(visits, `SWEEP RESULTS  ${resolved.name} — ${visits.length} markets`);
  const failed = visits.filter((visit) => visit.snapshot === null).length;
  if (failed > 0) {
    process.exitCode = 1;
    emitNote(`${failed} markets returned no usable data`);
  }
  if (eddn !== null) {
    const sent = visits.filter((visit) => visit.eddn?.ok).length;
    const rejected = visits.filter((visit) => visit.eddn !== null && !visit.eddn.ok);
    emitNote(`EDDN: ${sent} sent, ${rejected.length} rejected${eddn.test ? " (test schema — not relayed to consumers)" : ""}`);
    for (const visit of rejected.slice(0, 5)) emitNote(`  ${visit.name}: ${visit.eddn!.detail}`);
  }
}

async function runMarket(session: Session): Promise<void> {
  // An explicit --market-id pins one market, and then --system/--station only name it for
  // EDDN. A bare name sweeps. MARKET_ID from the environment is the last resort, so that
  // `market <system>` still sweeps when the variable happens to be set.
  const explicitId = optionalValue(session.args, "marketid");
  if (explicitId !== undefined) return runMarketSingle(session, parseUnsignedInteger("--market-id", explicitId));

  const positional = session.args.positionals.join(" ").trim();
  const name = optionalValue(session.args, "system") ?? optionalValue(session.args, "station") ??
    (positional === "" ? undefined : positional);
  if (name !== undefined) return runMarketSweep(session, name);

  const fromEnvironment = optionalValue(session.args, "marketid", "MARKET_ID");
  if (fromEnvironment === undefined) {
    throw new Error("market needs a system name, or --market-id <id> (or MARKET_ID in the environment)");
  }
  return runMarketSingle(session, parseUnsignedInteger("MARKET_ID", fromEnvironment));
}

// ---------------------------------------------------------------------------
// trade
// ---------------------------------------------------------------------------

type TransactionType = "buy" | "sell";

interface TradePlan {
  readonly marketId: string;
  readonly transactionType: TransactionType;
  readonly commodityId: number;
  readonly commodityName: string;
  readonly blackMarket: boolean;
  readonly stolen: boolean;
  readonly unitPrice: number;
  readonly qty: number;
  readonly finalQty: number;
}

/** Where each resolved value came from, so the plan table can show its provenance. */
type PlanSource = "flag" | "market" | "default";

interface PlanField {
  readonly label: string;
  readonly value: string;
  readonly source: PlanSource;
}

const PLAN_COLUMNS: readonly TableColumn[] = [
  { key: "field", header: "Field", minWidth: 8, maxWidth: 20 },
  { key: "value", header: "Value", minWidth: 10 },
  { key: "source", header: "From", priority: 1 },
];

function findCommodity(commodities: readonly Commodity[], token: string): Commodity {
  if (/^\d+$/.test(token)) {
    const byId = commodities.find((commodity) => commodity.id === Number(token));
    if (byId) return byId;
    throw new Error(`No commodity with id ${token} at this market`);
  }

  const needle = token.replace(/[\s_-]/g, "").toLowerCase();
  const byName = commodities.filter((commodity) => commodity.name.toLowerCase() === needle);
  if (byName.length === 1) return byName[0]!;

  const partial = commodities.filter((commodity) => commodity.name.toLowerCase().includes(needle));
  if (partial.length === 1) return partial[0]!;
  if (partial.length === 0) throw new Error(`No commodity matching "${token}" at this market`);
  throw new Error(
    `"${token}" matches ${partial.length} commodities: ${partial.slice(0, 8).map((c) => c.name).join(", ")}${
      partial.length > 8 ? ", ..." : ""
    }`,
  );
}

/** Total units in the hold, which is what a cargo capacity is measured against. */
function cargoUsed(inventory: readonly unknown[]): number {
  let total = 0;
  for (const entry of inventory) {
    const item = asRecord(entry);
    if (item) total += readNumber(item, "qty");
  }
  return total;
}

/** Illegal goods and anything stolen only move through the black market. */
function deriveBlackMarket(commodity: Commodity | null, stolen: boolean, explicit: boolean | undefined): boolean {
  return explicit ?? (stolen || (commodity?.illegal ?? false));
}

function derivePrice(commodity: Commodity, transactionType: TransactionType, blackMarket: boolean): number {
  if (transactionType === "buy") {
    if (commodity.buyPrice === 0) throw new Error(`${commodity.name} is not sold at this market (buyPrice 0)`);
    return commodity.buyPrice;
  }
  return blackMarket ? commodity.fencePrice : commodity.sellPrice;
}

/**
 * `finalQty` is the size the commodity's stack ends up at, not a copy of `qty` — the game's own
 * logs show `qty=13 finalQty=130` when 117 units were already aboard. Sending qty there is
 * rejected with HTTP 402. The sell direction is inferred; only buys appear in captured traffic.
 */
function resultingStack(held: number, qty: number, transactionType: TransactionType): number {
  return transactionType === "buy" ? held + qty : Math.max(0, held - qty);
}

/** Units of `commodity` currently held, matching the stolen flag of the intended sale. */
function heldQuantity(inventory: readonly unknown[], commodity: Commodity, stolen: boolean): number {
  let total = 0;
  for (const entry of inventory) {
    const item = asRecord(entry);
    if (!item) continue;
    if (readString(item, "commodity").toLowerCase() !== commodity.name.toLowerCase()) continue;
    if (readBoolean(item, "stolen") !== stolen) continue;
    total += readNumber(item, "qty");
  }
  return total;
}

interface ResolvedTrade {
  readonly plan: TradePlan;
  readonly fields: readonly PlanField[];
  readonly notes: readonly string[];
  readonly snapshot: MarketSnapshot | null;
}

function resolveTrade(session: Session, snapshot: MarketSnapshot | null): ResolvedTrade {
  const args = session.args;
  const marketId = requireValue(args, "marketid", "MARKET_ID");
  const rawType = requireValue(args, "type").toLowerCase();
  if (rawType !== "buy" && rawType !== "sell") throw new Error(`--type must be buy or sell, not "${rawType}"`);
  const transactionType = rawType;

  const item = requireValue(args, "item");
  const requestedQty = optionalNumber(args, "qty");
  if (requestedQty === undefined) throw new Error("Missing required option --qty");
  if (requestedQty === 0) throw new Error("--qty must be at least 1");

  const explicitPrice = optionalNumber(args, "unitprice");
  const explicitBlackMarket = optionalSwitch(args, "blackmarket");
  const stolen = switchValue(args, "stolen", false);
  const capQty = switchValue(args, "cap", true);

  const commodity = snapshot ? findCommodity(snapshot.commodities, item) : null;
  if (!commodity && !/^\d+$/.test(item)) throw new Error("--item must be a numeric id when --no-resolve is used");
  if (!commodity && explicitPrice === undefined) throw new Error("--unit-price is required when --no-resolve is used");

  const fields: PlanField[] = [];
  const notes: string[] = [];
  const record = (label: string, value: string | number, source: PlanSource): void => {
    fields.push({ label, value: String(value), source });
  };

  const commodityId = commodity ? commodity.id : Number(item);
  const commodityName = commodity ? commodity.name : `id ${commodityId}`;
  const blackMarket = deriveBlackMarket(commodity, stolen, explicitBlackMarket);

  let unitPrice = explicitPrice;
  let priceSource: PlanSource = "flag";
  if (unitPrice === undefined && commodity) {
    priceSource = "market";
    unitPrice = derivePrice(commodity, transactionType, blackMarket);
  }
  if (unitPrice === undefined) throw new Error("Could not determine a unit price; pass --unit-price");

  let qty = requestedQty;
  let qtySource: PlanSource = "flag";
  if (commodity && capQty) {
    const available = transactionType === "buy"
      ? commodity.stock
      : heldQuantity(snapshot!.inventory, commodity, stolen);
    const label = transactionType === "buy" ? "stock" : stolen ? "stolen holdings" : "holdings";
    if (available === 0) {
      throw new Error(
        `${commodity.name}: ${label} is 0, nothing to ${transactionType}. Pass --no-cap to send the request anyway.`,
      );
    }
    if (qty > available) {
      notes.push(`--qty ${formatInteger(requestedQty)} clamped to ${label} ${formatInteger(available)}`);
      qty = available;
      qtySource = "market";
    }

    // A buy cannot exceed the room left in the hold, when a capacity is known.
    const cargo = optionalNumber(args, "cargo");
    if (cargo !== undefined && transactionType === "buy") {
      const free = cargo - cargoUsed(snapshot!.inventory);
      if (free <= 0) throw new Error(`Cargo is full (${formatInteger(cargo)} units); nothing can be bought`);
      if (qty > free) {
        notes.push(`qty ${formatInteger(qty)} clamped to free cargo space ${formatInteger(free)}`);
        qty = free;
        qtySource = "market";
      }
    }
  }

  const explicitFinalQty = optionalNumber(args, "finalqty");
  let finalQty: number;
  if (explicitFinalQty !== undefined) {
    finalQty = explicitFinalQty;
  } else if (commodity) {
    finalQty = resultingStack(heldQuantity(snapshot!.inventory, commodity, stolen), qty, transactionType);
  } else {
    // Without a listing there is no way to know the current stack size.
    finalQty = qty;
    notes.push("--no-resolve: finalQty falls back to qty, which is only right if you hold none of this commodity");
  }

  record("marketId", marketId, optionalValue(args, "marketid") ? "flag" : "default");
  record("transactionType", transactionType, "flag");
  record("commodityId", `${commodityId} (${commodityName})`, /^\d+$/.test(item) ? "flag" : "market");
  record("blackMarket", blackMarket ? "1" : "0", explicitBlackMarket === undefined ? "market" : "flag");
  record("stolen", stolen ? "1" : "0", optionalSwitch(args, "stolen") === undefined ? "default" : "flag");
  record("unitPrice", formatInteger(unitPrice), priceSource);
  record("qty", formatInteger(qty), qtySource);
  record("finalQty", formatInteger(finalQty), explicitFinalQty !== undefined ? "flag" : commodity ? "market" : "default");
  record("total", `${formatInteger(unitPrice * qty)} cr`, "default");

  if (commodity) {
    const held = heldQuantity(snapshot!.inventory, commodity, stolen);
    notes.push(
      `${commodity.name}: stock ${formatQuantity(commodity.stock)} | demand ${formatQuantity(commodity.demand)} | ` +
        `buy ${formatQuantity(commodity.buyPrice)} | sell ${formatQuantity(commodity.sellPrice)} | ` +
        `fence ${formatQuantity(commodity.fencePrice)} | held ${formatQuantity(held)}`,
    );
  }

  return {
    plan: { marketId, transactionType, commodityId, commodityName, blackMarket, stolen, unitPrice, qty, finalQty },
    fields,
    notes,
    snapshot,
  };
}

function emitTradePlan(resolved: ResolvedTrade): void {
  emitTable(
    `TRADE PLAN  ${resolved.plan.transactionType} ${formatInteger(resolved.plan.qty)} x ${resolved.plan.commodityName}`,
    PLAN_COLUMNS,
    resolved.fields.map((field) => ({
      kind: "data" as const,
      cells: { field: field.label, value: field.value, source: field.source },
    })),
  );
  for (const note of resolved.notes) emitNote(note);
}

/** Fetches the listing and insists on commodity data; used by every resolving path. */
async function requireMarketSnapshot(session: Session, marketId: string): Promise<MarketSnapshot> {
  const lookup = await fetchMarket(session, marketId, { quiet: true, ignoreDryRun: true });
  if (!lookup.exchange?.decrypted) {
    throw new Error("Could not read the market listing; retry with --no-resolve and explicit values");
  }
  const snapshot = parseMarketSnapshot(lookup.exchange.decrypted);
  if (!snapshot) {
    emitOpaquePayload(lookup.exchange.decrypted);
    throw new Error("Market listing did not contain commodity data");
  }
  return snapshot;
}

async function runSingleTrade(session: Session): Promise<void> {
  const resolve = switchValue(session.args, "resolve", true);
  let snapshot: MarketSnapshot | null = null;

  if (resolve) {
    const marketId = requireValue(session.args, "marketid", "MARKET_ID");
    if (!session.json) emitNote(`resolving against ${MARKET_LIST_PATH} for market ${marketId}...`);
    // The listing is read-only, so it still runs under --dry-run to resolve the plan.
    snapshot = await requireMarketSnapshot(session, marketId);
  }

  const resolved = resolveTrade(session, snapshot);
  if (!session.json) emitTradePlan(resolved);

  const stamp = nextStamp(session.args);
  const request = prepareRequest(
    session,
    MARKET_TRADE,
    tradeEnvelopeFields(resolved.plan, session.credentials, stamp.frontierTime),
    stamp,
  );
  const exchange = await send(session, request);

  if (session.json) {
    emitJson(request, exchange, { plan: resolved.plan });
    return;
  }
  if (!exchange?.decrypted) return;

  const result = parseMarketSnapshot(exchange.decrypted);
  if (!result) {
    emitOpaquePayload(exchange.decrypted);
    return;
  }

  emitMarketSummary(result, `TRADE RESULT  ${resolved.plan.transactionType} ${resolved.plan.commodityName}`);
  emitInventoryTable(result.inventory);

  const traded = result.commodities.find((commodity) => commodity.id === resolved.plan.commodityId);
  if (traded) {
    emitTable(`${traded.name.toUpperCase()}  after the trade`, COMMODITY_COLUMNS, [commodityRow(traded)]);
  }
  if (switchValue(session.args, "fullmarket", false)) emitCommodityTable(result.commodities);
  else emitNote("pass --full-market to print the whole commodity table from this response");
}

// ---------------------------------------------------------------------------
// Batch trading: several commodities, cargo filling, and retry-until-full
// ---------------------------------------------------------------------------

interface BatchSettings {
  readonly marketId: string;
  readonly transactionType: TransactionType;
  readonly items: readonly string[];
  readonly fill: boolean;
  readonly cargo: number | undefined;
  /** Per-commodity ceiling; required unless --fill decides the amount. */
  readonly perItemQty: number | undefined;
  readonly stolen: boolean;
  readonly explicitBlackMarket: boolean | undefined;
  readonly explicitPrice: number | undefined;
  readonly watch: boolean;
  readonly intervalMs: number;
  readonly attemptLimit: number;
  /** Starting balance, if known; otherwise it is learned from the first trade reply. */
  readonly credits: number | undefined;
}

interface TradeRecord {
  readonly round: number;
  readonly commodity: string;
  readonly commodityId: number;
  readonly qty: number;
  readonly unitPrice: number;
  readonly status: number | null;
  readonly cargoUsed: number | null;
  readonly credits: number | null;
}

const TRADE_LOG_COLUMNS: readonly TableColumn[] = [
  { key: "round", header: "#", align: "right" },
  { key: "commodity", header: "Commodity", minWidth: 10, maxWidth: 28 },
  { key: "qty", header: "Qty", align: "right" },
  { key: "unitPrice", header: "Unit", align: "right" },
  { key: "total", header: "Total", align: "right" },
  { key: "status", header: "HTTP", align: "right", priority: 1 },
  { key: "cargo", header: "Cargo", align: "right", priority: 2 },
];

function sleep(milliseconds: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

function formatCargo(used: number, cargo: number | undefined): string {
  return cargo === undefined ? formatInteger(used) : `${formatInteger(used)}/${formatInteger(cargo)}`;
}

/** One streamed progress line; a table would have to buffer until the loop ended. */
function emitProgressLine(text: string): void {
  console.log(clampText(text, TERMINAL_WIDTH));
}

function readCredits(payload: Record<string, unknown>): number | null {
  return "credits" in payload ? readNumber(payload, "credits") : null;
}

function loadBatchSettings(session: Session, items: readonly string[]): BatchSettings {
  const args = session.args;
  const rawType = requireValue(args, "type").toLowerCase();
  if (rawType !== "buy" && rawType !== "sell") throw new Error(`--type must be buy or sell, not "${rawType}"`);

  const fill = switchValue(args, "fill", false);
  const cargo = optionalNumber(args, "cargo");
  const perItemQty = optionalNumber(args, "qty");
  const watch = switchValue(args, "watch", false);
  const attemptLimit = optionalNumber(args, "attempts") ?? 0;
  const interval = optionalDecimal(args, "interval") ?? 1;

  if (fill && rawType !== "buy") throw new Error("--fill only applies to --type buy");
  if (fill && cargo === undefined) throw new Error("--fill needs --cargo <capacity> to know when the hold is full");
  if (fill && !switchValue(args, "cap", true)) throw new Error("--fill cannot be combined with --no-cap");
  if (!fill && perItemQty === undefined) throw new Error("Missing required option --qty (or pass --fill)");
  if (perItemQty === 0) throw new Error("--qty must be at least 1");
  if (!switchValue(args, "resolve", true)) throw new Error("--no-resolve cannot be used with --fill or multiple items");
  if (watch && !fill && attemptLimit === 0) {
    throw new Error("--watch needs --fill (or --attempts <n>) so it has a stopping condition");
  }
  if (interval < 0.1 || interval > 3_600) throw new Error("--interval must be between 0.1 and 3600 seconds");

  return {
    marketId: requireValue(args, "marketid", "MARKET_ID"),
    transactionType: rawType,
    items,
    fill,
    cargo,
    perItemQty,
    stolen: switchValue(args, "stolen", false),
    explicitBlackMarket: optionalSwitch(args, "blackmarket"),
    explicitPrice: optionalNumber(args, "unitprice"),
    watch,
    intervalMs: Math.round(interval * 1_000),
    attemptLimit,
    credits: optionalNumber(args, "credits"),
  };
}

async function runBatchTrade(session: Session, items: readonly string[]): Promise<void> {
  const settings = loadBatchSettings(session, items);
  const executed: TradeRecord[] = [];
  let credits: number | null = settings.credits ?? null;
  let consecutiveFailures = 0;
  let latest = await requireMarketSnapshot(session, settings.marketId);

  // Names resolve once: the commodity set is fixed per market, only prices and stock move.
  const targets = settings.items.map((token) => findCommodity(latest.commodities, token));
  const duplicate = targets.find((target, index) => targets.findIndex((other) => other.id === target.id) !== index);
  if (duplicate) throw new Error(`--item lists ${duplicate.name} more than once`);

  if (!session.json) {
    emitTable("BATCH PLAN", FIELD_COLUMNS, [
      fieldRow("market", settings.marketId),
      fieldRow("action", settings.fill ? "buy until the hold is full" : `${settings.transactionType} up to --qty each`),
      fieldRow("order", targets.map((target) => target.name).join(" -> ")),
      ...(settings.cargo === undefined ? [] : [fieldRow("cargo", `${formatCargo(cargoUsed(latest.inventory), settings.cargo)} units`)]),
      ...(settings.perItemQty === undefined ? [] : [fieldRow("per item max", formatInteger(settings.perItemQty))]),
      fieldRow(
        "retry",
        settings.watch
          ? `every ${settings.intervalMs / 1_000}s${settings.attemptLimit ? `, up to ${settings.attemptLimit} rounds` : " until filled"}`
          : "single pass",
      ),
    ]);
  }

  let round = 0;
  let outcome = "";
  while (outcome === "") {
    round++;
    if (round > 1) latest = await requireMarketSnapshot(session, settings.marketId);

    let used = cargoUsed(latest.inventory);
    let free = settings.cargo === undefined ? Number.POSITIVE_INFINITY : settings.cargo - used;
    if (settings.fill && free <= 0) {
      outcome = "hold is full";
      break;
    }

    let tradesThisRound = 0;
    let abandonRound = false;
    const skipped: string[] = [];

    for (const target of targets) {
      if (settings.fill && free <= 0) break;

      const current = latest.commodities.find((commodity) => commodity.id === target.id);
      if (!current) {
        skipped.push(`${target.name}: no longer listed`);
        continue;
      }

      const blackMarket = deriveBlackMarket(current, settings.stolen, settings.explicitBlackMarket);
      let unitPrice: number;
      try {
        unitPrice = settings.explicitPrice ?? derivePrice(current, settings.transactionType, blackMarket);
      } catch (error) {
        skipped.push(error instanceof Error ? error.message : String(error));
        continue;
      }

      const held = heldQuantity(latest.inventory, current, settings.stolen);
      const available = settings.transactionType === "buy" ? current.stock : held;
      let qty = settings.fill ? Math.min(free, available) : Math.min(settings.perItemQty!, available);
      if (settings.transactionType === "buy") {
        if (Number.isFinite(free)) qty = Math.min(qty, free);
        // Never queue a purchase the balance cannot cover, once the balance is known.
        if (credits !== null && unitPrice > 0) qty = Math.min(qty, Math.floor(credits / unitPrice));
      }
      qty = Math.max(0, Math.floor(qty));

      if (qty === 0) {
        const reason = available === 0
          ? settings.transactionType === "buy" ? "no stock" : "nothing held"
          : credits !== null && credits < unitPrice ? "not enough credits" : "no cargo space";
        skipped.push(`${current.name}: ${reason}`);
        continue;
      }

      const plan: TradePlan = {
        marketId: settings.marketId,
        transactionType: settings.transactionType,
        commodityId: current.id,
        commodityName: current.name,
        blackMarket,
        stolen: settings.stolen,
        unitPrice,
        qty,
        finalQty: resultingStack(held, qty, settings.transactionType),
      };
      const stamp = nextStamp(session.args);
      const request = prepareRequest(
        session,
        MARKET_TRADE,
        tradeEnvelopeFields(plan, session.credentials, stamp.frontierTime),
        stamp,
      );

      if (session.dryRun) {
        // Simulate locally so a multi-item fill still previews the whole sequence.
        used += settings.transactionType === "buy" ? qty : -qty;
        free = settings.cargo === undefined ? Number.POSITIVE_INFINITY : settings.cargo - used;
        executed.push({ round, commodity: current.name, commodityId: current.id, qty, unitPrice, status: null, cargoUsed: used, credits });
        if (!session.json) {
          emitProgressLine(
            `[${round}] would ${settings.transactionType} ${formatInteger(qty)} x ${current.name} @ ${formatInteger(unitPrice)}` +
              ` = ${formatInteger(qty * unitPrice)} cr  cargo ${formatCargo(used, settings.cargo)}`,
          );
        }
        tradesThisRound++;
        continue;
      }

      const exchange = await send(session, request, { quiet: true });
      const result = exchange?.decrypted ? parseMarketSnapshot(exchange.decrypted) : null;
      if (result) {
        latest = result;
        credits = readCredits(result.payload) ?? credits;
        used = cargoUsed(result.inventory);
        free = settings.cargo === undefined ? Number.POSITIVE_INFINITY : settings.cargo - used;
      }

      executed.push({
        round,
        commodity: current.name,
        commodityId: current.id,
        qty,
        unitPrice,
        status: exchange?.response.status ?? null,
        cargoUsed: result ? used : null,
        credits,
      });
      tradesThisRound++;

      if (!session.json) {
        emitProgressLine(
          `[${round}] ${settings.transactionType} ${formatInteger(qty)} x ${current.name} @ ${formatInteger(unitPrice)}` +
            ` = ${formatInteger(qty * unitPrice)} cr  HTTP ${exchange?.response.status ?? "?"}` +
            `  cargo ${formatCargo(used, settings.cargo)}${credits === null ? "" : `  credits ${formatInteger(credits)}`}`,
        );
      }

      if (!exchange || exchange.decrypted === null) {
        // Stock or the balance may have moved under us; a watcher re-reads and tries again.
        consecutiveFailures++;
        abandonRound = true;
        if (!settings.watch || consecutiveFailures >= 3) {
          outcome = `a trade request failed${consecutiveFailures > 1 ? ` ${consecutiveFailures} times in a row` : ""}`;
        }
        break;
      }
      consecutiveFailures = 0;
    }

    if (outcome !== "") break;
    if (abandonRound && !session.json) emitProgressLine(`[${round}] retrying after a failed request`);
    if (settings.fill && free <= 0) {
      outcome = "hold is full";
      break;
    }
    if (session.dryRun) {
      outcome = "--dry-run: nothing was sent";
      break;
    }
    if (!settings.watch) {
      outcome = "single pass complete";
      break;
    }
    if (settings.attemptLimit > 0 && round >= settings.attemptLimit) {
      outcome = `stopped after ${round} rounds`;
      break;
    }

    if (tradesThisRound === 0 && !abandonRound && !session.json) {
      emitProgressLine(
        `[${round}] waiting ${settings.intervalMs / 1_000}s — cargo ${formatCargo(used, settings.cargo)}` +
          `${skipped.length ? `  (${skipped.slice(0, 3).join("; ")})` : ""}`,
      );
    }
    await sleep(settings.intervalMs);
  }

  if (session.json) {
    console.log(JSON.stringify({
      plan: { ...settings, items: targets.map((target) => ({ id: target.id, name: target.name })) },
      outcome,
      rounds: round,
      trades: executed,
      credits,
      cargoUsed: cargoUsed(latest.inventory),
      inventory: latest.inventory,
    }, null, 2));
    return;
  }

  const totalUnits = executed.reduce((sum, record) => sum + record.qty, 0);
  const totalValue = executed.reduce((sum, record) => sum + record.qty * record.unitPrice, 0);
  emitTable(`TRADES  ${executed.length} requests over ${round} round${round === 1 ? "" : "s"} — ${outcome}`, TRADE_LOG_COLUMNS, [
    ...executed.map((record) => ({
      kind: "data" as const,
      cells: {
        round: String(record.round),
        commodity: record.commodity,
        qty: formatInteger(record.qty),
        unitPrice: formatInteger(record.unitPrice),
        total: formatInteger(record.qty * record.unitPrice),
        status: record.status === null ? "-" : String(record.status),
        cargo: record.cargoUsed === null ? "-" : formatCargo(record.cargoUsed, settings.cargo),
      },
    })),
    { kind: "rule" },
    {
      kind: "data",
      cells: {
        round: "",
        commodity: "TOTAL",
        qty: formatInteger(totalUnits),
        unitPrice: "",
        total: formatInteger(totalValue),
        status: "",
        cargo: formatCargo(cargoUsed(latest.inventory), settings.cargo),
      },
    },
  ]);
  if (credits !== null) emitNote(`credits now ${formatInteger(credits)}`);
  emitInventoryTable(latest.inventory);
}

function splitItems(raw: string): string[] {
  const items = raw.split(",").map((token) => token.trim()).filter((token) => token !== "");
  if (items.length === 0) throw new Error("--item needs at least one commodity");
  return items;
}

async function runTrade(session: Session): Promise<void> {
  const items = splitItems(requireValue(session.args, "item"));
  const batch = items.length > 1 || switchValue(session.args, "fill", false) || switchValue(session.args, "watch", false);
  if (batch) await runBatchTrade(session, items);
  else await runSingleTrade(session);
}

// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// System addresses (ID64)
//
// An address packs the boxel a system sits in, not the system's position, so it cannot be
// derived from coordinates alone: the mass code and the system's index inside the boxel are
// generation properties. Layout, low bit first — verified against the game's own
// systemAddr=5378909424384 for Hyades Sector NI-X a16-0:
//
//   3 bits mass code | 7-m boxel Z | 7 sector Z | 7-m boxel Y | 6 sector Y |
//   7-m boxel X | 7 sector X | remainder = index within the boxel
// ---------------------------------------------------------------------------

const SECTOR_SIZE = 1_280;
/** Galactic grid origin: the corner of sector (0,0,0) in light years. */
const GALAXY_ORIGIN = { x: 49_985, y: 40_985, z: 24_105 } as const;

interface Coordinates {
  readonly x: number;
  readonly y: number;
  readonly z: number;
}

interface SystemAddressParts {
  readonly massCode: number;
  readonly massCodeLetter: string;
  readonly boxelSize: number;
  readonly sector: Coordinates;
  readonly boxel: Coordinates;
  readonly index: number;
  /** The boxel's low corner, in galactic coordinates. */
  readonly origin: Coordinates;
}

function decodeSystemAddress(address: number): SystemAddressParts {
  if (!Number.isSafeInteger(address) || address < 0) throw new Error(`${address} is not a system address`);
  let bits = BigInt(address);
  const take = (width: number): number => {
    const value = Number(bits & ((1n << BigInt(width)) - 1n));
    bits >>= BigInt(width);
    return value;
  };

  const massCode = take(3);
  const boxelBits = 7 - massCode;
  const boxelZ = take(boxelBits);
  const sectorZ = take(7);
  const boxelY = take(boxelBits);
  const sectorY = take(6);
  const boxelX = take(boxelBits);
  const sectorX = take(7);
  const index = Number(bits);
  const boxelSize = 10 * 2 ** massCode;

  return {
    massCode,
    massCodeLetter: String.fromCharCode(97 + massCode),
    boxelSize,
    sector: { x: sectorX, y: sectorY, z: sectorZ },
    boxel: { x: boxelX, y: boxelY, z: boxelZ },
    index,
    origin: {
      x: sectorX * SECTOR_SIZE + boxelX * boxelSize - GALAXY_ORIGIN.x,
      y: sectorY * SECTOR_SIZE + boxelY * boxelSize - GALAXY_ORIGIN.y,
      z: sectorZ * SECTOR_SIZE + boxelZ * boxelSize - GALAXY_ORIGIN.z,
    },
  };
}

function encodeSystemAddress(coordinates: Coordinates, massCode: number, index: number): number {
  if (!Number.isInteger(massCode) || massCode < 0 || massCode > 7) throw new Error("mass code must be 0-7 (a-h)");
  const boxelSize = 10 * 2 ** massCode;
  const boxelBits = 7 - massCode;

  const place = (value: number, offset: number, sectorLimit: number, axis: string): { sector: number; boxel: number } => {
    const shifted = value + offset;
    if (shifted < 0) throw new Error(`${axis}=${value} falls outside the galactic grid`);
    const sector = Math.floor(shifted / SECTOR_SIZE);
    if (sector > sectorLimit) throw new Error(`${axis}=${value} falls outside the galactic grid`);
    return { sector, boxel: Math.floor((shifted - sector * SECTOR_SIZE) / boxelSize) };
  };

  const x = place(coordinates.x, GALAXY_ORIGIN.x, 127, "x");
  const y = place(coordinates.y, GALAXY_ORIGIN.y, 63, "y");
  const z = place(coordinates.z, GALAXY_ORIGIN.z, 127, "z");

  let bits = BigInt(massCode);
  let shift = 3n;
  const put = (value: number, width: number): void => {
    bits |= BigInt(value) << shift;
    shift += BigInt(width);
  };
  put(z.boxel, boxelBits);
  put(z.sector, 7);
  put(y.boxel, boxelBits);
  put(y.sector, 6);
  put(x.boxel, boxelBits);
  put(x.sector, 7);
  bits |= BigInt(index) << shift;

  const packed = Number(bits);
  if (!Number.isSafeInteger(packed)) throw new Error("packed address exceeds the safe integer range");
  return packed;
}

function containsCoordinates(parts: SystemAddressParts, coordinates: Coordinates): boolean {
  return (["x", "y", "z"] as const).every((axis) => {
    const low = parts.origin[axis];
    return coordinates[axis] >= low && coordinates[axis] < low + parts.boxelSize;
  });
}

// ---------------------------------------------------------------------------
// Ardent lookup
// ---------------------------------------------------------------------------

interface ResolvedSystem {
  readonly name: string;
  readonly address: number;
  readonly coordinates: Coordinates;
  readonly via: string;
  readonly station: string | null;
}

interface ArdentModule {
  readonly systemUrl: (name: string) => string;
  readonly stationSearchUrl: (name: string) => string;
  readonly parseSystem: (value: unknown) => { name: string; address: number; coords: Coordinates } | null;
  readonly parseStationMatches: (value: unknown) => readonly { stationName: string; systemName: string }[];
}

async function loadArdent(): Promise<ArdentModule> {
  const path = process.env.ARDENT_MODULE?.trim() || ARDENT_MODULE;
  try {
    return (await import(path)) as unknown as ArdentModule;
  } catch (error) {
    throw new Error(
      `Could not load the Ardent endpoint module from ${path} ` +
        `(set ARDENT_MODULE to its location): ${error instanceof Error ? error.message : String(error)}`,
    );
  }
}

async function fetchArdentJson(url: string): Promise<unknown> {
  const response = await fetch(url, { headers: { Accept: "application/json" } });
  if (!response.ok) throw new Error(`Ardent replied HTTP ${response.status} ${response.statusText} for ${url}`);
  return response.json();
}

/** Resolves a system name, or a station name via its system. */
async function resolveLocation(name: string, kind: "auto" | "system" | "station"): Promise<ResolvedSystem> {
  const ardent = await loadArdent();

  if (kind !== "station") {
    const direct = ardent.parseSystem(await fetchArdentJson(ardent.systemUrl(name)).catch(() => null));
    if (direct) {
      return { name: direct.name, address: direct.address, coordinates: direct.coords, via: "system name", station: null };
    }
    if (kind === "system") throw new Error(`Ardent does not know a system called "${name}"`);
  }

  // Station search matches on prefix, so an exact hit wins before a unique prefix hit.
  const matches = ardent.parseStationMatches(await fetchArdentJson(ardent.stationSearchUrl(name)));
  if (matches.length === 0) throw new Error(`Ardent found no system or station matching "${name}"`);
  const exact = matches.filter((match) => match.stationName.toLowerCase() === name.toLowerCase());
  const chosen = exact.length > 0 ? exact : matches;
  if (chosen.length > 1) {
    const systems = new Set(chosen.map((match) => match.systemName));
    if (systems.size > 1) {
      throw new Error(
        `"${name}" matches ${chosen.length} stations across ${systems.size} systems: ` +
          chosen.slice(0, 6).map((match) => `${match.stationName} (${match.systemName})`).join(", ") +
          `${chosen.length > 6 ? ", ..." : ""}`,
      );
    }
  }
  const station = chosen[0]!;
  const system = ardent.parseSystem(await fetchArdentJson(ardent.systemUrl(station.systemName)));
  if (!system) throw new Error(`Ardent knows station ${station.stationName} but not its system ${station.systemName}`);
  return {
    name: system.name,
    address: system.address,
    coordinates: system.coords,
    via: `station "${station.stationName}"`,
    station: station.stationName,
  };
}

// ---------------------------------------------------------------------------
// Points of interest
//
// The starsystem payload is ~500 KB and the captured logs truncate at 16 KB, so the exact
// shape of its station section is unknown. Rather than guess, walk the tree for anything
// carrying a market id or looking station-like, and report where each hit was found.
// ---------------------------------------------------------------------------

interface PointOfInterest {
  readonly name: string;
  readonly marketId: number | null;
  readonly type: string | null;
  readonly economy: string | null;
  readonly faction: string | null;
  readonly path: string;
}

const MARKET_ID_KEYS = ["marketid", "market_id", "marketids"] as const;
const NAME_KEYS = ["name", "stationname", "station_name", "marketname", "portname", "settlementname"] as const;
const TYPE_KEYS = ["type", "stationtype", "station_type", "porttype", "subtype", "kind"] as const;
const ECONOMY_KEYS = ["economy", "primaryeconomy", "economyname", "economy_name"] as const;
const FACTION_KEYS = ["faction", "controllingfaction", "minorfaction", "owner", "ownername"] as const;

function lowerKeys(record: Record<string, unknown>): Map<string, unknown> {
  const map = new Map<string, unknown>();
  for (const [key, value] of Object.entries(record)) map.set(key.toLowerCase(), value);
  return map;
}

function pickString(keys: readonly string[], fields: Map<string, unknown>): string | null {
  for (const key of keys) {
    const value = fields.get(key);
    if (typeof value === "string" && value.trim() !== "") return value;
  }
  return null;
}

function pickNumber(keys: readonly string[], fields: Map<string, unknown>): number | null {
  for (const key of keys) {
    const value = fields.get(key);
    if (typeof value === "number" && Number.isSafeInteger(value) && value > 0) return value;
  }
  return null;
}

function collectPointsOfInterest(payload: unknown): PointOfInterest[] {
  const found = new Map<string, PointOfInterest>();

  const walk = (value: unknown, path: string, depth: number): void => {
    if (depth > 12) return;
    if (Array.isArray(value)) {
      value.forEach((entry, index) => walk(entry, `${path}[${index}]`, depth + 1));
      return;
    }
    const record = asRecord(value);
    if (!record) return;

    const fields = lowerKeys(record);
    const marketId = pickNumber(MARKET_ID_KEYS, fields);
    const name = pickString(NAME_KEYS, fields);
    const type = pickString(TYPE_KEYS, fields);
    // A market id is proof; otherwise require a name plus a station-ish companion field.
    const looksLikePort = marketId !== null ||
      (name !== null && (fields.has("services") || fields.has("landingpads") || fields.has("dockingaccess") ||
        (type !== null && /station|port|settlement|outpost|carrier|hub|dock/i.test(type))));

    if (looksLikePort && name !== null) {
      const key = marketId !== null ? `market:${marketId}` : `path:${path}:${name}`;
      if (!found.has(key)) {
        found.set(key, {
          name,
          marketId,
          type,
          economy: pickString(ECONOMY_KEYS, fields),
          faction: pickString(FACTION_KEYS, fields),
          path,
        });
      }
    }

    for (const [key, child] of Object.entries(record)) walk(child, path === "" ? key : `${path}.${key}`, depth + 1);
  };

  walk(payload, "", 0);
  return [...found.values()].sort((left, right) => {
    if ((left.marketId === null) !== (right.marketId === null)) return left.marketId === null ? 1 : -1;
    return left.name.localeCompare(right.name);
  });
}

const POI_COLUMNS: readonly TableColumn[] = [
  { key: "marketId", header: "Market ID", align: "right" },
  { key: "name", header: "Name", minWidth: 12, maxWidth: 34 },
  { key: "type", header: "Type", maxWidth: 18, priority: 1 },
  { key: "economy", header: "Economy", maxWidth: 16, priority: 2 },
  { key: "faction", header: "Faction", maxWidth: 24, priority: 3 },
  { key: "path", header: "Found at", maxWidth: 28, priority: 4 },
];

// ---------------------------------------------------------------------------
// Markets in a system
//
// Confirmed shape: starsystem.polities[n].markets[marketId], each entry carrying its own
// `id`, name, poiType/outpostType, imported/exported commodity maps, services, economies,
// bodyName and distFromSystem. The controlling faction resolves through
// starsystem.starsystem.minorFactions[controllingMinorFaction].
// ---------------------------------------------------------------------------

interface MarketPoint {
  readonly marketId: number;
  readonly name: string;
  readonly type: string;
  readonly bodyName: string | null;
  readonly distance: number | null;
  readonly economy: string | null;
  readonly faction: string | null;
  readonly imports: number;
  readonly exports: number;
  readonly services: ReadonlySet<string>;
  readonly marketState: string;
}

/** poiType values seen in live payloads, mapped to something a table can hold. */
const POI_TYPE_LABELS: Readonly<Record<string, string>> = {
  starport: "starport",
  outpost: "outpost",
  dockableplanetstation: "planetary",
  onfootsettlement: "settlement",
  fleetcarrier: "carrier",
  megaship: "megaship",
  gameplaypoi: "poi",
};

/** Display order for the type bands: things you can dock and trade at come first. */
const POI_TYPE_ORDER: readonly string[] = [
  "starport", "outpost", "planetary", "settlement", "megaship", "poi", "carrier",
];

const TRADE_SERVICES: ReadonlyArray<readonly [service: string, flag: string]> = [
  ["commodities", "C"],
  ["blackmarket", "B"],
  ["outfitting", "O"],
  ["shipyard", "Y"],
  ["refuel", "F"],
];

function primaryEconomy(market: Record<string, unknown>): string | null {
  const economies = asRecord(market.economies);
  if (!economies) return null;
  let best: { name: string; proportion: number } | null = null;
  for (const entry of Object.values(economies)) {
    const record = asRecord(entry);
    if (!record) continue;
    const name = readString(record, "name");
    const proportion = readNumber(record, "proportion");
    if (name !== "" && (best === null || proportion > best.proportion)) best = { name, proportion };
  }
  return best === null ? null : best.name;
}

function availableServices(market: Record<string, unknown>): Set<string> {
  const services = asRecord(market.services);
  const available = new Set<string>();
  if (!services) return available;
  for (const [name, state] of Object.entries(services)) {
    if (state === "ok") available.add(name.toLowerCase());
  }
  return available;
}

function countEntries(value: unknown): number {
  const record = asRecord(value);
  return record ? Object.keys(record).length : Array.isArray(value) ? value.length : 0;
}

/**
 * Minor faction ids run past 2^53 (e.g. 72060832334024995), so JSON.parse rounds the value
 * while the map's keys keep every digit — an exact string match misses. Compare the rounded
 * forms instead, which is exact enough to be unambiguous within one system.
 */
function lookupFaction(factions: Record<string, unknown>, id: unknown): Record<string, unknown> | null {
  if (typeof id !== "number" && typeof id !== "string") return null;
  const direct = asRecord(factions[String(id)]);
  if (direct) return direct;
  const wanted = Number(id);
  if (!Number.isFinite(wanted)) return null;
  for (const [key, value] of Object.entries(factions)) {
    if (Number(key) === wanted) return asRecord(value);
  }
  return null;
}

function readMarketPoints(payload: Record<string, unknown>): MarketPoint[] {
  const outer = asRecord(payload.starsystem);
  const polities = outer && asRecord(outer.polities);
  if (!polities) return [];
  const core = outer && asRecord(outer.starsystem);
  const factions = core && asRecord(core.minorFactions);
  const points: MarketPoint[] = [];

  for (const polityValue of Object.values(polities)) {
    const polity = asRecord(polityValue);
    const markets = polity && asRecord(polity.markets);
    if (!polity || !markets) continue;

    const factionRecord = factions ? lookupFaction(factions, polity.controllingMinorFaction) : null;
    const faction = factionRecord ? readString(factionRecord, "name") : "";

    for (const [key, value] of Object.entries(markets)) {
      const market = asRecord(value);
      if (!market) continue;
      const marketId = readNumber(market, "id") || Number(key);
      if (!Number.isSafeInteger(marketId) || marketId <= 0) continue;

      const distance = readNumber(market, "distFromSystem");
      const bodyName = readString(market, "bodyName");
      points.push({
        marketId,
        name: readString(market, "name") || `market ${marketId}`,
        type: POI_TYPE_LABELS[readString(market, "poiType").toLowerCase()] ??
          (readString(market, "poiType") || readString(market, "outpostType") || "unknown"),
        bodyName: bodyName === "" ? null : bodyName,
        distance: distance > 0 ? distance : null,
        economy: primaryEconomy(market),
        faction: faction === "" ? null : faction,
        imports: countEntries(market.imported),
        exports: countEntries(market.exported),
        services: availableServices(market),
        marketState: readString(market, "market_state"),
      });
    }
  }
  return points;
}

const MARKET_POINT_COLUMNS: readonly TableColumn[] = [
  { key: "marketId", header: "Market ID", align: "right" },
  { key: "name", header: "Name", minWidth: 14, maxWidth: 36 },
  { key: "services", header: "CBOYF", priority: 1 },
  { key: "imports", header: "Imp", align: "right", priority: 1 },
  { key: "exports", header: "Exp", align: "right", priority: 1 },
  { key: "distance", header: "Dist (Ls)", align: "right", priority: 2 },
  { key: "economy", header: "Economy", maxWidth: 14, priority: 3 },
  { key: "faction", header: "Faction", maxWidth: 26, priority: 4 },
  { key: "body", header: "Body", maxWidth: 22, priority: 5 },
];

function emitMarketPoints(points: readonly MarketPoint[], title: string): void {
  const groups = new Map<string, MarketPoint[]>();
  for (const point of points) {
    const bucket = groups.get(point.type);
    if (bucket) bucket.push(point);
    else groups.set(point.type, [point]);
  }

  const ordered = [...groups.keys()].sort((left, right) => {
    const leftRank = POI_TYPE_ORDER.indexOf(left);
    const rightRank = POI_TYPE_ORDER.indexOf(right);
    return (leftRank === -1 ? POI_TYPE_ORDER.length : leftRank) - (rightRank === -1 ? POI_TYPE_ORDER.length : rightRank) ||
      left.localeCompare(right);
  });

  const rows: TableRow[] = [];
  for (const type of ordered) {
    const bucket = groups.get(type)!.sort((left, right) => {
      if (left.distance !== right.distance) return (left.distance ?? Infinity) - (right.distance ?? Infinity);
      return left.name.localeCompare(right.name);
    });
    const trading = bucket.filter((point) => point.imports > 0 || point.exports > 0).length;
    rows.push(bandRow(`${type.toUpperCase()}  ${bucket.length} | ${trading} with a commodity market`));
    for (const point of bucket) {
      rows.push({
        kind: "data",
        cells: {
          marketId: String(point.marketId),
          name: point.name,
          services: TRADE_SERVICES.map(([service, flag]) => formatFlag(point.services.has(service), flag)).join(""),
          imports: formatQuantity(point.imports),
          exports: formatQuantity(point.exports),
          distance: point.distance === null ? "-" : formatInteger(Math.round(point.distance)),
          economy: point.economy ?? "-",
          faction: point.faction ?? "-",
          body: point.bodyName ?? "-",
        },
      });
    }
  }

  emitTable(title, MARKET_POINT_COLUMNS, rows);
  emitNote("CBOYF = Commodities/Blackmarket/Outfitting/shipYard/reFuel service available | Imp,Exp = commodities traded");
}

/**
 * Best-effort decode of a non-2xx body: still ChaCha20, but it may skip the EDDE/LZ4 framing
 * and the size header. Returns null when nothing legible comes out.
 */
function decodeOpaqueBody(raw: string, nonce: string, sizeHeader: string | null): string | null {
  const compact = raw.trim();
  if (compact === "" || !/^[A-Za-z0-9+/]*={0,2}$/.test(compact) || compact.length % 4 !== 0) return null;
  if (!/^[0-9a-fA-F]{12}$/.test(nonce)) return null;

  try {
    const decrypted = chacha20(new Uint8Array(Buffer.from(compact, "base64")), CHACHA_KEY, encoder.encode(nonce));
    const framed = decrypted.length >= 8 && decrypted[0] === 0x45 && decrypted[1] === 0x44 &&
      decrypted[2] === 0x44 && decrypted[3] === 0x45;
    const decoder = new TextDecoder("utf-8", { fatal: true });
    if (!framed) return decoder.decode(decrypted);

    const size = Number(sizeHeader);
    if (!Number.isSafeInteger(size) || size <= 0) return null;
    return decoder.decode(decompressLz4Block(decrypted.subarray(8), size));
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// EDDN submission
//
// Built against /models/dev/EDDN commodity-v3.0.json. The message shape is strict:
// `message` sets additionalProperties:false, and `id`, `Producer` and `Rare` inside a
// commodity are mapped to a `disallowed` definition that matches no JSON value at all, so
// including any of them is a hard validation failure rather than a warning.
// ---------------------------------------------------------------------------

interface EddnStation {
  readonly systemName: string;
  readonly stationName: string;
  readonly stationType: string | null;
  readonly economies: ReadonlyArray<{ readonly name: string; readonly proportion: number }> | null;
}

interface EddnOptions {
  readonly test: boolean;
  readonly uploaderId: string;
  readonly softwareName: string;
  readonly softwareVersion: string;
  readonly gameVersion: string;
  readonly gameBuild: string;
  readonly horizons: boolean | undefined;
  readonly odyssey: boolean | undefined;
}

function loadEddnOptions(args: ParsedArguments, credentials: Credentials): EddnOptions {
  return {
    test: switchValue(args, "eddntest", false),
    // "preferably simply the relevant in-game Commander name" — the id is what we have.
    uploaderId: optionalValue(args, "uploader") ?? credentials.commanderId,
    softwareName: optionalValue(args, "softwarename") ?? EDDN_SOFTWARE_NAME,
    softwareVersion: optionalValue(args, "softwareversion") ?? EDDN_SOFTWARE_VERSION,
    gameVersion: optionalValue(args, "gameversion") ?? EDDN_GAME_VERSION,
    gameBuild: optionalValue(args, "gamebuild") ?? "",
    horizons: optionalSwitch(args, "horizons"),
    odyssey: optionalSwitch(args, "odyssey"),
  };
}

/**
 * commodity-README.md:48-52 — skip NonMarketable goods and anything with a non-empty
 * legality string. Names are lowercased to the symbol form EDDN carries: the game gives
 * `AgronomicTreatment`, journal senders give `$agronomictreatment_name;` -> the same
 * lowercase token, which is what downstream consumers index on.
 */
function eddnCommodities(commodities: readonly Commodity[]): unknown[] {
  const rows: unknown[] = [];
  for (const commodity of commodities) {
    if (commodity.category === "NonMarketable" || commodity.illegal) continue;
    rows.push({
      name: commodity.name.toLowerCase(),
      meanPrice: commodity.meanPrice,
      buyPrice: commodity.buyPrice,
      stock: commodity.stock,
      stockBracket: commodity.stockBracket,
      sellPrice: commodity.sellPrice,
      demand: commodity.demand,
      demandBracket: commodity.demandBracket,
    });
  }
  return rows;
}

function buildEddnMessage(
  station: EddnStation,
  marketId: number,
  commodities: readonly Commodity[],
  timestamp: string,
  options: EddnOptions,
): { readonly payload: unknown; readonly count: number } {
  const rows = eddnCommodities(commodities);
  const message: Record<string, unknown> = {
    systemName: station.systemName,
    stationName: station.stationName,
    marketId,
    timestamp,
    commodities: rows,
  };
  if (station.stationType !== null && station.stationType !== "") message.stationType = station.stationType;
  // "You MUST NOT send empty lists" — omit rather than send [].
  if (station.economies !== null && station.economies.length > 0) message.economies = station.economies;
  if (options.horizons !== undefined) message.horizons = options.horizons;
  if (options.odyssey !== undefined) message.odyssey = options.odyssey;

  return {
    count: rows.length,
    payload: {
      $schemaRef: options.test ? `${EDDN_SCHEMA}/test` : EDDN_SCHEMA,
      header: {
        uploaderID: options.uploaderId,
        softwareName: options.softwareName,
        softwareVersion: options.softwareVersion,
        gameversion: options.gameVersion,
        gamebuild: options.gameBuild,
      },
      message,
    },
  };
}

interface EddnResult {
  readonly ok: boolean;
  readonly status: number | null;
  readonly detail: string;
  readonly commodities: number;
}

/**
 * One POST per market. Failures are reported and never retried inside a run: the spec
 * requires a minimum one minute wait, and forbids retrying 400 or 426 at all.
 */
async function submitToEddn(payload: unknown, count: number, signal?: AbortSignal): Promise<EddnResult> {
  try {
    const response = await fetch(EDDN_UPLOAD_URL, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
      signal,
    });
    const body = (await response.text()).trim();
    const ok = response.status === 200 && body === "OK";
    return {
      ok,
      status: response.status,
      detail: ok ? "OK" : `${response.status} ${clampText(body, 120)}`,
      commodities: count,
    };
  } catch (error) {
    return { ok: false, status: null, detail: error instanceof Error ? error.message : String(error), commodities: count };
  }
}

/** Ardent is the only route from a bare market id back to the names EDDN requires. */
async function resolveStationByMarketId(marketId: number): Promise<EddnStation | null> {
  try {
    const record = asRecord(await fetchArdentJson(ARDENT_MARKET_URL(marketId)));
    if (!record) return null;
    const systemName = readString(record, "systemName");
    const stationName = readString(record, "stationName");
    if (systemName === "" || stationName === "") return null;
    const stationType = readString(record, "stationType");
    return { systemName, stationName, stationType: stationType === "" ? null : stationType, economies: null };
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// Request pacing
// ---------------------------------------------------------------------------


// ---------------------------------------------------------------------------
// markets
// ---------------------------------------------------------------------------

async function runMarkets(session: Session): Promise<void> {
  const args = session.args;
  const explicitAddress = optionalNumber(args, "address");
  const stationName = optionalValue(args, "station");
  const systemName = optionalValue(args, "system");
  const positional = args.positionals.join(" ").trim();
  const name = stationName ?? systemName ?? (positional === "" ? undefined : positional);

  let resolved: ResolvedSystem | null = null;
  let address: number;

  if (explicitAddress !== undefined) {
    address = explicitAddress;
  } else {
    if (name === undefined) throw new Error("markets needs a system or station name (or --address <id64>)");
    if (!session.json) emitNote(`resolving "${name}" through Ardent...`);
    resolved = await resolveLocation(name, stationName !== undefined ? "station" : systemName !== undefined ? "system" : "auto");
    address = resolved.address;
  }

  // Cross-check the address against the packing algorithm rather than trusting either alone.
  const parts = decodeSystemAddress(address);
  const rows: TableRow[] = [
    fieldRow("system", resolved?.name ?? `address ${address}`),
    ...(resolved === null ? [] : [fieldRow("resolved via", resolved.via)]),
    fieldRow("systemAddress", String(address)),
    fieldRow("mass code", `${parts.massCodeLetter} (${parts.massCode}), boxel ${parts.boxelSize} ly`),
    fieldRow("sector", `${parts.sector.x} / ${parts.sector.y} / ${parts.sector.z}`),
    fieldRow("boxel", `${parts.boxel.x} / ${parts.boxel.y} / ${parts.boxel.z}, index ${parts.index}`),
    fieldRow("boxel origin", `${parts.origin.x} / ${parts.origin.y} / ${parts.origin.z}`),
  ];

  if (resolved !== null) {
    const { coordinates } = resolved;
    rows.push(fieldRow("coordinates", `${coordinates.x} / ${coordinates.y} / ${coordinates.z}`));
    const inside = containsCoordinates(parts, coordinates);
    const repacked = encodeSystemAddress(coordinates, parts.massCode, parts.index);
    rows.push(fieldRow("coords in boxel", inside ? "yes" : "NO — address and coordinates disagree"));
    rows.push(fieldRow("repacked address", `${repacked}${repacked === address ? " (round-trips)" : " (MISMATCH)"}`));
  }

  if (!session.json) emitTable("SYSTEM", FIELD_COLUMNS, rows);

  const language = optionalValue(args, "language") ?? "en";
  const cachedTimeStamp = optionalNumber(args, "cachedtimestamp") ?? 0;
  const stamp = nextStamp(args);
  const request = prepareRequest(
    session,
    STARSYSTEM,
    starsystemEnvelopeFields(address, language, cachedTimeStamp, session.credentials, stamp.frontierTime),
    stamp,
  );
  const exchange = await send(session, request, { quiet: session.json });

  if (session.json) {
    emitJson(request, exchange, { system: resolved, address, addressParts: parts });
    return;
  }
  if (!exchange?.decrypted) return;

  const dumpPath = optionalValue(args, "dump");
  if (dumpPath !== undefined) {
    writeFileSync(dumpPath, exchange.decrypted);
    emitNote(`wrote ${formatInteger(exchange.decrypted.length)} bytes of starsystem payload to ${dumpPath}`);
  }

  let payload: unknown;
  try {
    payload = JSON.parse(exchange.decrypted);
  } catch {
    emitOpaquePayload(exchange.decrypted);
    return;
  }

  const record = asRecord(payload);
  const all = record ? readMarketPoints(record) : [];

  if (all.length === 0) {
    // Shape drift: fall back to sniffing the tree for anything station-like.
    const guessed = collectPointsOfInterest(payload);
    if (guessed.length === 0) {
      emitNote("no markets found under starsystem.polities; pass --dump <file> to inspect the payload");
      return;
    }
    emitNote("starsystem.polities held no markets — falling back to a structural scan");
    emitTable(
      `POINTS OF INTEREST  ${guessed.length} found by scan`,
      POI_COLUMNS,
      guessed.map((point) => ({
        kind: "data" as const,
        cells: {
          marketId: point.marketId === null ? "-" : String(point.marketId),
          name: point.name,
          type: point.type ?? "-",
          economy: point.economy ?? "-",
          faction: point.faction ?? "-",
          path: point.path,
        },
      })),
    );
    return;
  }

  const includeCarriers = switchValue(args, "carriers", false);
  const tradingOnly = switchValue(args, "trading", false);
  let points = all;
  const hiddenCarriers = includeCarriers ? 0 : points.filter((point) => point.type === "carrier").length;
  if (!includeCarriers) points = points.filter((point) => point.type !== "carrier");
  const hiddenIdle = tradingOnly ? points.filter((point) => point.imports === 0 && point.exports === 0).length : 0;
  if (tradingOnly) points = points.filter((point) => point.imports > 0 || point.exports > 0);

  if (points.length === 0) {
    emitNote(`all ${all.length} markets were filtered out; drop --trading or add --carriers`);
    return;
  }

  emitMarketPoints(
    points,
    `MARKETS  ${points.length} of ${all.length} in ${resolved?.name ?? `address ${address}`}` +
      `${resolved?.station ? ` (asked about ${resolved.station})` : ""}`,
  );

  const skipped: string[] = [];
  if (hiddenCarriers > 0) skipped.push(`${hiddenCarriers} fleet carriers hidden (--carriers to show)`);
  if (hiddenIdle > 0) skipped.push(`${hiddenIdle} without a commodity market hidden by --trading`);
  if (skipped.length > 0) emitNote(skipped.join(" | "));

  const target = resolved?.station
    ? points.find((point) => point.name.toLowerCase() === resolved!.station!.toLowerCase())
    : undefined;
  emitNote(
    target
      ? `${target.name}: list --market-id ${target.marketId}   or   trade --market-id ${target.marketId} --type buy --item <name> --qty <n>`
      : `feed a market id to: list --market-id <id>   or   trade --market-id <id> --type buy --item <name> --qty <n>`,
  );
}

async function main(): Promise<void> {
  let args: ParsedArguments;
  try {
    args = parseArguments(process.argv.slice(2));
  } catch (error) {
    console.error(`${error instanceof Error ? error.message : String(error)}\n`);
    console.log(USAGE);
    process.exitCode = 2;
    return;
  }

  if (args.command === "help" || switchValue(args, "help", false)) {
    console.log(USAGE);
    return;
  }
  const known = new Set(["market", "list", "markets", "trade"]);
  if (!known.has(args.command)) {
    console.error(`Unknown command "${args.command}"\n`);
    console.log(USAGE);
    process.exitCode = 2;
    return;
  }

  try {
    const session = openSession(args);
    if (args.command === "trade") await runTrade(session);
    else if (args.command === "markets") await runMarkets(session);
    else await runMarket(session);
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  }
}

await main();
