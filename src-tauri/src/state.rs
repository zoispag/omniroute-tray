use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum ServerState {
    #[default]
    Stopped,
    Starting,
    Running {
        version: Option<String>,
    },
    UpdateAvailable {
        current: String,
        latest: String,
    },
    Updating {
        target: String,
    },
    Error {
        reason: String,
    },
}

#[allow(dead_code)]
impl ServerState {
    pub fn is_running(&self) -> bool {
        matches!(
            self,
            ServerState::Running { .. } | ServerState::UpdateAvailable { .. }
        )
    }

    pub fn tray_indicator(&self) -> &'static str {
        match self {
            ServerState::Stopped => "stopped",
            ServerState::Starting => "starting",
            ServerState::Running { .. } => "running",
            ServerState::UpdateAvailable { .. } => "update-available",
            ServerState::Updating { .. } => "updating",
            ServerState::Error { .. } => "error",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_and_update_available_are_running() {
        assert!(ServerState::Running { version: None }.is_running());
        assert!(ServerState::UpdateAvailable {
            current: "3.8.44".into(),
            latest: "3.9.0".into()
        }
        .is_running());
    }

    #[test]
    fn stopped_and_error_are_not_running() {
        assert!(!ServerState::Stopped.is_running());
        assert!(!ServerState::Error {
            reason: "boom".into()
        }
        .is_running());
        assert!(!ServerState::Starting.is_running());
    }

    #[test]
    fn indicators_map_to_expected_strings() {
        assert_eq!(ServerState::Stopped.tray_indicator(), "stopped");
        assert_eq!(ServerState::Starting.tray_indicator(), "starting");
        assert_eq!(
            ServerState::Running { version: None }.tray_indicator(),
            "running"
        );
        assert_eq!(
            ServerState::Updating {
                target: "3.9.0".into()
            }
            .tray_indicator(),
            "updating"
        );
        assert_eq!(
            ServerState::Error { reason: "x".into() }.tray_indicator(),
            "error"
        );
    }

    #[test]
    fn serializes_with_state_tag() {
        let json = serde_json::to_string(&ServerState::Running {
            version: Some("3.8.44".into()),
        })
        .unwrap();
        assert!(json.contains("\"state\":\"running\""));
        assert!(json.contains("\"version\":\"3.8.44\""));
    }
}
