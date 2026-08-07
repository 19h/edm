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

/// Reads the vendor target.
///
/// An explicit market id wins. Otherwise station, system, and positional names
/// are considered in that order; with no name, `MARKET_ID` is the final fallback.
pub fn vendor_target(cli: &Cli<'_>) -> Result<VendorTarget, CliError> {
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

    let Some(raw) = cli.optional_value(Flag::MarketId, Some("MARKET_ID")) else {
        return Err(
            "vendor needs a system or station name, or --market-id <id> (or MARKET_ID in the environment)"
                .to_owned()
                .into(),
        );
    };
    js::parse_unsigned_integer("MARKET_ID", raw)
        .map(VendorTarget::Market)
        .map_err(CliError::from)
}

/// Help for the Rust-only vendor locator.
#[must_use]
pub fn vendor_usage() -> String {
    r#"edm vendor — find live Pioneer Supplies stock

Usage
  edm vendor <system-or-station> [options]
  edm vendor --market-id <id> [options]

Targets
  <system-or-station>       resolve through Ardent; a station result with a market id checks only
                            that market, while a system result checks its non-carrier markets
  --system <name>           require a system-name match
  --station <name>          require a station-name match
  --market-id <id>          check one market directly (else MARKET_ID)

Options
  --detail                  include sold-out premium offers and Frontier prototype names
  --concurrency <n>         simultaneous Frontier reads, default 5, max 16
  --carriers                include Fleet Carriers in a system search
  --dry-run                 resolve and print the request or system plan without sending it
  --json                    emit one JSON document
  --method <verb>           override GET for endpoint probing

Credentials (option, else environment)
  --cmdr-id       COMMANDER_ID    --machine-id     MACHINE_ID
  --machine-token MACHINE_TOKEN   --auth-token     AUTH_TOKEN

Examples
  edm vendor Sol
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
    }
}
