use semver::Version;

use crate::installer::{install_version, InstallError, NodeRuntime};
use crate::runtime::Prefix;

pub fn is_newer(latest: &str, current: &str) -> bool {
    match (Version::parse(latest), Version::parse(current)) {
        (Ok(l), Ok(c)) => l > c,
        _ => latest != current,
    }
}

pub fn apply_update(
    prefix: &Prefix,
    node: &NodeRuntime,
    target: &str,
) -> Result<String, InstallError> {
    let previous = prefix.active_version();
    match install_version(prefix, node, target) {
        Ok(()) => Ok(target.to_string()),
        Err(e) => {
            if let Some(prev) = previous {
                let _ = prefix.discard_incomplete();
                let _ = prefix.activate(&prev);
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_newer_semver() {
        assert!(is_newer("3.9.0", "3.8.44"));
        assert!(is_newer("3.8.45", "3.8.44"));
    }

    #[test]
    fn same_or_older_is_not_newer() {
        assert!(!is_newer("3.8.44", "3.8.44"));
        assert!(!is_newer("3.8.43", "3.8.44"));
    }

    #[test]
    fn non_semver_falls_back_to_inequality() {
        assert!(is_newer("beta", "3.8.44"));
        assert!(!is_newer("3.8.44", "3.8.44"));
    }
}
