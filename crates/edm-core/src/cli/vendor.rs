//! Command-line configuration and help for `edm vendor`.

use crate::js::{self, text};

use super::config::LookupMode;
use super::{Cli, CliError, Flag};

/// What the vendor locator should inspect.
#[derive(Clone, Debug, PartialEq)]
pub enum VendorTarget {
    /// One Frontier market, without any required Ardent lookup.
    Market(f64),
    /// A system or station name to resolve through Ardent.
    Location { name: String, mode: LookupMode },
}

/// Reads an explicit vendor target.
pub fn vendor_target(cli: &Cli<'_>) -> Result<VendorTarget, CliError> {
    vendor_target_with_default(cli, None)
}

/// Reads the vendor target with an optional current-system default.
///
/// An explicit market id wins. Otherwise station, system, and positional names
/// are considered in that order. With no explicit target, the commander's current
/// journal system supplies the default.
pub fn vendor_target_with_default(
    cli: &Cli<'_>,
    current_system: Option<&str>,
) -> Result<VendorTarget, CliError> {
    let explicit_market = cli.optional_value(Flag::MarketId, None);
    let station = cli.optional_value(Flag::Station, None);
    let system = cli.optional_value(Flag::System, None);
    let joined = cli.args().positionals.join(" ");
    let positional = text::js_trim(&joined);
    if station.is_some() && system.is_some() {
        return Err("vendor accepts either --station or --system, not both"
            .to_owned()
            .into());
    }
    let name = station
        .or(system)
        .or((!positional.is_empty()).then_some(positional));

    if explicit_market.is_some() && name.is_some() {
        return Err(
            "vendor accepts either --market-id or a system/station name, not both"
                .to_owned()
                .into(),
        );
    }
    if let Some(raw) = explicit_market {
        return js::parse_unsigned_integer("--market-id", raw)
            .map(VendorTarget::Market)
            .map_err(CliError::from);
    }
    if let Some(name) = name {
        let mode = if station.is_some() {
            LookupMode::Station
        } else if system.is_some() {
            LookupMode::System
        } else {
            LookupMode::Auto
        };
        return Ok(VendorTarget::Location {
            name: name.to_owned(),
            mode,
        });
    }

    if let Some(name) = current_system
        .map(text::js_trim)
        .filter(|name| !name.is_empty())
    {
        return Ok(VendorTarget::Location {
            name: name.to_owned(),
            mode: LookupMode::System,
        });
    }

    Err(
        "vendor could not determine the current system; pass a target or set EDM_JOURNAL_DIR"
            .to_owned()
            .into(),
    )
}

/// Optional radius around the resolved target system.
///
/// Absence deliberately differs from the route command's default radius: a
/// vendor lookup without this flag retains its one-system scope.
pub fn search_radius(cli: &Cli<'_>) -> Result<Option<f64>, CliError> {
    let radius = cli.optional_decimal(Flag::Radius)?;
    if radius.is_some_and(|radius| radius > crate::ardent::ARDENT_MAX_DISTANCE_LY) {
        return Err(format!(
            "--radius must be at most {}",
            js::js_number(crate::ardent::ARDENT_MAX_DISTANCE_LY),
        )
        .into());
    }
    Ok(radius)
}

/// Minimum suit or weapon grade to include. The default keeps every grade.
pub fn minimum_level(cli: &Cli<'_>) -> Result<f64, CliError> {
    let level = cli.optional_number(Flag::MinLevel)?.unwrap_or(1.0);
    if level < 1.0 {
        return Err("--min-level must be at least 1".to_owned().into());
    }
    Ok(level)
}

