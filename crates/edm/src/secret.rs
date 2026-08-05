//! Credentials that cannot be printed by accident.
//!
//! The session carries an 80-character machine token and a 2024-character auth
//! token. The TypeScript holds both as ordinary strings and masks them at the
//! one place it renders them; a single stray interpolation anywhere else would
//! put a live credential on a terminal or into a log.
//!
//! [`Secret`] makes that a compile error rather than a review item. It has no
//! `Display`, no `Serialize`, no `Deref` and no `Clone`; its `Debug` shows only
//! a length; and the only way to reach the bytes is `expose`, which is
//! crate-private and greppable. The buffer is zeroed on drop.

use std::fmt;

use edm_core::js::text;
use zeroize::Zeroizing;

/// A credential.
pub struct Secret(Zeroizing<String>);

impl Secret {
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    /// The bytes. Crate-private on purpose: every call site should be one
    /// `rg 'expose\(' ` away.
    #[allow(dead_code, reason = "the envelope builder is the only caller and lands with capi.rs")]
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// `String.prototype.length` — UTF-16 code units, because that is the
    /// number the masked rendering prints and the length validation checks.
    #[must_use]
    pub fn len(&self) -> usize {
        text::utf16_len(&self.0)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How the envelope table renders it: `"80 chars (hidden)"`.
    #[must_use]
    pub fn masked(&self) -> String {
        format!("{} chars (hidden)", self.len())
    }
}

impl fmt::Debug for Secret {
    /// Never the value. `{:?}` on a struct holding one of these — in a panic
    /// message, a `dbg!`, an error chain — must not leak it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret({} chars)", self.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_leaks_the_value() {
        let secret = Secret::new("hunter2-and-then-some".to_owned());
        let rendered = format!("{secret:?}");
        assert!(!rendered.contains("hunter2"), "got: {rendered}");
        assert_eq!(rendered, "Secret(21 chars)");
    }

    #[test]
    fn masking_reports_utf16_length() {
        // What the envelope table shows, and what the credential length check
        // measures — both `String.prototype.length`, not bytes.
        assert_eq!(Secret::new("abc".to_owned()).masked(), "3 chars (hidden)");
        assert_eq!(Secret::new("é".to_owned()).len(), 1, "one UTF-16 unit, two UTF-8 bytes");
        assert_eq!(Secret::new("🚀".to_owned()).len(), 2, "a surrogate pair is two units");
    }
}
