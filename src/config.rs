//! Route table: the broker's configuration, loaded from TOML.
//!
//! A route is one generation backend the broker can dispatch to.
//! The table is data, not code (PLAN §2.3): adding a route or
//! changing its lane eligibility is a config edit, never a
//! rebuild. Validation is fail-closed — a malformed table refuses
//! to load, naming the offending route.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::decide::{JobKind, Lane};

/// Environment variable naming the broker's data directory.
pub const DATA_DIR_ENV: &str = "CARTONE_DATA_DIR";
/// Environment variable naming the route-table file directly.
/// When unset, the table lives at `<data dir>/routes.toml`.
pub const ROUTES_FILE_ENV: &str = "CARTONE_ROUTES_FILE";

/// Longest route name accepted (bounds config and wire input).
pub const MAX_ROUTE_NAME_CHARS: usize = 64;
/// Largest per-route execution timeout accepted, in seconds.
pub const MAX_TIMEOUT_SECS: u64 = 3600;

/// One generation route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    /// Wire/config name, unique in the table (`wai`, `kokoro`, …).
    pub name: String,
    /// The job kind this route serves.
    pub kind: JobKind,
    /// Whether the route accepts jobs at all. A disabled route is
    /// still listed — honestly, as disabled — but `decide()`
    /// denies every job for it.
    pub enabled: bool,
    /// Lanes eligible to use this route (from the route's own
    /// terms: a hosted route whose terms bar adult content is
    /// SFW-only here, enforced by the table, not prompt wording).
    pub lanes: Vec<Lane>,
    /// Execution command template: argv, executed directly (never
    /// through a shell). `{job_id}`, `{route}`, `{kind}`, `{lane}`
    /// and `{units}` are substituted per argument. Empty means the
    /// route is not wired to an executor yet; jobs for it fail at
    /// run time with a `route_not_configured` detail rather than
    /// pretending to generate.
    pub command: Vec<String>,
    /// Ledger cost per unit, in broker credits.
    pub unit_cost: f64,
    /// Bounded execution timeout, in seconds.
    pub timeout_secs: u64,
}

impl Route {
    /// True when an executor command is configured.
    pub fn configured(&self) -> bool {
        !self.command.is_empty()
    }

    /// True when the route can actually take a job right now:
    /// enabled and wired to an executor.
    pub fn available(&self) -> bool {
        self.enabled && self.configured()
    }
}

/// The route table as stored on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct RoutesFile {
    #[serde(default, rename = "route")]
    routes: Vec<Route>,
}

/// A validated route table.
#[derive(Debug, Clone, PartialEq)]
pub struct Routes {
    routes: Vec<Route>,
}

impl Routes {
    /// The built-in table (PLAN §2.3): local routes eligible for
    /// both lanes, hosted routes present but disabled and
    /// SFW-only until their terms and keys are configured. Local
    /// routes ship without command templates — wiring each
    /// engine's invocation is operator configuration, and an
    /// unwired route reports itself unconfigured rather than
    /// faking a binding.
    pub fn defaults() -> Routes {
        let local = vec![Lane::Sfw, Lane::Nsfw];
        let sfw_only = vec![Lane::Sfw];
        let routes = vec![
            Route {
                name: "wai".to_string(),
                kind: JobKind::Image,
                enabled: true,
                lanes: local.clone(),
                command: Vec::new(),
                unit_cost: 1.0,
                timeout_secs: 600,
            },
            Route {
                name: "ltx-video".to_string(),
                kind: JobKind::Video,
                enabled: true,
                lanes: local.clone(),
                command: Vec::new(),
                unit_cost: 10.0,
                timeout_secs: 1800,
            },
            Route {
                name: "kokoro".to_string(),
                kind: JobKind::Tts,
                enabled: true,
                lanes: local,
                command: Vec::new(),
                unit_cost: 0.5,
                timeout_secs: 600,
            },
            Route {
                name: "runway".to_string(),
                kind: JobKind::Video,
                enabled: false,
                lanes: sfw_only.clone(),
                command: Vec::new(),
                unit_cost: 25.0,
                timeout_secs: 1800,
            },
            Route {
                name: "hailuo".to_string(),
                kind: JobKind::Video,
                enabled: false,
                lanes: sfw_only.clone(),
                command: Vec::new(),
                unit_cost: 20.0,
                timeout_secs: 1800,
            },
            Route {
                name: "fal".to_string(),
                kind: JobKind::Image,
                enabled: false,
                lanes: sfw_only.clone(),
                command: Vec::new(),
                unit_cost: 5.0,
                timeout_secs: 600,
            },
            Route {
                name: "grok".to_string(),
                kind: JobKind::Video,
                enabled: false,
                lanes: sfw_only,
                command: Vec::new(),
                unit_cost: 20.0,
                timeout_secs: 1800,
            },
        ];
        Routes { routes }
    }

    /// Parse and validate a route table from TOML text.
    pub fn from_toml(text: &str) -> Result<Routes, ConfigError> {
        let file: RoutesFile =
            toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        let routes = Routes {
            routes: file.routes,
        };
        routes.validate()?;
        Ok(routes)
    }

