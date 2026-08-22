# Mission API research

## Request

> “Do you by any chance see any game api call in the logs for missions?”

Record whether the captured Elite Dangerous traffic exposes an API call that
could list missions, especially offers currently available from a mission board.
This note preserves the evidence for a possible future `edm` mission feature; it
does **not** claim that a usable mission-list endpoint has been identified.

The client-binary path from `missionsservers` provisioning through UDP/fNet,
encryption, reliable-letter delivery, and in-memory offer storage is documented
in [`MISSION_TRANSPORT_RE.md`](MISSION_TRANSPORT_RE.md).

## Source and handling

The observations below come from the instrumented Odyssey `out2.log` captured
on 2026-08-07. Line numbers refer to that capture and are only evidence anchors;
the log is external to this repository.

The capture contains live account credentials, machine tokens, server addresses,
and session encryption material. None of their values may be copied into source,
fixtures, issues, or documentation. In particular, the values returned inside
`missionsservers` must remain redacted.

## Findings

No captured game-API endpoint has `mission` in its URL path. Mission-related
information is nevertheless visible through several generic endpoints:

| Endpoint | Evidence | What it contains | What it does not contain |
|---|---|---|---|
| `2.0/elite/server/list` | Decoded responses at lines 1076, 2403, and 7830 | A `missionsservers` list. Each entry has the keys `id`, `ip`, `port`, `runId`, `connectionDetails`, and `encryptionKey`. | No mission offers. The connection values are sensitive and intentionally omitted. |
| `2.0/elite/event` | Mission-bearing requests include lines 55, 292, 1390, 1581, and 3826 | Aggregate telemetry: `numMissions`, sometimes `marketID`, theme counts, and archetype counts such as `OF_MB_Collect`, `OF_MB_Heist_*`, and `OF_NPC_*`. | No per-offer identifier, faction, destination, expiry, reward, or requirements. |
| `2.0/elite/commander/reputation` | Requests such as line 3692 use `trigger=MissionsUpdate` | The decoded response contains reputation, rank, merit, and progression data. | No mission-board offers or accepted-mission records. |
| `2.0/elite/starsystem` | Decoded response at line 1061 and later repeats | Market metadata can contain `services.missions`, `services.missionsgenerated`, and `missions: {"target": bool}`. | No offer list. These fields appear to describe service availability and/or whether a location is a current mission target. |
| `2.0/elite/journal` | Mission lifecycle records occur between lines 1457 and 28967 | Uploaded journal events: `Missions`, `MissionAccepted`, `MissionRedirected`, `MissionCompleted`, and `MissionAbandoned`. | No list of unaccepted offers currently on a board. |
| `2.0/elite/inbox` and `2.0/elite/inbox/read` | Repeated decoded inbox responses; one read response contains `message.missionId` | Messages associated with missions already known to the commander. | No mission-board catalogue. |
| `2.0/elite/commander/savegame` and inventory/loadout calls | Several decoded responses contain `missionId` on backpack or ship-locker items/data | Mission association for carried or stored items. | No mission definitions or available offers. |

### Counts in this capture

- Nine `2.0/elite/event` requests with event id `3731858691` report
  `numMissions` plus `theme_*` totals.
- Sixteen requests with event id `2583128589` report `numMissions`, usually a
  `marketID`, and on-foot mission-board (`OF_MB_*`) or NPC (`OF_NPC_*`) totals.
- Forty-three `2.0/elite/commander/reputation` requests use
  `trigger=MissionsUpdate`.
- The uploaded journal contains four `MissionAccepted`, three
  `MissionRedirected`, three `MissionCompleted`, one `MissionAbandoned`, and one
  `Missions` event in the observed request batches.

## Interpretation

The strongest lead is `missionsservers` in `2.0/elite/server/list`. The HTTP API
appears to provide connection material for a separate mission service, while
`2.0/elite/event` reports aggregate telemetry after mission data has been
presented. This suggests that the actual board offers travel over the separate
mission-server connection rather than a conventional `/missions/list` game-API
endpoint. That is an inference from the capture, not yet a decoded protocol.

The current log is sufficient for two narrower features, but not a mission-board
browser:

1. Local journals can reconstruct accepted-mission lifecycle state.
2. `starsystem` can indicate which markets expose mission services or are
   mission targets.

Neither source can enumerate offers that the commander has not accepted.

## Follow-up needed before implementation

1. Capture a tightly bounded session while opening and refreshing one mission
   board, then compare it with opening an NPC mission contact at the same market.
2. Instrument the connection selected from `missionsservers`, recording framing,
   request fields, and decoded response shapes while redacting every credential,
   address, token, and encryption key.
3. Confirm that any decoded response contains stable per-offer fields such as an
   offer id, origin faction, destination, expiry, reward choices, reputation,
   rank requirements, and wing/on-foot flags.
4. Determine refresh, pagination, acceptance, and expiry semantics before
   exposing data as live or complete.
5. Keep a future accepted-missions command separate from an available-offers
   command: journal state and mission-board state have different authority and
   freshness.

Until that work is complete, do not present the aggregate `numMissions` telemetry
as an available-mission list.
