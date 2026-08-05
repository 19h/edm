// Blesses tests/snapshots/render__*.snap from Bun.
//
// Lines 358-495 of market-request.ts, verbatim, plus the eight column sets,
// plus a driver that renders render_scenarios.json at the width in $COLUMNS.
// The Rust test reads the same scenario file, so the two sides cannot drift
// apart in their input; what comes out here is what the snapshots must contain.
//
//   for w in 48 60 80 100 200; do
//     COLUMNS=$w bun crates/edm-core/tests/fixtures/render_oracle.ts \
//       crates/edm-core/tests/fixtures/render_scenarios.json /tmp/blessed
//   done
//
// Each output file is `<scenario>.<width>.txt` and must equal the body of
// `tests/snapshots/render__<scenario>_w<width>.snap` byte for byte.
import { mkdirSync, writeFileSync } from "node:fs";

type CellAlignment = "left" | "right";

interface TableColumn {
  readonly key: string;
  readonly header: string;
  readonly align?: CellAlignment;
  readonly priority?: number;
  readonly minWidth?: number;
  readonly maxWidth?: number;
}

type TableRow =
  | { readonly kind: "data"; readonly cells: Readonly<Record<string, string>> }
  | { readonly kind: "band"; readonly text: string }
  | { readonly kind: "rule" };

// ---- verbatim from market-request.ts:358-495 --------------------------------

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

function emitNote(text: string, log: (line: string) => void): void {
  const indent = "   ";
  const limit = Math.max(20, TERMINAL_WIDTH - indent.length);
  let line = "";
  for (const word of text.split(" ")) {
    if (line === "") line = word;
    else if (line.length + 1 + word.length <= limit) line += ` ${word}`;
    else {
      log(`${indent}${line}`);
      line = word;
    }
  }
  if (line !== "") log(`${indent}${line}`);
}

function emitTable(
  title: string,
  columns: readonly TableColumn[],
  rows: readonly TableRow[],
  log: (line: string) => void,
): void {
  log(heading(title));
  const { lines, omitted } = renderTable(columns, rows);
  for (const line of lines) log(line);
  if (omitted.length > 0) {
    emitNote(`columns hidden to fit ${TERMINAL_WIDTH} cols: ${omitted.join(", ")}`, log);
  }
}

// ---- the eight column sets, verbatim ----------------------------------------

const COLUMN_SETS: Record<string, readonly TableColumn[]> = {
  FIELD: [
    { key: "field", header: "Field", minWidth: 8, maxWidth: 22 },
    { key: "value", header: "Value", minWidth: 12 },
  ],
  COMMODITY: [
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
  ],
  INVENTORY: [
    { key: "commodity", header: "Commodity", minWidth: 10, maxWidth: 30 },
    { key: "qty", header: "Qty", align: "right" },
    { key: "value", header: "Value", align: "right" },
    { key: "stolen", header: "S" },
    { key: "marked", header: "Marked", align: "right", priority: 3 },
    { key: "owner", header: "Owner", align: "right", priority: 2 },
    { key: "origin", header: "Origin", align: "right", priority: 2 },
    { key: "position", header: "Position (x / y / z)", priority: 1 },
  ],
  SWEEP: [
    { key: "marketId", header: "Market ID", align: "right" },
    { key: "name", header: "Name", minWidth: 12, maxWidth: 32 },
    { key: "status", header: "HTTP", align: "right" },
    { key: "commodities", header: "Comm", align: "right" },
    { key: "supplied", header: "Sup", align: "right", priority: 2 },
    { key: "demanded", header: "Dem", align: "right", priority: 2 },
    { key: "eddn", header: "EDDN", priority: 1 },
    { key: "attempts", header: "Try", align: "right", priority: 3 },
  ],
  PLAN: [
    { key: "field", header: "Field", minWidth: 8, maxWidth: 20 },
    { key: "value", header: "Value", minWidth: 10 },
    { key: "source", header: "From", priority: 1 },
  ],
  TRADE_LOG: [
    { key: "round", header: "#", align: "right" },
    { key: "commodity", header: "Commodity", minWidth: 10, maxWidth: 28 },
    { key: "qty", header: "Qty", align: "right" },
    { key: "unitPrice", header: "Unit", align: "right" },
    { key: "total", header: "Total", align: "right" },
    { key: "status", header: "HTTP", align: "right", priority: 1 },
    { key: "cargo", header: "Cargo", align: "right", priority: 2 },
  ],
  POI: [
    { key: "marketId", header: "Market ID", align: "right" },
    { key: "name", header: "Name", minWidth: 12, maxWidth: 34 },
    { key: "type", header: "Type", maxWidth: 18, priority: 1 },
    { key: "economy", header: "Economy", maxWidth: 16, priority: 2 },
    { key: "faction", header: "Faction", maxWidth: 24, priority: 3 },
    { key: "path", header: "Found at", maxWidth: 28, priority: 4 },
  ],
  MARKET_POINT: [
    { key: "marketId", header: "Market ID", align: "right" },
    { key: "name", header: "Name", minWidth: 14, maxWidth: 36 },
    { key: "services", header: "CBOYF", priority: 1 },
    { key: "imports", header: "Imp", align: "right", priority: 1 },
    { key: "exports", header: "Exp", align: "right", priority: 1 },
    { key: "distance", header: "Dist (Ls)", align: "right", priority: 2 },
    { key: "economy", header: "Economy", maxWidth: 14, priority: 3 },
    { key: "faction", header: "Faction", maxWidth: 26, priority: 4 },
    { key: "body", header: "Body", maxWidth: 22, priority: 5 },
  ],
};

// ---- driver ------------------------------------------------------------------

const [, , fixturePath, outDir] = process.argv;
const scenarios = JSON.parse(await Bun.file(fixturePath!).text()) as Array<{
  name: string;
  columns: string;
  title: string;
  rows: Array<{ kind: string; cells?: string[]; text?: string }>;
}>;
mkdirSync(outDir!, { recursive: true });

for (const scenario of scenarios) {
  const columns = COLUMN_SETS[scenario.columns]!;
  const rows: TableRow[] = scenario.rows.map((row) => {
    if (row.kind === "rule") return { kind: "rule" };
    if (row.kind === "band") return { kind: "band", text: row.text! };
    const cells: Record<string, string> = {};
    columns.forEach((column, index) => {
      const value = row.cells![index];
      if (value !== undefined) cells[column.key] = value;
    });
    return { kind: "data", cells };
  });
  const out: string[] = [];
  emitTable(scenario.title, columns, rows, (line) => out.push(line));
  writeFileSync(`${outDir}/${scenario.name}.${TERMINAL_WIDTH}.txt`, out.map((l) => `${l}\n`).join(""));
}
