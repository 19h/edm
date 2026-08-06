/**
 * The Bun shim for `cargo xtask parity`.
 *
 *   bun --preload xtask/oracle/preload.ts game-internal-api.ts <argv...>
 *
 * It does two things and may never do a third:
 *
 *   1. rewrites `fetch` URLs to `edm-mock`, preserving path and query;
 *   2. freezes `Date`, which pins the EDDN message timestamp.
 *
 * It does **not** modify `game-internal-api.ts`. The whole value of the harness is
 * that the thing being measured is the original, unedited; a shim that patched
 * the program would be comparing the Rust against a fork.
 *
 * Everything else that could differ between two runs is pinned by the original's
 * own flags — `--nonce`, `--f-time` and `--request-time` make the encrypted
 * query byte-identical, and `COLUMNS` pins `TERMINAL_WIDTH`.
 *
 * The origin is *not* rewritten in the printed output, because the program
 * prints `API_ORIGIN` (ts:1181) rather than the URL it fetched. The Rust side,
 * which takes its origin from `EDM_ORIGIN_OVERRIDE`, prints the override — the
 * runner canonicalises that back before diffing.
 */

const base = process.env["EDM_MOCK_BASE"];
if (!base) {
  // Refusing loudly rather than falling through: without this the run would
  // send real credentials to the real game-internal API.
  throw new Error("preload.ts: EDM_MOCK_BASE is not set — refusing to let fetch reach the network");
}

/**
 * `${base}${path}${query}`.
 *
 * String surgery rather than `new URL`, because the game-internal API query is
 * standard base64 appended raw (R64) and a URL round-trip is entitled to
 * re-encode `+`, `/` and `=`. The bytes on the wire are the thing under test.
 */
function toMock(url: string): string {
  const scheme = url.indexOf("://");
  if (scheme < 0) return url;
  const slash = url.indexOf("/", scheme + 3);
  return base + (slash < 0 ? "/" : url.slice(slash));
}

const realFetch = globalThis.fetch;
globalThis.fetch = function fetch(input: RequestInfo | URL, init?: RequestInit): Promise<Response> {
  if (typeof input === "string") return realFetch(toMock(input), init);
  if (input instanceof URL) return realFetch(toMock(input.href), init);
  return realFetch(new Request(toMock(input.url), input), init);
} as typeof globalThis.fetch;

const frozen = process.env["EDM_MOCK_NOW"];
if (frozen) {
  const fixed = Number(frozen);
  const RealDate = Date;
  class FrozenDate extends RealDate {
    constructor(...args: ConstructorParameters<typeof Date>) {
      // Only the no-argument form is the clock; `new Date(ms)` must still mean
      // what it says, and `formatUnixSeconds` depends on it.
      if (args.length === 0) super(fixed);
      else super(...args);
    }
    static override now(): number {
      return fixed;
    }
  }
  globalThis.Date = FrozenDate as DateConstructor;
}
