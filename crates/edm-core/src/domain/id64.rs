//! The ID64 system address codec.
//!
//! An address packs the *boxel* a system was generated in, not its position, so
//! it cannot be derived from coordinates alone — the mass code and the system's
//! index within its boxel are generation properties. Layout, low bit first:
//!
//! ```text
//! 3 bits mass code | 7-m boxel Z | 7 sector Z | 7-m boxel Y | 6 sector Y |
//! 7-m boxel X | 7 sector X | remainder = index within the boxel
//! ```
//!
//! The `markets` command decodes an address, then re-encodes it from the
//! coordinates Ardent reports and checks the two agree. That cross-check is the
//! only reason this module exists, and it is why the arithmetic must stay in
//! `f64`: Colonia's `x` is −9530.5, and an integer implementation lands it in
//! the wrong boxel.

use crate::js;

const SECTOR_SIZE: f64 = 1280.0;

/// The corner of sector (0,0,0), in light years.
pub const GALAXY_ORIGIN: Coordinates = Coordinates {
    x: 49_985.0,
    y: 40_985.0,
    z: 24_105.0,
};

/// A point in galactic space. Held as `f64` because Ardent reports fractional
/// light years and the boxel arithmetic depends on them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Coordinates {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Coordinates {
    #[must_use]
    pub fn axis(&self, axis: Axis) -> f64 {
        match axis {
            Axis::X => self.x,
            Axis::Y => self.y,
            Axis::Z => self.z,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    X,
    Y,
    Z,
}

impl Axis {
    const fn name(self) -> &'static str {
        match self {
            Self::X => "x",
            Self::Y => "y",
            Self::Z => "z",
        }
    }
}

/// An unpacked address.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AddressParts {
    pub mass_code: u32,
    /// `a`..`h`.
    pub mass_code_letter: char,
    /// Boxel edge length in light years: `10 * 2^mass_code`.
    pub boxel_size: f64,
    pub sector: Coordinates,
    pub boxel: Coordinates,
    pub index: f64,
    /// The boxel's low corner, in galactic coordinates.
    pub origin: Coordinates,
}

/// Unpacks a system address.
///
/// Takes an `f64` rather than a `u64` because the rejection message
/// interpolates the input through `String(n)`, so `decode(1.5)` must be able to
/// say `"1.5 is not a system address"`.
pub fn decode(address: f64) -> Result<AddressParts, String> {
    // ts:2370
    if !js::safe_int(address) || address < 0.0 {
        return Err(format!(
            "{} is not a system address",
            js::js_number(address)
        ));
    }

    let mut bits = address as u64;
    let mut take = |width: u32| -> u64 {
        // A width of 0 (mass code 7) must yield 0 and consume nothing, which
        // `(1 << 0) - 1 == 0` gives for free.
        let value = bits & ((1u64 << width) - 1);
        bits >>= width;
        value
    };

    let mass_code = take(3) as u32;
    let boxel_bits = 7 - mass_code;
    let boxel_z = take(boxel_bits) as f64;
    let sector_z = take(7) as f64;
    let boxel_y = take(boxel_bits) as f64;
    let sector_y = take(6) as f64;
    let boxel_x = take(boxel_bits) as f64;
    let sector_x = take(7) as f64;
    let index = bits as f64;

    let boxel_size = 10.0 * f64::from(1u32 << mass_code);

    Ok(AddressParts {
        mass_code,
        mass_code_letter: char::from(b'a' + mass_code as u8),
        boxel_size,
        sector: Coordinates {
            x: sector_x,
            y: sector_y,
            z: sector_z,
        },
        boxel: Coordinates {
            x: boxel_x,
            y: boxel_y,
            z: boxel_z,
        },
        index,
        origin: Coordinates {
            x: sector_x * SECTOR_SIZE + boxel_x * boxel_size - GALAXY_ORIGIN.x,
            y: sector_y * SECTOR_SIZE + boxel_y * boxel_size - GALAXY_ORIGIN.y,
            z: sector_z * SECTOR_SIZE + boxel_z * boxel_size - GALAXY_ORIGIN.z,
        },
    })
}

/// Packs coordinates, a mass code and a boxel index back into an address.
pub fn encode(coordinates: Coordinates, mass_code: f64, index: f64) -> Result<f64, String> {
    // ts:2405. `Number.isInteger`, not `isSafeInteger`.
    if !mass_code.is_finite() || mass_code.fract() != 0.0 || !(0.0..=7.0).contains(&mass_code) {
        return Err("mass code must be 0-7 (a-h)".to_owned());
    }
    let mass_code = mass_code as u32;
    let boxel_size = 10.0 * f64::from(1u32 << mass_code);
    let boxel_bits = 7 - mass_code;

    let x = place(coordinates.x, GALAXY_ORIGIN.x, 127.0, Axis::X, boxel_size)?;
    let y = place(coordinates.y, GALAXY_ORIGIN.y, 63.0, Axis::Y, boxel_size)?;
    let z = place(coordinates.z, GALAXY_ORIGIN.z, 127.0, Axis::Z, boxel_size)?;

    // The TypeScript packs with BigInt, so an oversized index widens the value
    // rather than wrapping, and only the final safe-integer check rejects it.
    // `u128` reproduces that without arbitrary precision: the widest field
    // layout tops out at 23 + 3*7 = 44 bits before the index, and any index
    // large enough to overflow `u128` from there is orders of magnitude past
    // the safe-integer bound this returns through anyway.
    let mut bits: u128 = u128::from(mass_code);
    let mut shift = 3u32;
    let mut put = |value: f64, width: u32| {
        bits |= (value as u128) << shift;
        shift += width;
    };
    put(z.boxel, boxel_bits);
    put(z.sector, 7);
    put(y.boxel, boxel_bits);
    put(y.sector, 6);
    put(x.boxel, boxel_bits);
    put(x.sector, 7);
    bits |= (index as u128) << shift;

    let packed = bits as f64;
    if !js::safe_int(packed) {
        return Err("packed address exceeds the safe integer range".to_owned());
    }
    Ok(packed)
}

struct Placement {
    sector: f64,
    boxel: f64,
}

fn place(
    value: f64,
    offset: f64,
    sector_limit: f64,
    axis: Axis,
    boxel_size: f64,
) -> Result<Placement, String> {
    let outside = || {
        format!(
            "{}={} falls outside the galactic grid",
            axis.name(),
            js::js_number(value)
        )
    };

    let shifted = value + offset;
    if shifted < 0.0 {
        return Err(outside());
    }
    let sector = (shifted / SECTOR_SIZE).floor();
    if sector > sector_limit {
        return Err(outside());
    }
    Ok(Placement {
        sector,
        boxel: ((shifted - sector * SECTOR_SIZE) / boxel_size).floor(),
    })
}

/// Do these coordinates fall inside the boxel the address describes?
///
/// Half-open on every axis: the low corner is inside, the high corner is the
/// next boxel along.
#[must_use]
pub fn contains(parts: &AddressParts, coordinates: Coordinates) -> bool {
    [Axis::X, Axis::Y, Axis::Z].into_iter().all(|axis| {
        let low = parts.origin.axis(axis);
        let value = coordinates.axis(axis);
        value >= low && value < low + parts.boxel_size
    })
}
