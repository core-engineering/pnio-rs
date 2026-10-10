//! `NameOfStation` syntax (IEC 61158-6-10 §4.3.1.4.15, DNS-label form).

use thiserror::Error;

/// Why a station name was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NameError {
    /// Empty name (only ResetToFactory may blank the name).
    #[error("empty name")]
    Empty,
    /// Longer than 240 bytes.
    #[error("name is {0} bytes, max 240")]
    TooLong(usize),
    /// A character outside `[a-z0-9-.]`.
    #[error("invalid character {0:?}")]
    BadChar(char),
    /// A label is empty, longer than 63, or starts/ends with `-`.
    #[error("invalid label")]
    BadLabel,
    /// The first label is `port-xyz` or `port-xyz-abcde` (reserved for port IDs).
    #[error("reserved port-name form")]
    LooksLikePort,
    /// Four numeric labels: an IPv4 literal.
    #[error("looks like an IPv4 address")]
    LooksLikeIp,
}

/// Maximum `NameOfStation` length in bytes.
pub const MAX_NAME_LEN: usize = 240;

/// Validate a `NameOfStation` against the DNS-label rules.
pub fn validate_station_name(name: &str) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name.len() > MAX_NAME_LEN {
        return Err(NameError::TooLong(name.len()));
    }
    if let Some(c) = name
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '.'))
    {
        return Err(NameError::BadChar(c));
    }
    let labels: Vec<&str> = name.split('.').collect();
    for l in &labels {
        if l.is_empty() || l.len() > 63 || l.starts_with('-') || l.ends_with('-') {
            return Err(NameError::BadLabel);
        }
    }
    if labels.len() == 4 && labels.iter().all(|l| l.bytes().all(|b| b.is_ascii_digit())) {
        return Err(NameError::LooksLikeIp);
    }
    if is_port_form(labels[0]) {
        return Err(NameError::LooksLikePort);
    }
    Ok(())
}

/// `port-xyz` or `port-xyz-abcde` with x, y, z, a..e digits.
fn is_port_form(label: &str) -> bool {
    let parts: Vec<&str> = label.split('-').collect();
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    match parts.as_slice() {
        ["port", xyz] => digits(xyz, 3),
        ["port", xyz, abcde] => digits(xyz, 3) && digits(abcde, 5),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_and_dotted_names() {
        // 240 bytes in total: labels of 63, 63, 63 and 48 chars plus three dots.
        let max_len = format!("{0}.{0}.{0}.{1}", "x".repeat(63), "x".repeat(48));
        assert_eq!(max_len.len(), 240);
        for n in [
            "pnio-dev",
            "pnio-dev-b",
            "a",
            "device-1.plant-2.site",
            max_len.as_str(),
        ] {
            assert_eq!(validate_station_name(n), Ok(()), "{n}");
        }
    }

    #[test]
    fn rejects_empty_too_long_uppercase_and_bad_labels() {
        assert_eq!(validate_station_name(""), Err(NameError::Empty));
        assert_eq!(
            validate_station_name(&"x".repeat(241)),
            Err(NameError::TooLong(241))
        );
        assert_eq!(validate_station_name("Pnio"), Err(NameError::BadChar('P')));
        assert_eq!(
            validate_station_name("pnio_dev"),
            Err(NameError::BadChar('_'))
        );
        assert_eq!(validate_station_name("-pnio"), Err(NameError::BadLabel));
        assert_eq!(validate_station_name("pnio-"), Err(NameError::BadLabel));
        assert_eq!(validate_station_name("a..b"), Err(NameError::BadLabel));
        assert_eq!(
            validate_station_name(&format!("{}.b", "x".repeat(64))),
            Err(NameError::BadLabel)
        );
    }

    #[test]
    fn rejects_port_names_and_ip_literals() {
        assert_eq!(
            validate_station_name("port-001"),
            Err(NameError::LooksLikePort)
        );
        assert_eq!(
            validate_station_name("port-001-12345"),
            Err(NameError::LooksLikePort)
        );
        assert_eq!(
            validate_station_name("port-001.pnio-dev"),
            Err(NameError::LooksLikePort)
        ); // first label reserved
        assert_eq!(validate_station_name("pnio-dev.port-001"), Ok(())); // only the *first label* form is reserved
        assert_eq!(
            validate_station_name("192.168.0.50"),
            Err(NameError::LooksLikeIp)
        );
        assert_eq!(validate_station_name("1.2.3"), Ok(())); // not four numeric labels
    }
}
