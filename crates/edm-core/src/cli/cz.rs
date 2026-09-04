//! Command-line configuration and help for `edm cz`.

use crate::js::{self, text};

use super::config::LookupMode;
use super::{Cli, CliError, Flag};

/// Where to centre a combat-zone search.
#[derive(Clone, Debug, PartialEq)]
pub enum CzTarget {
    /// A system or station name to resolve through Ardent.
    Location { name: String, mode: LookupMode },
}

/// Reads an explicit combat-zone target.
pub fn cz_target(cli: &Cli<'_>) -> Result<CzTarget, CliError> {
    cz_target_with_default(cli, None)
}

/// Reads the target with an optional current-system default from the journal.
pub fn cz_target_with_default(
    cli: &Cli<'_>,
    current_system: Option<&str>,
) -> Result<CzTarget, CliError> {
    let station = cli.optional_value(Flag::Station, None);
    let system = cli.optional_value(Flag::System, None);
    let joined = cli.args().positionals.join(" ");
    let positional = text::js_trim(&joined);
    if station.is_some() && system.is_some() {
        return Err("cz accepts either --station or --system, not both"
            .to_owned()
            .into());
    }
    if let Some(name) = station
        .or(system)
        .or((!positional.is_empty()).then_some(positional))
    {
        let mode = if station.is_some() {
            LookupMode::Station
        } else if system.is_some() {
            LookupMode::System
        } else {
            LookupMode::Auto
        };
        return Ok(CzTarget::Location {
            name: name.to_owned(),
            mode,
        });
    }

    if let Some(name) = current_system
        .map(text::js_trim)
        .filter(|name| !name.is_empty())
    {
        return Ok(CzTarget::Location {
            name: name.to_owned(),
            mode: LookupMode::System,
        });
    }

    Err(
        "cz could not determine the current system; pass a target or set EDM_JOURNAL_DIR"
            .to_owned()
            .into(),
    )
}

/// Optional radius around the resolved target system.
///
/// Absence keeps the search in that one system.
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

/// Help for the Rust-only combat-zone locator.
#[must_use]
pub fn cz_usage() -> String {
    r"edm cz — list combat zones near a system

Usage
  edm cz [<system-or-station>] [options]

The system map (including the Frontline Solutions overlay) draws space conflict
zones from the game-internal starsystem payload. Intensity is Low, Med, or High.

Targets
  (no target)              use the player's current system from the latest local journal
                            (set EDM_JOURNAL_DIR to override automatic journal discovery)
  <system-or-station>       resolve through Ardent
  --system <name>           require a system-name match
  --station <name>          require a station-name match and use its system

Search area
  (no --radius)             list combat zones in the resolved system only
  --radius <ly>             check every known system within this distance of the resolved system;
                            with no target, centre the search on the player's current system

Options
  --settlements             include on-foot settlement warzones (with settlement names)
  --detail                  add site id and gameplay name
  --concurrency <n>         simultaneous Frontier reads, default 5, max 16
  --max-requests <n>        request ceiling, default 2000; above 250 requires --yes
  --yes                     confirm a regional search above 250 Frontier requests
  --cache, --no-cache       use or bypass the regional Ardent discovery cache
  --refresh                 refresh cached Ardent discovery before the search
  --cache-dir <path>        override the shared route/discovery cache directory
  --language <code>         default en          --cached-timestamp <n>  default 0
  --dry-run                 resolve and print the request or system plan without sending it
  --json                    emit one JSON document
  --method <verb>           override GET for endpoint probing

Credentials (option, else environment)
  --cmdr-id       COMMANDER_ID    --machine-id     MACHINE_ID
  --machine-token MACHINE_TOKEN   --auth-token     AUTH_TOKEN

Examples
  edm cz
  edm cz Sol
  edm cz --radius 20
  edm cz Arangorii --json
  edm cz Sol --radius 15 --settlements --yes"
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{EnvSnapshot, Table, parse_with};

    #[test]
    fn location_modes_and_radius_bounds() {
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

        let args = parse(&["cz", "--station", "Jaques Station"]);
        assert_eq!(
            cz_target(&Cli::new(&args, &env)).unwrap(),
            CzTarget::Location {
                name: "Jaques Station".to_owned(),
                mode: LookupMode::Station,
            }
        );

        let args = parse(&["cz", "--system", "Sol", "--station", "Jaques Station"]);
        assert!(cz_target(&Cli::new(&args, &env)).is_err());

        let args = parse(&["cz", "Sol", "--radius", "20"]);
        assert_eq!(search_radius(&Cli::new(&args, &env)).unwrap(), Some(20.0));
        let args = parse(&["cz", "Sol"]);
        assert_eq!(search_radius(&Cli::new(&args, &env)).unwrap(), None);
        let args = parse(&["cz", "Sol", "--radius", "501"]);
        assert!(search_radius(&Cli::new(&args, &env)).is_err());
        let args = parse(&["cz", "Sol", "--radius", "0"]);
        assert!(search_radius(&Cli::new(&args, &env)).is_err());

        let args = parse(&["cz"]);
        assert_eq!(
            cz_target_with_default(&Cli::new(&args, &env), Some("  Colonia  ")).unwrap(),
            CzTarget::Location {
                name: "Colonia".to_owned(),
                mode: LookupMode::System,
            }
        );
        assert!(cz_target(&Cli::new(&args, &env)).is_err());
    }
}
