//! The lane-eligibility gate: a pure `decide()` over
//! (route, lane, job kind).
//!
//! The gate is the broker's enforcement point (PLAN §2.3): a
//! route's lane eligibility comes from the route's own terms —
//! hosted terms that bar adult content make the route SFW-only —
//! and is enforced here, by the route table, never by prompt
//! wording. `decide()` is pure: no I/O, no state, so every path is
//! unit-testable and the same verdict is reached by every caller.

use serde::{Deserialize, Serialize};

use crate::config::Route;

/// The lane a job belongs to. Lane is a property of the job's
/// project, declared by the caller at submit time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lane {
    Sfw,
    Nsfw,
}

impl Lane {
    /// Parse the wire form (`"sfw"` / `"nsfw"`). Anything else is
    /// not a lane; there is no default.
    pub fn parse(text: &str) -> Option<Lane> {
        match text {
            "sfw" => Some(Lane::Sfw),
            "nsfw" => Some(Lane::Nsfw),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Lane::Sfw => "sfw",
            Lane::Nsfw => "nsfw",
        }
    }
}

/// What a job asks a route to make.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    Image,
    Video,
    Tts,
}

impl JobKind {
    /// Parse the wire form (`"image"` / `"video"` / `"tts"`).
    pub fn parse(text: &str) -> Option<JobKind> {
        match text {
            "image" => Some(JobKind::Image),
            "video" => Some(JobKind::Video),
            "tts" => Some(JobKind::Tts),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            JobKind::Image => "image",
            JobKind::Video => "video",
            JobKind::Tts => "tts",
        }
    }
}

/// The gate's verdict. A denial always names its rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny { code: &'static str, reason: String },
}

/// Decide whether `lane` may run a `kind` job on `route`.
///
/// Check order is part of the contract: a kind mismatch is named
/// first (the route cannot make that at all), then lane
/// eligibility (the route's terms bar this lane — the security
/// fact a caller most needs), then enablement (the route is
/// configured off).
pub fn decide(route: &Route, lane: Lane, kind: JobKind) -> Decision {
    if route.kind != kind {
        return Decision::Deny {
            code: "kind_mismatch",
            reason: format!(
                "route {:?} serves {:?} jobs, not {:?}",
                route.name,
                route.kind.as_str(),
                kind.as_str()
            ),
        };
    }
    if !route.lanes.contains(&lane) {
        return Decision::Deny {
            code: "lane_not_eligible",
            reason: format!(
                "route {:?} is not eligible for the {} lane (eligible: {})",
                route.name,
                lane.as_str(),
                route
                    .lanes
                    .iter()
                    .map(Lane::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
    }
    if !route.enabled {
        return Decision::Deny {
            code: "route_disabled",
            reason: format!("route {:?} is disabled until it is configured", route.name),
        };
    }
    Decision::Allow
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Routes;

    fn route(name: &str) -> Route {
        Routes::defaults().get(name).expect("default route").clone()
    }

    // ---------- validation (allowed paths) ----------

    #[test]
    fn local_routes_allow_both_lanes() {
        assert_eq!(
            decide(&route("wai"), Lane::Sfw, JobKind::Image),
            Decision::Allow
        );
        assert_eq!(
            decide(&route("wai"), Lane::Nsfw, JobKind::Image),
            Decision::Allow
        );
        assert_eq!(
            decide(&route("ltx-video"), Lane::Nsfw, JobKind::Video),
            Decision::Allow
        );
        assert_eq!(
            decide(&route("kokoro"), Lane::Sfw, JobKind::Tts),
            Decision::Allow
        );
    }

    // ---------- adversarial (denied paths) ----------

    #[test]
    fn nsfw_is_denied_on_sfw_only_hosted_routes() {
        // The Artlist lesson (§2.3): terms bar adult content, so
        // the table says SFW-only and the gate enforces it — even
        // though the route is also disabled, the lane fact is the
        // one named.
        match decide(&route("runway"), Lane::Nsfw, JobKind::Video) {
            Decision::Deny { code, reason } => {
                assert_eq!(code, "lane_not_eligible");
                assert!(reason.contains("nsfw"), "{reason}");
            }
            Decision::Allow => panic!("nsfw on an sfw-only route must be denied"),
        }
    }

    #[test]
    fn disabled_route_denies_even_an_eligible_lane() {
        match decide(&route("fal"), Lane::Sfw, JobKind::Image) {
            Decision::Deny { code, .. } => assert_eq!(code, "route_disabled"),
            Decision::Allow => panic!("a disabled route must deny"),
        }
    }

    #[test]
    fn kind_mismatch_denies_on_any_route() {
        match decide(&route("wai"), Lane::Sfw, JobKind::Video) {
            Decision::Deny { code, .. } => assert_eq!(code, "kind_mismatch"),
            Decision::Allow => panic!("an image route must not take video jobs"),
        }
        // Kind is checked before lane and enablement.
        match decide(&route("runway"), Lane::Nsfw, JobKind::Tts) {
            Decision::Deny { code, .. } => assert_eq!(code, "kind_mismatch"),
            Decision::Allow => panic!("kind mismatch must win"),
        }
    }

    #[test]
    fn unparseable_lanes_and_kinds_do_not_exist() {
        assert_eq!(Lane::parse("SFW"), None);
        assert_eq!(Lane::parse("adult"), None);
        assert_eq!(Lane::parse(""), None);
        assert_eq!(JobKind::parse("hologram"), None);
        assert_eq!(JobKind::parse(""), None);
    }
}
