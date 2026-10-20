//! Collect all problems of a request instead of failing on the first one
//!
//! ```
//! use ullun::validate::Validator;
//!
//! fn check(title: &str, priority: u8) -> ullun::error::Result<()> {
//!     let mut v = Validator::new();
//!     v.len_between("title", title, 1, 120);
//!     v.check(priority <= 3, "priority", "must be 0-3");
//!     v.finish()
//! }
//!
//! assert!(check("Write docs", 2).is_ok());
//! let err = check("", 9).unwrap_err();
//! assert_eq!(err.status_code(), 422);
//! assert_eq!(err.fields().len(), 2);
//! ```
//!
//! The client receives every problem at once:
//! `{"error":"Validation failed","status":422,"fields":{"title":["..."],"priority":["..."]}}`.

use crate::error::{Error, FieldError, Result};

/// Accumulates [`FieldError`]s; call [`finish`](Validator::finish) at the end
#[derive(Debug, Default)]
pub struct Validator {
    errors: Vec<FieldError>,
}

impl Validator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an error for `field`
    pub fn error(&mut self, field: &str, message: impl Into<String>) -> &mut Self {
        self.errors.push(FieldError {
            field: field.to_string(),
            message: message.into(),
        });
        self
    }

    /// Record `message` for `field` unless `condition` holds
    pub fn check(&mut self, condition: bool, field: &str, message: impl Into<String>) -> &mut Self {
        if !condition {
            self.error(field, message);
        }
        self
    }

    /// Text that is not empty after trimming
    pub fn non_empty(&mut self, field: &str, value: &str) -> &mut Self {
        self.check(!value.trim().is_empty(), field, "must not be empty")
    }

    /// Length in characters (not bytes) within `min..=max`
    pub fn len_between(&mut self, field: &str, value: &str, min: usize, max: usize) -> &mut Self {
        let len = value.chars().count();
        if len < min || len > max {
            self.error(field, format!("must be {min} to {max} characters long"));
        }
        self
    }

    /// Value within `min..=max`
    pub fn range<T: PartialOrd + std::fmt::Display>(
        &mut self,
        field: &str,
        value: T,
        min: T,
        max: T,
    ) -> &mut Self {
        if value < min || value > max {
            self.error(field, format!("must be between {min} and {max}"));
        }
        self
    }

    /// Simple e-mail shape check: one `@`, text on both sides, a dot in the
    /// domain, no whitespace. Deliverability is only proven by sending mail.
    pub fn email(&mut self, field: &str, value: &str) -> &mut Self {
        let ok = value.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && !domain.contains('@')
                && domain.split('.').count() >= 2
                && domain.split('.').all(|p| !p.is_empty())
        }) && !value.chars().any(|c| c.is_whitespace() || c.is_control())
            && value.len() <= 254;
        self.check(ok, field, "must be a valid e-mail address")
    }

    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }

    /// `Ok(())` without errors, otherwise [`Error::Validation`] (HTTP 422)
    pub fn finish(self) -> Result<()> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(Error::Validation(self.errors))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_every_problem() {
        let mut v = Validator::new();
        v.non_empty("a", "  ")
            .len_between("b", "xy", 3, 5)
            .range("c", 9, 0, 3);
        v.error("d", "custom").check(true, "e", "never");
        assert!(!v.is_valid());
        let err = v.finish().unwrap_err();
        let fields: Vec<_> = err.fields().iter().map(|f| f.field.as_str()).collect();
        assert_eq!(fields, ["a", "b", "c", "d"]);
        assert_eq!(err.status_code(), 422);
        assert_eq!(err.message(), "Validation failed");
        assert_eq!(err.fields()[1].message, "must be 3 to 5 characters long");
        assert_eq!(err.fields()[2].message, "must be between 0 and 3");
    }

    #[test]
    fn ok_when_nothing_failed() {
        let mut v = Validator::new();
        v.non_empty("a", "x")
            .len_between("b", "äöü", 3, 3)
            .range("c", 1.5, 0.0, 2.0);
        assert!(v.is_valid());
        assert!(v.finish().is_ok());
    }

    #[test]
    fn email_shapes() {
        let ok = |s: &str| {
            let mut v = Validator::new();
            v.email("email", s);
            v.is_valid()
        };
        for good in ["a@b.co", "first.last+tag@sub.example.org"] {
            assert!(ok(good), "{good}");
        }
        for bad in [
            "",
            "a",
            "a@",
            "@b.co",
            "a@b",
            "a@@b.co",
            "a b@c.de",
            "a@b..co",
            "a@.co",
            "a@b.co\n",
            &format!("{}@b.co", "x".repeat(260)),
        ] {
            assert!(!ok(bad), "{bad:?}");
        }
    }
}
