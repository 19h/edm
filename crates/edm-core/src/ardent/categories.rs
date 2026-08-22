//! Frontier commodity categories, keyed the way `--category` and `--quick` use them.
//!
//! Ardent's `/commodities` report is a list of ids with no `categoryname`. The
//! live market payload has the category, but `--quick` has to name commodities
//! *before* those payloads exist. This table is that missing column: EDCD's
//! `commodity.csv` membership, with the help-text spellings (`Narcotics` for
//! Legal Drugs, `Slaves` for Slavery) as the names a caller can type.

/// Display names advertised by `edm route --help`, in that order.
const KNOWN: &[&str] = &[
    "Metals",
    "Minerals",
    "Foods",
    "Chemicals",
    "Machinery",
    "Medicines",
    "Technology",
    "Textiles",
    "Consumer Items",
    "Industrial Materials",
    "Salvage",
    "Weapons",
    "Waste",
    "Narcotics",
    "Slaves",
];

/// Normalised spellings that resolve to a [`KNOWN`] name.
///
/// Includes the help names themselves, the space-stripped forms (`consumeritems`),
/// and the two EDCD labels that the game-internal API also emits (`legaldrugs`,
/// `slavery`).
const ALIASES: &[(&str, &str)] = &[
    ("metals", "Metals"),
    ("minerals", "Minerals"),
    ("foods", "Foods"),
    ("chemicals", "Chemicals"),
    ("machinery", "Machinery"),
    ("medicines", "Medicines"),
    ("technology", "Technology"),
    ("textiles", "Textiles"),
    ("consumeritems", "Consumer Items"),
    ("industrialmaterials", "Industrial Materials"),
    ("salvage", "Salvage"),
    ("weapons", "Weapons"),
    ("waste", "Waste"),
    ("narcotics", "Narcotics"),
    ("legaldrugs", "Narcotics"),
    ("slaves", "Slaves"),
    ("slavery", "Slaves"),
];

