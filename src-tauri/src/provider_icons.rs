//! Provider brand marks served by the local OmniRoute dashboard.
//!
//! OmniRoute ships one SVG per provider under `public/providers/<id>.svg` and serves
//! them unauthenticated from the same port as its API. The tray bundles only a
//! handful of marks itself (`src/icons.js`); for every other provider it asks the
//! server, so a Qwen or Kimi account gets its real logo instead of a generic dot.
//!
//! Id resolution mirrors OmniRoute's own `ProviderIcon.tsx` (`PROVIDER_ICON_ALIASES`
//! and `LOCAL_SVG_ALIASES`, verified against v3.8.50), then falls back to
//! progressively shorter ids (`kimi-coding` → `kimi`, `minimax-cn` → `minimax`) —
//! OmniRoute names account-plan variants by suffixing the brand.

use std::io::Read;
use std::time::Duration;

/// Hard cap on an accepted mark. Real assets are 1–5 KB; anything bigger is not a logo.
const MAX_BYTES: usize = 256 * 1024;

/// Provider ids whose mark lives under another name (OmniRoute's alias tables, plus
/// the Zhipu family, whose `glm*`/`zai*` ids have no asset of their own).
const ALIASES: &[(&str, &str)] = &[
    ("opencode-go", "opencode"),
    ("opencode-zen", "opencode"),
    ("poe-web", "poe"),
    ("cursor-api", "cursor"),
    ("qwen-cloud", "qwencloud"),
    ("qwen-cloud-token-plan", "qwencloud"),
    ("glm", "zhipu"),
    ("glmt", "zhipu"),
    ("glm-cn", "zhipu"),
    ("zai", "zhipu"),
    ("zai-web", "zhipu"),
    ("zai-coding-plan", "zhipu"),
];

/// Result of asking the server for a mark.
///
/// `Unreachable` is kept apart from `Missing` so the caller does not cache a negative
/// answer produced while the server was still starting up.
#[derive(Debug, Clone, PartialEq)]
pub enum Lookup {
    Found(String),
    Missing,
    Unreachable,
}

/// Asset ids to try for `provider`, most specific first, without duplicates.
pub fn candidates(provider: &str) -> Vec<String> {
    let id = provider.trim().to_ascii_lowercase();
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        if !s.is_empty() && !out.iter().any(|o| o == s) {
            out.push(s.to_string());
        }
    };
    let aliased = ALIASES
        .iter()
        .find(|(from, _)| *from == id)
        .map(|(_, to)| *to);
    if let Some(a) = aliased {
        push(a);
    }
    push(&id);
    let mut cur = id.as_str();
    while let Some(i) = cur.rfind('-') {
        cur = &cur[..i];
        push(cur);
    }
    out
}

/// Fetch the first mark the server has for `provider`.
pub fn fetch(base_url: &str, provider: &str) -> Lookup {
    let mut reachable = false;
    for id in candidates(provider) {
        let url = format!("{base_url}/providers/{id}.svg");
        match ureq::get(&url).timeout(Duration::from_secs(3)).call() {
            Ok(resp) => {
                reachable = true;
                if !resp.content_type().contains("svg") {
                    continue;
                }
                let mut body = String::new();
                let read = resp
                    .into_reader()
                    .take(MAX_BYTES as u64 + 1)
                    .read_to_string(&mut body);
                if read.is_err() || body.len() > MAX_BYTES || !looks_like_svg(&body) {
                    continue;
                }
                return Lookup::Found(body);
            }
            // The Next.js server answers unknown assets with a 404 HTML page.
            Err(ureq::Error::Status(_, _)) => reachable = true,
            Err(_) => {}
        }
    }
    if reachable {
        Lookup::Missing
    } else {
        Lookup::Unreachable
    }
}

fn looks_like_svg(body: &str) -> bool {
    body.contains("<svg")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_wins_then_own_id_then_shorter_ids() {
        assert_eq!(
            candidates("qwen-cloud-token-plan"),
            vec![
                "qwencloud",
                "qwen-cloud-token-plan",
                "qwen-cloud-token",
                "qwen-cloud",
                "qwen"
            ]
        );
        assert_eq!(candidates("opencode-go"), vec!["opencode", "opencode-go"]);
    }

    #[test]
    fn suffixed_plan_ids_fall_back_to_the_brand() {
        assert_eq!(candidates("kimi-coding"), vec!["kimi-coding", "kimi"]);
        assert_eq!(candidates("minimax-cn"), vec!["minimax-cn", "minimax"]);
    }

    #[test]
    fn plain_ids_are_normalised_and_not_duplicated() {
        assert_eq!(candidates(" Claude "), vec!["claude"]);
        assert_eq!(candidates("glm"), vec!["zhipu", "glm"]);
        assert!(candidates("").is_empty());
    }

    #[test]
    fn rejects_non_svg_bodies() {
        assert!(!looks_like_svg("<!DOCTYPE html><html>404</html>"));
        assert!(looks_like_svg(
            "<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\"/>"
        ));
    }
}
