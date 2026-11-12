use anyhow::{Context as _, Result};
use std::cmp::Ordering;

pub fn parse(version: &str) -> Result<semver::Version> {
    semver::Version::parse(version.trim_start_matches('v')).with_context(|| format!("invalid version {version:?}"))
}

/// Orders by semver precedence: `2.5.10-custom.2 < 2.5.10-custom.10 < 2.5.10`.
pub fn compare(a: &str, b: &str) -> Result<Ordering> {
    Ok(parse(a)?.cmp_precedence(&parse(b)?))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::compare;
    use std::cmp::Ordering::{Equal, Greater, Less};

    #[test]
    fn orders_custom_builds() {
        assert_eq!(compare("2.5.10-custom.2", "2.5.10-custom.10").unwrap(), Less);
        assert_eq!(compare("2.5.10-custom.10", "2.5.10").unwrap(), Less);
        assert_eq!(compare("2.5.11-custom.1", "2.5.10-custom.9").unwrap(), Greater);
        assert_eq!(compare("v2.5.10-custom.3", "2.5.10-custom.3").unwrap(), Equal);
        assert!(compare("2.5", "2.5.10").is_err());
    }
}