/// Ardent/Frontier commodity ids grouped by the canonical category they belong to.
///
/// Ids are stored already normalised (ASCII letters and digits, lowercased), so
/// a catalogue row that keeps an underscore still matches after
/// [`super::normalise_commodity_name`]. Non-marketable rows (limpets) are
/// omitted: they are not a `--category` anyone should look up.
const MEMBERS: &[(&str, &[&str])] = &[
    (
        "Metals",
        &[
            "aluminium",
            "beryllium",
            "bismuth",
            "cobalt",
            "copper",
            "gallium",
            "gold",
            "hafnium178",
            "indium",
            "lanthanum",
            "lithium",
            "osmium",
            "palladium",
            "platinum",
            "praseodymium",
            "samarium",
            "silver",
            "steel",
            "tantalum",
            "thallium",
            "thorium",
            "titanium",
            "uranium",
        ],
    ),
    (
        "Minerals",
        &[
            "alexandrite",
            "bauxite",
            "benitoite",
            "bertrandite",
            "bromellite",
            "coltan",
            "cryolite",
            "gallite",
            "goslarite",
            "grandidierite",
            "haematite",
            "indite",
            "jadeite",
            "lepidolite",
            "lithiumhydroxide",
            "lowtemperaturediamond",
            "methaneclathrate",
            "methanolmonohydratecrystals",
            "moissanite",
            "monazite",
            "musgravite",
            "opal",
            "painite",
            "pyrophyllite",
            "rhodplumsite",
            "rutile",
            "serendibite",
            "taaffeite",
            "uraninite",
        ],
    ),
    (
        "Foods",
        &[
            "algae",
            "animalmeat",
            "coffee",
            "fish",
            "foodcartridges",
            "fruitandvegetables",
            "grain",
            "syntheticmeat",
            "tea",
        ],
    ),
    (
        "Chemicals",
        &[
            "agronomictreatment",
            "explosives",
            "hydrogenfuel",
            "hydrogenperoxide",
            "liquidoxygen",
            "mineraloil",
            "nerveagents",
            "pesticides",
            "rockforthfertiliser",
            "surfacestabilisers",
            "syntheticreagents",
            "tritium",
            "water",
        ],
    ),
    (
        "Machinery",
        &[
            "articulationmotors",
            "atmosphericextractors",
            "buildingfabricators",
            "cropharvesters",
            "emergencypowercells",
            "exhaustmanifold",
            "geologicalequipment",
            "heatsinkinterlink",
            "heliostaticfurnaces",
            "hnshockmount",
            "iondistributor",
            "magneticemittercoil",
            "marinesupplies",
            "mineralextractors",
            "modularterminals",
            "powerconverter",
            "powergenerators",
            "powergridassembly",
            "powertransferconduits",
            "radiationbaffle",
            "reinforcedmountingplate",
            "skimercomponents",
            "thermalcoolingunits",
            "waterpurifiers",
        ],
    ),
    (
        "Medicines",
        &[
            "advancedmedicines",
            "agriculturalmedicines",
            "basicmedicines",
            "combatstabilisers",
            "performanceenhancers",
            "progenitorcells",
        ],
    ),
    (
        "Technology",
        &[
            "advancedcatalysers",
            "animalmonitors",
            "aquaponicsystems",
            "autofabricators",
            "bioreducinglichen",
            "computercomponents",
            "diagnosticsensor",
            "hazardousenvironmentsuits",
            "medicaldiagnosticequipment",
            "microcontrollers",
            "mutomimager",
            "nanobreakers",
            "resonatingseparators",
            "robotics",
            "structuralregulators",
            "telemetrysuite",
            "terrainenrichmentsystems",
        ],
    ),
    (
        "Textiles",
        &[
            "conductivefabrics",
            "leather",
            "militarygradefabrics",
            "naturalfabrics",
            "syntheticfabrics",
        ],
    ),
    (
        "Consumer Items",
        &[
            "clothing",
            "consumertechnology",
            "domesticappliances",
            "evacuationshelter",
            "survivalequipment",
            "trinketsoffortune",
        ],
    ),
    (
        "Industrial Materials",
        &[
            "ceramiccomposites",
            "cmmcomposite",
            "coolinghoses",
            "insulatingmembrane",
            "metaalloys",
            "neofabricinsulation",
            "polymers",
            "semiconductors",
            "superconductors",
        ],
    ),
    (
        "Salvage",
        &[
            "airelics",
            "ancientcasket",
            "ancientkey",
            "ancientorb",
            "ancientrelic",
            "ancientrelictg",
            "ancienttablet",
            "ancienttotem",
            "ancienturn",
            "antimattercontainmentunit",
            "antiquejewellery",
            "antiquities",
            "assaultplans",
            "comercialsamples",
            "coralsap",
            "damagedescapepod",
            "datacore",
            "diplomaticbag",
            "earthrelics",
            "encripteddatastorage",
            "encryptedcorrespondence",
            "fossilremnants",
            "genebank",
            "geologicalsamples",
            "hostage",
            "largeexplorationdatacash",
            "m3tissuesamplemembrane",
            "m3tissuesamplemycelium",
            "m3tissuesamplespores",
            "militaryintelligence",
            "mtissuesamplefluid",
            "mtissuesamplenerves",
            "mtissuesamplesoft",
            "mysteriousidol",
            "occupiedcryopod",
            "personaleffects",
            "politicalprisoner",
            "pparticulatesample",
            "preciousgems",
            "prohibitedresearchmaterials",
            "s6tissuesamplecells",
            "s6tissuesamplecoenosarc",
            "s6tissuesamplemesoglea",
            "s9tissuesampleshell",
            "sap8corecontainer",
            "scientificresearch",
            "scientificsamples",
            "smallexplorationdatacash",
            "spacepioneerrelics",
            "stissuesamplecells",
            "stissuesamplecore",
            "stissuesamplesurface",
            "tacticaldata",
            "thargoidbonefragments",
            "thargoidcystspecimen",
            "thargoidgeneratortissuesample",
            "thargoidheart",
            "thargoidorgansample",
            "thargoidpod",
            "thargoidscouttissuesample",
            "thargoidtissuesampletype1",
            "thargoidtissuesampletype10a",
            "thargoidtissuesampletype10b",
            "thargoidtissuesampletype10c",
            "thargoidtissuesampletype2",
            "thargoidtissuesampletype3",
            "thargoidtissuesampletype4",
            "thargoidtissuesampletype5",
            "thargoidtissuesampletype6",
            "thargoidtissuesampletype7",
            "thargoidtissuesampletype9a",
            "thargoidtissuesampletype9b",
            "thargoidtissuesampletype9c",
            "thargoidtitandrivecomponent",
            "timecapsule",
            "unknownartifact",
            "unknownartifact2",
            "unknownartifact3",
            "unknownbiologicalmatter",
            "unknownmineral",
            "unknownrefinedmineral",
            "unknownresin",
            "unknownsack",
            "unknowntechnologysamples",
            "unocuppiedescapepod",
            "unstabledatacore",
            "usscargoancientartefact",
            "usscargoblackbox",
            "usscargoexperimentalchemicals",
            "usscargomilitaryplans",
            "usscargoprototypetech",
            "usscargorareartwork",
            "usscargorebeltransmissions",
            "usscargotechnicalblueprints",
            "usscargotradedata",
            "wreckagecomponents",
        ],
    ),
    (
        "Weapons",
        &[
            "battleweapons",
            "landmines",
            "nonlethalweapons",
            "personalweapons",
            "reactivearmour",
        ],
    ),
    (
        "Waste",
        &["biowaste", "chemicalwaste", "scrap", "toxicwaste"],
    ),
    (
        "Narcotics",
        &[
            "basicnarcotics",
            "beer",
            "bootlegliquor",
            "liquor",
            "onionheadc",
            "tobacco",
            "wine",
        ],
    ),
    ("Slaves", &["imperialslaves", "slaves"]),
];

