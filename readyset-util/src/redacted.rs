//! Wrapper types which hide the contents of the wrapped types when printed with Debug and/or
//! Display. These wrappers are intended to be used to hide user PII in logs or errors.

use std::convert::Infallible;
use std::fmt::{Debug, Display};
use std::ops::Deref;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Wraps a type that implements Display and Debug, overriding both implementations if the
/// `redact_sensitive` feature is enabled
pub struct Sensitive<'a, T: ?Sized>(pub &'a T);

impl<T> Display for Sensitive<'_, T>
where
    T: ?Sized + Display,
{
    #[cfg(not(feature = "redact_sensitive"))]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
    #[cfg(feature = "redact_sensitive")]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<redacted>")
    }
}

impl<T> Debug for Sensitive<'_, T>
where
    T: ?Sized + Debug,
{
    #[cfg(not(feature = "redact_sensitive"))]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
    #[cfg(feature = "redact_sensitive")]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<redacted>")
    }
}

/// Wraps a given string, replacing its contents with "<redacted>" when debug
/// printed if the `redact_sensitive` feature is enabled.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactedString(pub String);

impl Deref for RedactedString {
    type Target = String;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Display for RedactedString {
    #[cfg(not(feature = "redact_sensitive"))]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
    #[cfg(feature = "redact_sensitive")]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<redacted>")
    }
}

impl Debug for RedactedString {
    #[cfg(not(feature = "redact_sensitive"))]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
    #[cfg(feature = "redact_sensitive")]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<redacted>")
    }
}

impl FromStr for RedactedString {
    type Err = Infallible;
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Ok(RedactedString(input.to_string()))
    }
}

impl From<String> for RedactedString {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<RedactedString> for String {
    fn from(s: RedactedString) -> Self {
        s.0
    }
}
