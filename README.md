# edm

Turn live Elite Dangerous market data into routes you can fly, trades you can
execute, and commodity updates you can share with the community.

## Find profitable trade routes

`edm route` surveys the markets around a system or station and ranks the best
one-way trips, round trips, and repeatable loops. Results say where to go, what
to carry, expected profit, travel time, and credits per hour. They also include
ready-to-run `edm trade` commands for each leg.

Shape the search around the ship and the trip you want to fly:

- Set cargo capacity, available credits, laden jump range, and landing-pad size.
- Limit distance from the arrival star, supply, demand, commodity category, or
  station type.
- Exclude carriers, settlements, or illegal commodities unless you explicitly
  want them considered.
- Search a single hop, a round trip, the best loop of any length, or a loop with
  a fixed maximum number of stops.
- See whether a result is proven optimal or affected by estimates, incomplete
  coverage, or a search deadline.

```bash
edm route Sol
edm route "Shinrarta Dezhra" --radius 50 --cargo 784 --shape loop
edm route Colonia --cargo 1232 --credits 500000000 --shape loop:4 --yes
edm route Sol --radius 15 --dry-run
```

The route planner can use local Journal, Status, and Cargo files to pick up your
current system, free cargo space, balance, and jump range. Command-line values
always take priority.

## How a route search works

The complete route-search path is:

`reference system -> Ardent discovery -> market filters -> cache check -> request plan -> live prices -> optional EDDN relay -> route ranking`

1. **Resolve the reference.** The supplied system or station name is resolved
   through Ardent to a system, address, and galactic coordinates. When no
   reference is supplied, local commander state can provide the current system.

2. **Discover the region.** Ardent is queried for systems inside the requested
   radius and for the markets in each system. If a nearby-system response hits
   Ardent's result limit, `edm` continues from additional systems until it has
   covered the radius or exhausted the configured discovery budget. The output
   reports how far the resulting survey is known to be complete.

3. **Reduce the candidate set.** Stations and markets are filtered before live
   price requests are made. This applies constraints such as landing-pad size,
   distance from the arrival star, station type, carriers, and settlements.
   Slowly changing Ardent system and station data is cached locally so repeated
   searches do not need to rediscover the same region from scratch.

4. **Check the price cache and price the run.** Every remaining market is
   checked for a fresh, quantity-aware listing in the local cache. Cache hits
   are included in the search without another request. Missing, stale, or
   corrupt entries become live polling jobs. Before each authenticated Frontier
   phase, `edm` prints its measured request count and estimated cost;
   `--dry-run`, `--max-requests`, and the `--yes` confirmation gate can stop the
   run before that work begins.

5. **Fetch authoritative market prices.** Uncached markets are polled from the
   Frontier game APIs with authenticated requests. The sweep observes the
   configured concurrency, rate, retry, and deadline limits, backs off when the
   service asks it to, and stops escalating failures if its circuit breaker
   trips. `--verify-systems` adds separately gated official topology and bulk
   market-data checks before the exact per-market reads.

6. **Cache and optionally share each fresh listing.** Every valid live market
   response is written to the price cache immediately, so an interrupted sweep
   still preserves completed work. With `--eddn` or `--eddn-test`, that fresh
   listing is also converted to an EDDN commodity message and submitted through
   a separate rate limiter. Listings loaded from the price cache are never
   relayed, because doing so would present an older observation as newly read;
   markets relayed recently by this machine are suppressed as well.

7. **Build and rank flyable routes.** The collected listings are converted into
   a trade graph, constrained by the selected ship, balance, supply, demand,
   commodity, and station rules, then searched for the requested route shape.
   The final output includes ranked routes, ready-to-run trade commands, and a
   coverage report showing cache hits, successful and failed market reads,
   EDDN activity, incomplete discovery, and whether the result was proven or
   cut short by the deadline.