/// The help-text category names, in advertised order.
#[must_use]
pub fn known_categories() -> &'static [&'static str] {
    KNOWN
}

/// Turn a `--category` spelling into a canonical name, or nothing.
///
/// Case, spaces and punctuation are ignored the same way as commodity ids, so
/// `consumer items`, `ConsumerItems` and `consumer-items` are one name.
#[must_use]
pub fn resolve_category(typed: &str) -> Option<&'static str> {
    let key = super::normalise_commodity_name(typed);
    ALIASES
        .iter()
        .find_map(|(alias, canonical)| (*alias == key).then_some(*canonical))
}

/// Frontier `categoryname` for an Ardent commodity id, when this table knows it.
///
/// The id is normalised first: Ardent keeps an underscore in a few salvage
/// symbols (`m_tissuesample_fluid`) that the table stores without one.
#[must_use]
pub fn commodity_category(id: &str) -> Option<&'static str> {
    let key = super::normalise_commodity_name(id);
    if key.is_empty() {
        return None;
    }
    MEMBERS
        .iter()
        .find_map(|(category, members)| members.contains(&key.as_str()).then_some(*category))
}

#[cfg(test)]
mod tests {
    use super::{ALIASES, MEMBERS, commodity_category, known_categories, resolve_category};

    #[test]
    fn help_names_and_their_aliases_resolve() {
        for name in known_categories() {
            assert_eq!(resolve_category(name), Some(*name), "{name}");
        }
        assert_eq!(resolve_category("metals"), Some("Metals"));
        assert_eq!(resolve_category("MINERALS"), Some("Minerals"));
        assert_eq!(resolve_category("consumer items"), Some("Consumer Items"));
        assert_eq!(resolve_category("Consumer-Items"), Some("Consumer Items"));
        assert_eq!(
            resolve_category("industrial materials"),
            Some("Industrial Materials")
        );
        assert_eq!(resolve_category("Legal Drugs"), Some("Narcotics"));
        assert_eq!(resolve_category("slavery"), Some("Slaves"));
        assert!(resolve_category("rocks").is_none());
        assert!(resolve_category("").is_none());
    }

    #[test]
    fn membership_matches_the_live_symbols_quick_will_query() {
        assert_eq!(commodity_category("gold"), Some("Metals"));
        assert_eq!(commodity_category("Gold"), Some("Metals"));
        assert_eq!(
            commodity_category("lowtemperaturediamond"),
            Some("Minerals")
        );
        assert_eq!(commodity_category("painite"), Some("Minerals"));
        assert_eq!(commodity_category("basicnarcotics"), Some("Narcotics"));
        assert_eq!(commodity_category("imperialslaves"), Some("Slaves"));
        // Ardent keeps the underscore; the table does not.
        assert_eq!(commodity_category("m_tissuesample_fluid"), Some("Salvage"));
        assert!(commodity_category("unobtainium").is_none());
        assert!(
            commodity_category("drones").is_none(),
            "limpets are not a category lookup"
        );
    }

    #[test]
    fn every_member_belongs_to_exactly_one_known_category() {
        let mut seen = std::collections::BTreeSet::new();
        for (category, members) in MEMBERS {
            assert!(
                known_categories().contains(category),
                "{category} is not advertised"
            );
            for member in *members {
                assert!(
                    seen.insert(*member),
                    "{member} is listed under more than one category"
                );
                assert_eq!(commodity_category(member), Some(*category), "{member}");
            }
        }
        for (alias, canonical) in ALIASES {
            assert!(
                known_categories().contains(canonical),
                "alias {alias} points at unknown {canonical}"
            );
        }
    }
}
