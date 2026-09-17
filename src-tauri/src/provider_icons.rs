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
///
/// Deliberately NOT here: `github` (the GitHub Copilot connection). The server's
/// `copilot.svg` is Microsoft Copilot's mark; the GitHub one is bundled in
/// `src/icons.js`, which the popover consults before asking the server.
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
    // `openai-compatible-<uuid>` / `anthropic-compatible-<uuid>` are user-defined
    // endpoints (Ollama, a corporate gateway…). Shortening them would land on the
    // OpenAI/Anthropic brand mark, which is exactly the wrong claim to make, and
    // OmniRoute itself shows a text badge for these. No candidates at all: they
    // are never looked up and always get the letter badge.
    if GENERIC_PREFIXES.iter().any(|p| id.starts_with(p)) {
        return out;
    }
    push(&id);
    let mut cur = id.as_str();
    while let Some(i) = cur.rfind('-') {
        cur = &cur[..i];
        push(cur);
    }
    out
}

const GENERIC_PREFIXES: &[&str] = &["openai-compatible", "anthropic-compatible"];

/// Fetch the first mark the server has for `provider`.
///
/// `Missing` is only returned when EVERY candidate was definitively absent. If any
/// candidate failed transiently (timeout, 429, 5xx) and none was found, the answer
/// is `Unreachable`, because a preferred alias might exist and merely be unavailable
/// right now — caching `Missing` there would pin the fallback badge until restart.
pub fn fetch(base_url: &str, provider: &str) -> Lookup {
    let ids = candidates(provider);
    if ids.is_empty() {
        // Nothing to ask for (user-defined `*-compatible` node): definitively no mark.
        return Lookup::Missing;
    }
    let mut transient = false;
    for id in ids {
        let url = format!("{base_url}/providers/{id}.svg");
        match ureq::get(&url).timeout(Duration::from_secs(3)).call() {
            Ok(resp) => {
                // A 200 that is not an SVG (e.g. an HTML page) is a definitive miss
                // for this candidate.
                if !resp.content_type().contains("svg") {
                    continue;
                }
                let mut body = String::new();
                let read = resp
                    .into_reader()
                    .take(MAX_BYTES as u64 + 1)
                    .read_to_string(&mut body);
                if read.is_err() {
                    // The asset is there; we just failed to read it. Retry later.
                    transient = true;
                    continue;
                }
                if body.len() > MAX_BYTES || !looks_like_svg(&body) {
                    continue;
                }
                return Lookup::Found(body);
            }
            // The Next.js server answers unknown assets with a 404 HTML page. Any
            // other status (429, 5xx, a proxy hiccup) says nothing about whether the
            // asset exists, so it must not turn into a cached `Missing`.
            Err(ureq::Error::Status(code, _)) if is_definitive_miss(code) => {}
            Err(_) => transient = true,
        }
    }
    if transient {
        Lookup::Unreachable
    } else {
        Lookup::Missing
    }
}

/// Statuses that prove the asset is not there, as opposed to a transient failure.
fn is_definitive_miss(code: u16) -> bool {
    matches!(code, 404 | 410)
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
    fn github_is_not_aliased_to_microsoft_copilot() {
        // The GitHub Copilot mark is bundled in the frontend; the server's copilot.svg
        // is a different product's logo and must never be picked up for `github`.
        assert_eq!(candidates("github"), vec!["github"]);
        assert!(!candidates("github-copilot").contains(&"copilot".to_string()));
    }

    #[test]
    fn user_defined_compatible_endpoints_are_never_looked_up() {
        let id = "openai-compatible-chat-6739873e-d0f7-4f94-8533-510c8652eb36";
        assert!(candidates(id).is_empty());
        assert!(candidates("anthropic-compatible-x").is_empty());
        // No candidates → no request at all, and a definitive (cacheable) miss even
        // when the server is nowhere to be found.
        assert_eq!(fetch("http://127.0.0.1:1", id), Lookup::Missing);
    }

    #[test]
    fn unreachable_server_is_not_a_miss() {
        // Nothing listens on port 1: every candidate fails transiently.
        assert_eq!(fetch("http://127.0.0.1:1", "kimi"), Lookup::Unreachable);
    }

    #[test]
    fn plain_ids_are_normalised_and_not_duplicated() {
        assert_eq!(candidates(" Claude "), vec!["claude"]);
        assert_eq!(candidates("glm"), vec!["zhipu", "glm"]);
        assert!(candidates("").is_empty());
    }

    #[test]
    fn only_not_found_is_a_cacheable_miss() {
        assert!(is_definitive_miss(404));
        assert!(is_definitive_miss(410));
        for transient in [429, 500, 502, 503, 504] {
            assert!(
                !is_definitive_miss(transient),
                "{transient} must retry later"
            );
        }
    }

    #[test]
    fn rejects_non_svg_bodies() {
        assert!(!looks_like_svg("<!DOCTYPE html><html>404</html>"));
        assert!(looks_like_svg(
            "<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\"/>"
        ));
    }
}