/// Help for the Rust-only vendor locator.
#[must_use]
pub fn vendor_usage() -> String {
    r#"edm vendor — find live Pioneer Supplies stock

Usage
  edm vendor [<system-or-station>] [options]
  edm vendor --market-id <id> [options]

Targets
  (no target)              use the player's current system from the latest local journal
                            (set EDM_JOURNAL_DIR to override automatic journal discovery)
  <system-or-station>       resolve through Ardent; a station result with a market id checks only
                            that market, while a system result checks its non-carrier markets
  --system <name>           require a system-name match
  --station <name>          require a station-name match
  --market-id <id>          check one market directly

Search area
  (no --radius)             check only the resolved market or system
  --radius <ly>             check every known system within this distance of the resolved system;
                            with no target, centre the search on the player's current system
  Results                   show system and distance from that centre, sorted by item name

Options
  --min-level <n>           include only items of grade n or higher (alias: --min-grade)
  --detail                  include sold-out premium offers and Frontier prototype names
  --concurrency <n>         simultaneous Frontier reads, default 5, max 16
  --max-requests <n>        request ceiling, default 2000; above 250 requires --yes
  --yes                     confirm a regional search above 250 Frontier requests
  --carriers                include Fleet Carriers in a system search
  --cache, --no-cache       use or bypass the regional Ardent discovery cache
  --refresh                 refresh cached Ardent discovery before the search
  --cache-dir <path>        override the shared route/discovery cache directory
  --dry-run                 resolve and print the request or system plan without sending it
  --json                    emit one JSON document
  --method <verb>           override GET for endpoint probing

Credentials (option, else environment)
  --cmdr-id       COMMANDER_ID    --machine-id     MACHINE_ID
  --machine-token MACHINE_TOKEN   --auth-token     AUTH_TOKEN

Examples
  edm vendor
  edm vendor Sol
  edm vendor --radius 20
  edm vendor Sol --radius 20
  edm vendor --station "Jaques Station"
  edm vendor --market-id 4370953219
  edm vendor Colonia --detail --json"#
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{EnvSnapshot, Table, parse_with};

    #[test]
    fn explicit_market_and_ardent_lookup_modes_are_distinct() {
        let env = EnvSnapshot::empty();
        let parse = |argv: &[&str]| {
            parse_with(
                &argv
                    .iter()
                    .map(|value| (*value).to_owned())
                    .collect::<Vec<_>>(),
                Table::Extended,
            )
            .unwrap()
        };

        let args = parse(&["vendor", "--market-id", "42"]);
        assert_eq!(
            vendor_target(&Cli::new(&args, &env)).unwrap(),
            VendorTarget::Market(42.0)
        );

        let args = parse(&["vendor", "Sol", "--market-id", "42"]);
        assert!(vendor_target(&Cli::new(&args, &env)).is_err());

        let args = parse(&[
            "vendor",
            "--station",
            "Jaques Station",
            "--system",
            "Colonia",
        ]);
        assert!(vendor_target(&Cli::new(&args, &env)).is_err());

        let args = parse(&["vendor", "--station", "Jaques Station"]);
        assert_eq!(
            vendor_target(&Cli::new(&args, &env)).unwrap(),
            VendorTarget::Location {
                name: "Jaques Station".to_owned(),
                mode: LookupMode::Station,
            }
        );

        let args = parse(&["vendor", "--min-level", "3"]);
        assert_eq!(minimum_level(&Cli::new(&args, &env)).unwrap(), 3.0);
        let args = parse(&["vendor", "--min-grade", "0"]);
        assert!(minimum_level(&Cli::new(&args, &env)).is_err());

        let args = parse(&["vendor", "Sol", "--radius", "20"]);
        assert_eq!(search_radius(&Cli::new(&args, &env)).unwrap(), Some(20.0));
        let args = parse(&["vendor", "Sol"]);
        assert_eq!(search_radius(&Cli::new(&args, &env)).unwrap(), None);
        let args = parse(&["vendor", "Sol", "--radius", "501"]);
        assert!(search_radius(&Cli::new(&args, &env)).is_err());
        let args = parse(&["vendor", "Sol", "--radius", "0"]);
        assert!(search_radius(&Cli::new(&args, &env)).is_err());
        let args = parse(&["vendor", "Sol", "--radius", "Infinity"]);
        assert!(search_radius(&Cli::new(&args, &env)).is_err());

        let args = parse(&["vendor"]);
        assert_eq!(
            vendor_target_with_default(&Cli::new(&args, &env), Some("  Colonia  ")).unwrap(),
            VendorTarget::Location {
                name: "Colonia".to_owned(),
                mode: LookupMode::System,
            }
        );
        let market_env = EnvSnapshot::from_pairs(vec![("MARKET_ID".to_owned(), "42".to_owned())]);
        assert_eq!(
            vendor_target_with_default(&Cli::new(&args, &market_env), Some("Colonia")).unwrap(),
            VendorTarget::Location {
                name: "Colonia".to_owned(),
                mode: LookupMode::System,
            },
            "the live commander system is preferred to a stale MARKET_ID fallback"
        );
        assert!(
            vendor_target(&Cli::new(&args, &market_env)).is_err(),
            "MARKET_ID does not replace the journal-derived system default"
        );
    }
}
