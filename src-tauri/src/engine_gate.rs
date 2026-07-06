use semver::{Version, VersionReq};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineVerdict {
    Compatible,
    Incompatible { required: String, bundled: String },
    Unknown,
}

pub fn check_engine(engines_node: Option<&str>, bundled_node: &str) -> EngineVerdict {
    let Some(range) = engines_node else {
        return EngineVerdict::Unknown;
    };

    let Ok(bundled) = Version::parse(bundled_node.trim_start_matches('v')) else {
        return EngineVerdict::Unknown;
    };

    let alternatives = parse_node_range(range);
    if alternatives.is_empty() {
        return EngineVerdict::Unknown;
    }

    if alternatives.iter().any(|req| req.matches(&bundled)) {
        EngineVerdict::Compatible
    } else {
        EngineVerdict::Incompatible {
            required: range.to_string(),
            bundled: bundled_node.to_string(),
        }
    }
}

fn parse_node_range(range: &str) -> Vec<VersionReq> {
    range
        .split("||")
        .filter_map(|part| {
            let anded = part.split_whitespace().collect::<Vec<_>>().join(",");
            VersionReq::parse(&anded).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_version_in_range() {
        let verdict = check_engine(Some(">=22.0.0 <23 || >=24.0.0 <27"), "24.5.0");
        assert_eq!(verdict, EngineVerdict::Compatible);
    }

    #[test]
    fn blocks_version_requiring_newer_node() {
        let verdict = check_engine(Some(">=27.0.0"), "24.5.0");
        assert!(matches!(verdict, EngineVerdict::Incompatible { .. }));
    }

    #[test]
    fn blocks_lower_gap_between_ranges() {
        let verdict = check_engine(Some(">=22.0.0 <23 || >=24.0.0 <27"), "23.4.0");
        assert!(matches!(verdict, EngineVerdict::Incompatible { .. }));
    }

    #[test]
    fn unknown_when_no_constraint() {
        assert_eq!(check_engine(None, "24.5.0"), EngineVerdict::Unknown);
    }

    #[test]
    fn strips_leading_v_from_bundled() {
        let verdict = check_engine(Some(">=24.0.0 <27"), "v24.5.0");
        assert_eq!(verdict, EngineVerdict::Compatible);
    }
}
