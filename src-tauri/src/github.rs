use serde::{Deserialize, Serialize};
use std::time::Duration;

const RELEASES_URL: &str = "https://api.github.com/repos/zoispag/omniroute-tray/releases/latest";

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TrayUpdate {
    pub current: String,
    pub latest: String,
    pub available: bool,
}

pub fn latest_release(current: &str) -> TrayUpdate {
    let mut update = TrayUpdate {
        current: current.to_string(),
        latest: current.to_string(),
        available: false,
    };

    // GitHub's REST API rejects requests without a User-Agent (HTTP 403).
    let release: Option<Release> = ureq::get(RELEASES_URL)
        .timeout(Duration::from_secs(8))
        .set("User-Agent", "omniroute-tray")
        .set("Accept", "application/vnd.github+json")
        .call()
        .ok()
        .and_then(|r| r.into_json().ok());

    if let Some(release) = release {
        let latest = release.tag_name.trim_start_matches('v').to_string();
        update.available = crate::updater::is_newer(&latest, current);
        update.latest = latest;
    }

    update
}
