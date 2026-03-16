use core::fmt;

/// Error returned when decoding a wire format field fails.
///
/// Wraps the `&'static str` field name that could not be decoded.
#[derive(Clone, Debug)]
pub struct DecodeError(pub &'static str);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "decode error: {}", self.0)
    }
}
