//! `markets` — what a star system contains, and whether its address is
//! self-consistent.
//!
//! The address cross-check is the reason this command exists in the shape it
//! does: an ID64 packs the boxel a system was *generated* in, so it cannot be
//! derived from coordinates. Decoding the address and then re-encoding it from
//! the coordinates Ardent reports is the only way to catch a disagreement
//! between the two, and the `(round-trips)` / `(MISMATCH)` suffix is that
//! check's whole output.

use edm_core::cli::Flag;
use edm_core::cli::config::{self, CachedTimestamp, LookupMode, MarketsConfig};
use edm_core::consts::STARSYSTEM;
use edm_core::domain::id64::{self, AddressParts, Coordinates};
use edm_core::domain::starsystem::{MarketPoint, collect_points_of_interest, read_market_points};
use edm_core::js::json::JsValue;
use edm_core::js::{self, text};
use edm_core::render::{Block, columns, views};

use crate::ardent::{ArdentClient, ResolvedSystem};
use crate::game_api;
use crate::exchange::SendOptions;
use crate::net::HttpTransport;
use crate::ports::{Clock, Entropy, Fs};

use super::{App, CmdResult, decrypted, field, message, object, str_value};

/// `runMarkets` (ts:2997).
#[expect(
    clippy::too_many_lines,
    reason = "R50: one ordered sequence of reads, one network call and one long emission ladder; \
              the order is what is under review and splitting it would hide it"
)]
pub async fn run<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
) -> CmdResult {
    // Name precedence here is `--station ?? --system ?? positional`, the
    // opposite of `market`'s \[R52\], and `--address` is read first — so
    // `markets --address abc` with no name reports the address, not the name.
    let (resolved, address) = match config::markets_config(&app.cli).map_err(message)? {
        MarketsConfig::Address(address) => (None, address),
        MarketsConfig::Lookup { name, mode } => {
            if !app.session.json {
                // ts:3012
                app.note(format!("resolving \"{name}\" through Ardent..."));
            }
            let resolved = ArdentClient::new(app.http, &app.overrides.ardent_base)
                .resolve_location(&name, lookup(mode))
                .await?;
            let address = resolved.system.address;
            (Some(resolved), address)
        }
    };

    // Cross-checked against the packing algorithm rather than trusted alone.
    let parts = id64::decode(address)?;
    let mut rows = vec![
        field(
            "system",
            resolved.as_ref().map_or_else(
                || format!("address {}", js::js_number(address)),
                |resolved| resolved.system.name.clone(),
            ),
        ),
    ];
    if let Some(resolved) = &resolved {
        rows.push(field("resolved via", resolved.via.clone()));
    }
    rows.push(field("systemAddress", js::js_number(address)));
    rows.push(field(
        "mass code",
        format!(
            // ts:3033
            "{} ({}), boxel {} ly",
            parts.mass_code_letter,
            js::js_number(f64::from(parts.mass_code)),
            js::js_number(parts.boxel_size),
        ),
    ));
    rows.push(field("sector", triple(parts.sector)));
    rows.push(field(
        "boxel",
        format!("{}, index {}", triple(parts.boxel), js::js_number(parts.index)),
    ));
    rows.push(field("boxel origin", triple(parts.origin)));

    if let Some(resolved) = &resolved {
        let coordinates = resolved.system.coordinates;
        rows.push(field("coordinates", triple(coordinates)));
        let inside = id64::contains(&parts, coordinates);
        // Can throw: coordinates outside the galactic grid have no packing.
        let repacked = id64::encode(coordinates, f64::from(parts.mass_code), parts.index)?;
        rows.push(field(
            "coords in boxel",
            // ts:3042 — the dash is U+2014.
            if inside { "yes".to_owned() } else { "NO \u{2014} address and coordinates disagree".to_owned() },
        ));
        rows.push(field(
            "repacked address",
            format!(
                "{}{}",
                js::js_number(repacked),
                if repacked == address { " (round-trips)" } else { " (MISMATCH)" }
            ),
        ));
    }

    if !app.session.json {
        app.out.emit(&[Block::Table {
            title: "SYSTEM".to_owned(),
            columns: columns::FIELD_COLUMNS,
            rows,
        }]);
    }

    // Read *after* the table has already been printed, so a malformed
    // `--cached-timestamp` surfaces with output on stdout \[R50\].
    let query =
        config::starsystem_query(&app.cli, CachedTimestamp::Flag).map_err(message)?;
    let stamp = app.stamp()?;
    let request = app.prepare(
        STARSYSTEM,
        game_api::starsystem_fields(
            address,
            &query.language,
            query.cached_timestamp,
            &app.credentials,
            stamp.frontier_time,
        ),
        stamp,
    );
    let exchange = app.send(&request, SendOptions { quiet: app.session.json, ..Default::default() }).await;

    if app.session.json {
        // ts:3055 — and this return is why `--json --dump f` writes nothing
        // \[C16\].
        app.emit_json(
            &request,
            exchange.as_ref(),
            vec![
                ("system", resolved.as_ref().map_or(JsValue::Null, super::market::resolved_json)),
                ("address", JsValue::Num(address)),
                ("addressParts", parts_json(&parts)),
            ],
        );
        return Ok(());
    }
    let Some(text) = decrypted(exchange.as_ref()) else { return Ok(()) };

    if let Some(path) = app.cli.optional_value(Flag::Dump, None) {
        // Before `JSON.parse`, so a payload that cannot be parsed is still
        // written out \[C16\].
        app.ports.fs.write(std::path::Path::new(path), text).map_err(|error| error.to_string())?;
        app.note(format!(
            // ts:3065 — `decrypted.length` is UTF-16 units, labelled "bytes"
            // \[R37\].
            "wrote {} bytes of starsystem payload to {path}",
            js::format_integer(text::utf16_len(text) as f64)
        ));
    }

    let Ok(payload) = JsValue::parse(text) else {
        app.out.emit(&views::opaque_payload(text));
        return Ok(());
    };

    let all: Vec<MarketPoint<'_>> = payload.as_record().map_or_else(Vec::new, read_market_points);
    if all.is_empty() {
        // Shape drift: sniff the tree for anything station-like rather than
        // reporting nothing at all.
        let guessed = collect_points_of_interest(&payload);
        if guessed.is_empty() {
            // ts:3078
            app.note(
                "no markets found under starsystem.polities; pass --dump <file> to inspect the payload"
                    .to_owned(),
            );
            return Ok(());
        }
        // ts:3081
        app.note("starsystem.polities held no markets — falling back to a structural scan".to_owned());
        app.out.emit(&views::points_of_interest(&guessed));
        return Ok(());
    }

    let include_carriers = app.cli.switch_value(Flag::Carriers, false).map_err(message)?;
    // `markets` filters on `--trading` where the sweep filters on
    // `--all-markets`, and the sense is the complement \[R52\].
    let trading_only = app.cli.switch_value(Flag::Trading, false).map_err(message)?;

    let hidden_carriers =
        if include_carriers { 0 } else { all.iter().filter(|point| point.is_carrier()).count() };
    let survivors: Vec<&MarketPoint<'_>> =
        all.iter().filter(|point| include_carriers || !point.is_carrier()).collect();
    let hidden_idle =
        if trading_only { survivors.iter().filter(|point| !point.trades()).count() } else { 0 };
    let points: Vec<MarketPoint<'_>> = survivors
        .into_iter()
        .filter(|point| !trading_only || point.trades())
        .cloned()
        .collect();

    if points.is_empty() {
        // ts:3107
        app.note(format!(
            "all {} markets were filtered out; drop --trading or add --carriers",
            all.len()
        ));
        return Ok(());
    }

    app.out.emit(&views::market_points(
        &points,
        &format!(
            // ts:3113
            "MARKETS  {} of {} in {}{}",
            points.len(),
            all.len(),
            resolved.as_ref().map_or_else(
                || format!("address {}", js::js_number(address)),
                |resolved| resolved.system.name.clone()
            ),
            station_of(resolved.as_ref())
                .map_or_else(String::new, |station| format!(" (asked about {station})")),
        ),
    ));

    let mut skipped: Vec<String> = Vec::new();
    if hidden_carriers > 0 {
        skipped.push(format!("{hidden_carriers} fleet carriers hidden (--carriers to show)"));
    }
    if hidden_idle > 0 {
        skipped.push(format!("{hidden_idle} without a commodity market hidden by --trading"));
    }
    if !skipped.is_empty() {
        app.note(skipped.join(" | "));
    }

    // The name match is full-Unicode lowercasing on both sides \[R32\].
    let target = station_of(resolved.as_ref()).and_then(|station| {
        let wanted = station.to_lowercase();
        points.iter().find(|point| point.name.to_lowercase() == wanted)
    });
    app.note(match target {
        // ts:3126 — three spaces either side of the `or`.
        Some(point) => format!(
            "{}: list --market-id {}   or   trade --market-id {} --type buy --item <name> --qty <n>",
            point.name,
            js::js_number(point.market_id),
            js::js_number(point.market_id),
        ),
        None =>
            "feed a market id to: list --market-id <id>   or   trade --market-id <id> --type buy --item <name> --qty <n>"
                .to_owned(),
    });
    Ok(())
}

