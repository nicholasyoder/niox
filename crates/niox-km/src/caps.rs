//! Device capabilities: what a client's virtual device must be able to emit.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::event::{EV_ABS, EV_SYN};

/// Event types are below `EV_CNT`.
pub const TYPE_LIMIT: u16 = 0x20;
/// Codes of any type are below `KEY_CNT`, the largest per-type count.
pub const CODE_LIMIT: u16 = 0x300;
/// Absolute axes are below `ABS_CNT`.
pub const ABS_LIMIT: u16 = 0x40;
/// Input properties are below `INPUT_PROP_CNT`.
pub const PROP_LIMIT: u16 = 0x20;

/// An absolute axis's range, as in the kernel's `struct input_absinfo`
/// (minus the current value, which a new device doesn't need).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct AbsInfo {
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

/// A code that can't exist in a kernel capability bitmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CapsError {
    #[error("event type {0:#x} is out of range or EV_SYN")]
    Type(u16),
    #[error("code {code:#x} is out of range for event type {kind:#x}")]
    Code { kind: u16, code: u16 },
    #[error("EV_ABS axis {0:#x} needs its range: use insert_abs")]
    AbsWithoutInfo(u16),
    #[error("input property {0:#x} is out of range")]
    Property(u16),
}

/// The event types and codes a set of input devices can produce, with
/// absolute axis ranges and input properties: everything needed to create
/// a uinput device that can replay their events.
///
/// The host unions the capabilities of every device it captures and sends
/// them to each client. Every code is within kernel limits, so a value of
/// this type always encodes (but see [`crate::wire`] for the size limit).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Capabilities {
    /// Event type → codes. A type may be present with no codes (`EV_REP`).
    /// Never contains `EV_SYN`, which every device has implicitly.
    types: BTreeMap<u16, BTreeSet<u16>>,
    /// The range of every `EV_ABS` code in `types`.
    abs: BTreeMap<u16, AbsInfo>,
    properties: BTreeSet<u16>,
}

impl Capabilities {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an event type with no codes (e.g. `EV_REP`).
    pub fn insert_type(&mut self, kind: u16) -> Result<(), CapsError> {
        if kind == EV_SYN || kind >= TYPE_LIMIT {
            return Err(CapsError::Type(kind));
        }
        self.types.entry(kind).or_default();
        Ok(())
    }

    /// Adds one code of a type other than `EV_ABS`.
    pub fn insert(&mut self, kind: u16, code: u16) -> Result<(), CapsError> {
        if kind == EV_ABS {
            return Err(CapsError::AbsWithoutInfo(code));
        }
        if code >= CODE_LIMIT {
            return Err(CapsError::Code { kind, code });
        }
        self.insert_type(kind)?;
        self.types.entry(kind).or_default().insert(code);
        Ok(())
    }

    /// Adds an absolute axis and its range. If the axis is already present
    /// (from another device), the range widens to cover both.
    pub fn insert_abs(&mut self, code: u16, info: AbsInfo) -> Result<(), CapsError> {
        if code >= ABS_LIMIT {
            return Err(CapsError::Code { kind: EV_ABS, code });
        }
        self.types.entry(EV_ABS).or_default().insert(code);
        self.abs
            .entry(code)
            .and_modify(|have| {
                have.minimum = have.minimum.min(info.minimum);
                have.maximum = have.maximum.max(info.maximum);
            })
            .or_insert(info);
        Ok(())
    }

    /// Adds an input property (`INPUT_PROP_*`).
    pub fn insert_property(&mut self, prop: u16) -> Result<(), CapsError> {
        if prop >= PROP_LIMIT {
            return Err(CapsError::Property(prop));
        }
        self.properties.insert(prop);
        Ok(())
    }

    /// Adds everything in `other` to `self`.
    pub fn union_with(&mut self, other: &Capabilities) {
        for (&kind, codes) in &other.types {
            let have = self.types.entry(kind).or_default();
            if kind != EV_ABS {
                have.extend(codes);
            }
        }
        for (&code, &info) in &other.abs {
            // Always in range: `other` upheld the same invariants.
            let _ = self.insert_abs(code, info);
        }
        self.properties.extend(&other.properties);
    }