    /// Serialize the table back to TOML (used to write the
    /// first-run default routes file).
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        let file = RoutesFile {
            routes: self.routes.clone(),
        };
        toml::to_string(&file).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Load the table from a file.
    pub fn load(path: &Path) -> Result<Routes, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Io(format!("read {}: {e}", path.display())))?;
        Routes::from_toml(&text)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let mut names: Vec<&str> = Vec::new();
        for route in &self.routes {
            if route.name.is_empty() || route.name.chars().count() > MAX_ROUTE_NAME_CHARS {
                return Err(ConfigError::InvalidRoute(format!(
                    "route name must be 1..={MAX_ROUTE_NAME_CHARS} chars, got {:?}",
                    route.name
                )));
            }
            if names.contains(&route.name.as_str()) {
                return Err(ConfigError::InvalidRoute(format!(
                    "duplicate route name {:?}",
                    route.name
                )));
            }
            names.push(route.name.as_str());
            if route.lanes.is_empty() {
                return Err(ConfigError::InvalidRoute(format!(
                    "route {:?} declares no eligible lanes",
                    route.name
                )));
            }
            if !route.unit_cost.is_finite() || route.unit_cost < 0.0 {
                return Err(ConfigError::InvalidRoute(format!(
                    "route {:?} unit_cost must be finite and >= 0",
                    route.name
                )));
            }
            if route.timeout_secs == 0 || route.timeout_secs > MAX_TIMEOUT_SECS {
                return Err(ConfigError::InvalidRoute(format!(
                    "route {:?} timeout_secs must be 1..={MAX_TIMEOUT_SECS}",
                    route.name
                )));
            }
            if let Some(program) = route.command.first()
                && program.is_empty()
            {
                return Err(ConfigError::InvalidRoute(format!(
                    "route {:?} command program must not be empty",
                    route.name
                )));
            }
        }
        Ok(())
    }

    /// Look up one route by name.
    pub fn get(&self, name: &str) -> Option<&Route> {
        self.routes.iter().find(|r| r.name == name)
    }

    /// All routes, in table order.
    pub fn all(&self) -> &[Route] {
        &self.routes
    }
}

/// Route-table load failure: the broker refuses to start.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("route table parse error: {0}")]
    Parse(String),
    #[error("route table invalid: {0}")]
    InvalidRoute(String),
    #[error("route table io error: {0}")]
    Io(String),
}

/// Resolve the data directory: `CARTONE_DATA_DIR`, else
/// `$HOME/.local/share/cartone`. No home and no override is a
/// refusal, never a guess.
pub fn resolve_data_dir(explicit: Option<&str>, home: Option<&str>) -> Result<PathBuf, String> {
    if let Some(dir) = explicit
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir));
    }
    match home {
        Some(home) if !home.is_empty() => Ok(PathBuf::from(home).join(".local/share/cartone")),
        _ => Err(format!(
            "{DATA_DIR_ENV} is unset and HOME is unavailable; refusing to guess a data directory"
        )),
    }
}

/// Resolve the routes file: `CARTONE_ROUTES_FILE`, else
/// `<data dir>/routes.toml`.
pub fn resolve_routes_path(explicit: Option<&str>, data_dir: &Path) -> PathBuf {
    match explicit {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => data_dir.join("routes.toml"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_carry_the_plan_routes() {
        let routes = Routes::defaults();
        let wai = routes.get("wai").expect("wai");
        assert_eq!(wai.kind, JobKind::Image);
        assert!(wai.enabled);
        assert_eq!(wai.lanes, vec![Lane::Sfw, Lane::Nsfw]);
        assert_eq!(routes.get("ltx-video").expect("ltx").kind, JobKind::Video);
        assert_eq!(routes.get("kokoro").expect("kokoro").kind, JobKind::Tts);
        for hosted in ["runway", "hailuo", "fal", "grok"] {
            let route = routes.get(hosted).expect(hosted);
            assert!(!route.enabled, "{hosted} starts disabled");
            assert_eq!(route.lanes, vec![Lane::Sfw], "{hosted} is sfw-only");
            assert!(
                !route.available(),
                "{hosted} is unavailable until configured"
            );
        }
    }

    #[test]
    fn toml_round_trip_preserves_the_table() {
        let routes = Routes::defaults();
        let text = routes.to_toml().expect("serialize");
        let back = Routes::from_toml(&text).expect("parse");
        assert_eq!(routes, back);
    }

    #[test]
    fn duplicate_route_names_are_refused() {
        let text = r#"
[[route]]
name = "wai"
kind = "image"
enabled = true
lanes = ["sfw"]
command = []
unit_cost = 1.0
timeout_secs = 60

[[route]]
name = "wai"
kind = "image"
enabled = true
lanes = ["sfw"]
command = []
unit_cost = 1.0
timeout_secs = 60
"#;
        assert!(matches!(
            Routes::from_toml(text),
            Err(ConfigError::InvalidRoute(_))
        ));
    }

    #[test]
    fn adversarial_tables_are_refused() {
        // A route with no eligible lanes could never be decided
        // honestly; a negative cost would corrupt the ledger; an
        // unbounded timeout breaks bounded work.
        let table = |lanes: &str, unit_cost: &str, timeout_secs: &str| {
            format!(
                "[[route]]\nname = \"x\"\nkind = \"image\"\nenabled = true\nlanes = {lanes}\ncommand = []\nunit_cost = {unit_cost}\ntimeout_secs = {timeout_secs}\n"
            )
        };
        for (case, text) in [
            ("no lanes", table("[]", "1.0", "60")),
            ("negative cost", table("[\"sfw\"]", "-1.0", "60")),
            ("zero timeout", table("[\"sfw\"]", "1.0", "0")),
            ("timeout over bound", table("[\"sfw\"]", "1.0", "99999")),
        ] {
            assert!(
                Routes::from_toml(&text).is_err(),
                "case {case} must be refused"
            );
        }
        assert!(Routes::from_toml("this is not toml").is_err());
        assert!(Routes::from_toml("[[route]]\nname = \"x\"\nkind = \"hologram\"\nenabled = true\nlanes = [\"sfw\"]\ncommand = []\nunit_cost = 1.0\ntimeout_secs = 60\n").is_err());
    }
}