/// `markets` distinguishes all three lookup modes, unlike the sweep \[R52\].
const fn lookup(mode: LookupMode) -> edm_core::ardent::Lookup {
    match mode {
        LookupMode::Station => edm_core::ardent::Lookup::Station,
        LookupMode::System => edm_core::ardent::Lookup::System,
        LookupMode::Auto => edm_core::ardent::Lookup::Auto,
    }
}

/// `resolved?.station` — the station name a station search came in through.
fn station_of(resolved: Option<&ResolvedSystem>) -> Option<&str> {
    // Truthiness of the string: an empty station name is falsy in the
    // TypeScript's `resolved?.station ? ... : ...`.
    resolved?.station.as_deref().filter(|station| !station.is_empty())
}

/// `${x} / ${y} / ${z}` — interpolated, so each axis goes through
/// `Number::toString` \[R1\].
fn triple(coordinates: Coordinates) -> String {
    format!(
        "{} / {} / {}",
        js::js_number(coordinates.x),
        js::js_number(coordinates.y),
        js::js_number(coordinates.z)
    )
}

fn coordinates_json(coordinates: Coordinates) -> JsValue {
    object([
        ("x".to_owned(), JsValue::Num(coordinates.x)),
        ("y".to_owned(), JsValue::Num(coordinates.y)),
        ("z".to_owned(), JsValue::Num(coordinates.z)),
    ])
}

/// `SystemAddressParts` (ts:2352) as JSON, in the literal's own key order.
fn parts_json(parts: &AddressParts) -> JsValue {
    object([
        ("massCode".to_owned(), JsValue::Num(f64::from(parts.mass_code))),
        ("massCodeLetter".to_owned(), str_value(&parts.mass_code_letter.to_string())),
        ("boxelSize".to_owned(), JsValue::Num(parts.boxel_size)),
        ("sector".to_owned(), coordinates_json(parts.sector)),
        ("boxel".to_owned(), coordinates_json(parts.boxel)),
        ("index".to_owned(), JsValue::Num(parts.index)),
        ("origin".to_owned(), coordinates_json(parts.origin)),
    ])
}