    /// Whether a device created with `self` can do everything `other`
    /// describes: every type, code, and property, and every axis's range.
    pub fn covers(&self, other: &Capabilities) -> bool {
        other.types.iter().all(|(kind, codes)| {
            self.types
                .get(kind)
                .is_some_and(|have| have.is_superset(codes))
        }) && other.abs.iter().all(|(code, info)| {
            self.abs
                .get(code)
                .is_some_and(|have| have.minimum <= info.minimum && have.maximum >= info.maximum)
        }) && self.properties.is_superset(&other.properties)
    }

    /// Event types, ascending, each with its codes ascending.
    pub fn types(&self) -> impl Iterator<Item = (u16, &BTreeSet<u16>)> {
        self.types.iter().map(|(&kind, codes)| (kind, codes))
    }

    /// Absolute axes and their ranges, ascending by axis.
    pub fn abs(&self) -> impl Iterator<Item = (u16, AbsInfo)> + '_ {
        self.abs.iter().map(|(&code, &info)| (code, info))
    }

    /// Input properties, ascending.
    pub fn properties(&self) -> impl Iterator<Item = u16> + '_ {
        self.properties.iter().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.types.is_empty() && self.properties.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EV_KEY, EV_REL};

    const EV_REP: u16 = 0x14;

    fn info(minimum: i32, maximum: i32) -> AbsInfo {
        AbsInfo {
            minimum,
            maximum,
            ..AbsInfo::default()
        }
    }

    #[test]
    fn rejects_out_of_range() {
        let mut c = Capabilities::new();
        assert_eq!(c.insert_type(EV_SYN), Err(CapsError::Type(EV_SYN)));
        assert_eq!(c.insert(TYPE_LIMIT, 1), Err(CapsError::Type(TYPE_LIMIT)));
        assert_eq!(
            c.insert(EV_KEY, CODE_LIMIT),
            Err(CapsError::Code {
                kind: EV_KEY,
                code: CODE_LIMIT
            })
        );
        assert_eq!(c.insert(EV_ABS, 0), Err(CapsError::AbsWithoutInfo(0)));
        assert!(c.insert_abs(ABS_LIMIT, info(0, 1)).is_err());
        assert!(c.insert_property(PROP_LIMIT).is_err());
        assert!(c.is_empty());
    }

    #[test]
    fn union_and_covers() {
        let mut keyboard = Capabilities::new();
        keyboard.insert(EV_KEY, 30).unwrap();
        keyboard.insert_type(EV_REP).unwrap();
        let mut mouse = Capabilities::new();
        mouse.insert(EV_KEY, 0x110).unwrap();
        mouse.insert(EV_REL, 0).unwrap();

        let mut both = keyboard.clone();
        both.union_with(&mouse);
        assert!(both.covers(&keyboard));
        assert!(both.covers(&mouse));
        assert!(!keyboard.covers(&both));
        assert!(both.covers(&Capabilities::new()));

        let types: Vec<u16> = both.types().map(|(k, _)| k).collect();
        assert_eq!(types, [EV_KEY, EV_REL, EV_REP]);
    }

    #[test]
    fn abs_ranges_widen_on_union() {
        let mut a = Capabilities::new();
        a.insert_abs(0, info(0, 100)).unwrap();
        let mut b = Capabilities::new();
        b.insert_abs(0, info(-10, 50)).unwrap();

        let mut both = a.clone();
        both.union_with(&b);
        assert_eq!(both.abs().collect::<Vec<_>>(), [(0, info(-10, 100))]);
        assert!(both.covers(&a) && both.covers(&b));
        assert!(!a.covers(&b), "a narrower range isn't covered");
    }

    #[test]
    fn properties_count() {
        let mut a = Capabilities::new();
        a.insert_property(0).unwrap();
        assert!(!Capabilities::new().covers(&a));
        assert!(!a.is_empty());
    }
}