The default cache lives under `$XDG_CACHE_HOME/edm/route` (or
`$HOME/.cache/edm/route`). Use `--max-age` to control price freshness,
`--refresh` to poll again while warming the cache with new results, or
`--no-cache` to bypass both cache reads and writes.

## Inspect markets and trading locations

Read one market's commodity listing, sweep every trading market in a system, or
resolve a system or station name to the dockable markets around it.

```bash
edm market --market-id 4306502403
edm market Colonia --detail
edm markets "Hyades Sector NI-X a16-0" --trading
edm markets --station "Jaques Station"
```

Market sweeps can run concurrently, retry transient failures, include carriers
or non-trading markets on request, and return either fitted terminal tables or
JSON for downstream tools.

## Find Pioneer Supplies stock

`edm vendor` reads Frontier's market-scoped vendor endpoint and locates suits and
weapons offered across a system. Ardent supplies the station-to-market mapping;
a station match that includes a market ID checks only that station, while a
system match checks every non-carrier market in the system.

With no target, the command reads the latest local Elite Dangerous journal and
searches the commander's current system.

```bash
edm vendor
edm vendor Sol
edm vendor Sol --min-level 3
edm vendor --station "Jaques Station"
edm vendor --market-id 4370953219
edm vendor Colonia --json
```

The default output includes in-stock upgraded offers and ordinary grade-1
outfitting. Use `--min-level N` (or `--min-grade N`) to retain only grade N and
higher. Add `--detail` to retain sold-out premium slots and Frontier's raw
prototype names. JSON output also preserves each decoded vendor payload for
shape inspection and downstream tools.

## Execute controlled trades

Buy or sell by commodity name or ID. `edm` can look up the current price, clamp
orders to stock, holdings, cargo space, or available credits, and show the
resolved request before sending it.

```bash
edm trade --market-id 4306502403 --type buy --item silver --qty 10
edm trade --type sell --item 128049155 --qty 5 --unit-price 3340 --stolen
edm trade --type buy --item palladium,gold --cargo 1232 --fill
edm trade --type buy --item palladium,gold --cargo 1232 --fill --watch
```

Batch trades work through a commodity list in order. Fill mode can spend the
remaining hold space across that list, while watch mode can repeat attempts at
a chosen interval until the hold is full or an attempt limit is reached.

## Publish fresh prices to EDDN

Share market data with the [Elite Dangerous Data
Network](https://github.com/EDCD/EDDN) while sweeping a system or route, or run a
dedicated refresh for selected markets and systems.

```bash
edm market Colonia --eddn
edm eddn market --market-id 4306502403
edm eddn market --from-file stale-systems.txt
edm eddn market --from-file stale-systems.txt --dry-run
```

A refresh file accepts one market ID or system name per line. Repeats are
removed, comments are allowed, and systems expand to all of their markets.
Uploads are separately paced, recently relayed markets are suppressed, and
`--eddn-test` exercises EDDN's test schema without relaying the data onward.

## Automate safely

- Use `--json` for machine-readable market and route output.
- Use `--dry-run` to inspect requests or price a regional survey before it
  starts.
- Bound large jobs with request ceilings, deadlines, rate limits, and explicit
  confirmation.
- Reuse recent route data from the local cache, or force a refresh when current
  prices matter more.
- Add `--verbose` to see throttling, retries, pacing changes, and early-stop
  reasons.

`edm` can execute trades against a live commander account. Start with
`--dry-run`, especially when testing trade, sweep, or publishing options.

## Credentials

Provide credentials as flags or environment variables:

| Flag | Environment variable | Requirement |
|---|---|---|
| `--cmdr-id` | `COMMANDER_ID` | Commander ID |
| `--machine-id` | `MACHINE_ID` | Machine ID |
| `--machine-token` | `MACHINE_TOKEN` | Exactly 80 characters |
| `--auth-token` | `AUTH_TOKEN` | Exactly 2024 characters |

Run `edm help`, `edm route --help`, `edm vendor --help`, or `edm eddn --help` for
the complete option reference.
