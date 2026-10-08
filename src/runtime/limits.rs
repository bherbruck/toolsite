//! How much one call into an app's code may use, and how an app asks for
//! more.
//!
//! Every app gets the defaults below without asking. A `[limits]` block in
//! its toolsite.toml may ask for more (or less) of each, and gets what it
//! asked up to the ceiling the site's owner set in the environment: a
//! request above a ceiling is clamped, never refused, and the deploy says
//! so. What is stored is what the app asked, so an owner who raises a
//! ceiling later gives it the rest without a redeploy.
//!
//! Fuel is optional throughout. A site whose owner sets a fuel ceiling of
//! `none` meters no fuel at all and relies on the wall clock alone.

use crate::{config::Config, runtime::wasm::Guards};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const MIB: u64 = 1024 * 1024;

/// What an app gets without asking: the limits every call had before an
/// app could ask for any.
pub const DEFAULT_REQUEST_FUEL: u64 = 200_000_000;
pub const DEFAULT_REQUEST_SECONDS: u64 = 5;
pub const DEFAULT_REQUEST_MEMORY_MB: u64 = 64;
pub const DEFAULT_JOB_FUEL: u64 = 2_000_000_000;
pub const DEFAULT_JOB_SECONDS: u64 = 60;
pub const DEFAULT_JOB_MEMORY_MB: u64 = 128;
pub const DEFAULT_QUERY_ROWS: u64 = 1_000;

/// The most any app may ask for, set by the site's owner. `None` fuel means
/// fuel is not metered for that kind of call at all.
#[derive(Debug, Clone, PartialEq)]
pub struct Ceilings {
    pub request_fuel: Option<u64>,
    pub request_seconds: u64,
    pub job_fuel: Option<u64>,
    pub job_seconds: u64,
    pub query_rows: u64,
    pub memory_mb: u64,
}

impl Default for Ceilings {
    /// Ten times a request's fuel, a minute for a request; fifty times a
    /// job's fuel, a quarter of an hour for a job; fifty thousand rows a
    /// query; a gigabyte of memory.
    fn default() -> Self {
        Self {
            request_fuel: Some(10 * DEFAULT_REQUEST_FUEL),
            request_seconds: 60,
            job_fuel: Some(50 * DEFAULT_JOB_FUEL),
            job_seconds: 15 * 60,
            query_rows: 50_000,
            memory_mb: 1024,
        }
    }
}

/// What an app's `[limits]` asks for. Every field is optional; one left out
/// takes the default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asked {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_fuel: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_fuel: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_seconds: Option<u64>,
    /// Rows one query may return, for `query` and `query-scoped` alike.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_rows: Option<u64>,
    /// Memory for a request and for a job. A resident instance has its own,
    /// under `[resident]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u64>,
}

impl Asked {
    fn fields(&self) -> [(&'static str, Option<u64>); 6] {
        [
            ("request_fuel", self.request_fuel),
            ("request_seconds", self.request_seconds),
            ("job_fuel", self.job_fuel),
            ("job_seconds", self.job_seconds),
            ("query_rows", self.query_rows),
            ("memory_mb", self.memory_mb),
        ]
    }

    /// Refuses a zero: no call can do anything with none of something.
    pub fn check(&self) -> Result<(), String> {
        for (name, value) in self.fields() {
            if value == Some(0) {
                return Err(format!("[limits] {name} must be at least 1"));
            }
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.fields().iter().all(|(_, value)| value.is_none())
    }
}

/// The limits a request and a job of one app actually run under.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Effective {
    pub request: Guards,
    pub job: Guards,
}

impl Effective {
    /// For an app's admin page and fetch metadata.
    pub fn describe(&self) -> serde_json::Value {
        let side = |guards: &Guards| {
            serde_json::json!({
                "fuel": guards.fuel,
                "seconds": guards.wall_clock.as_secs_f64(),
                "memory_mb": guards.memory_bytes as u64 / MIB,
                "query_rows": guards.query_rows,
            })
        };
        serde_json::json!({ "request": side(&self.request), "job": side(&self.job) })
    }
}

fn fuel(asked: Option<u64>, default: u64, ceiling: Option<u64>) -> Option<u64> {
    ceiling.map(|ceiling| asked.unwrap_or(default).min(ceiling))
}

impl Ceilings {
    /// Applies an app's ask, clamped to these ceilings. `None` is the app
    /// that asked for nothing.
    pub fn effective(&self, asked: Option<&Asked>) -> Effective {
        let none = Asked::default();
        let asked = asked.unwrap_or(&none);
        let rows = asked.query_rows.unwrap_or(DEFAULT_QUERY_ROWS).min(self.query_rows) as usize;
        let memory = |default: u64| (asked.memory_mb.unwrap_or(default).min(self.memory_mb) * MIB) as usize;
        let seconds = |asked: Option<u64>, default: u64, ceiling: u64| Duration::from_secs(asked.unwrap_or(default).min(ceiling));
        Effective {
            request: Guards {
                fuel: fuel(asked.request_fuel, DEFAULT_REQUEST_FUEL, self.request_fuel),
                memory_bytes: memory(DEFAULT_REQUEST_MEMORY_MB),
                wall_clock: seconds(asked.request_seconds, DEFAULT_REQUEST_SECONDS, self.request_seconds),
                query_rows: rows,
            },
            job: Guards {
                fuel: fuel(asked.job_fuel, DEFAULT_JOB_FUEL, self.job_fuel),
                memory_bytes: memory(DEFAULT_JOB_MEMORY_MB),
                wall_clock: seconds(asked.job_seconds, DEFAULT_JOB_SECONDS, self.job_seconds),
                query_rows: rows,
            },
        }
    }

    /// One line for each value asked past its ceiling, for the deploy to
    /// report: "asked for X, the site allows Y".
    pub fn clamped(&self, asked: &Asked) -> Vec<String> {
        let ceilings: [Option<u64>; 6] = [
            self.request_fuel,
            Some(self.request_seconds),
            self.job_fuel,
            Some(self.job_seconds),
            Some(self.query_rows),
            Some(self.memory_mb),
        ];
        let mut notes = Vec::new();
        for ((name, value), ceiling) in asked.fields().into_iter().zip(ceilings) {
            let Some(value) = value else { continue };
            match ceiling {
                Some(ceiling) if value > ceiling => {
                    notes.push(format!("[limits] {name}: asked for {value}, the site allows {ceiling}"))
                }
                None => notes.push(format!("[limits] {name}: asked for {value}, but this site meters no fuel")),
                _ => {}
            }
        }
        notes
    }

    /// The ceilings from the environment, falling back to the defaults for
    /// anything unset. A fuel ceiling of `none` (or 0) turns fuel off for
    /// that kind of call.
    pub fn from_env(read: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let defaults = Self::default();
        let whole = |name: &str, default: u64| -> Result<u64, String> {
            match read(name) {
                None => Ok(default),
                Some(value) => match value.trim().parse::<u64>() {
                    Ok(0) | Err(_) => Err(format!("{name} must be a whole number above 0, not {value:?}")),
                    Ok(n) => Ok(n),
                },
            }
        };
        let fuel = |name: &str, default: Option<u64>| -> Result<Option<u64>, String> {
            match read(name) {
                None => Ok(default),
                Some(value) if value.trim().eq_ignore_ascii_case("none") || value.trim() == "0" => Ok(None),
                Some(value) => value
                    .trim()
                    .parse::<u64>()
                    .map(Some)
                    .map_err(|_| format!("{name} must be a whole number, or none, not {value:?}")),
            }
        };
        Ok(Self {
            request_fuel: fuel("TOOLSITE_MAX_REQUEST_FUEL", defaults.request_fuel)?,
            request_seconds: whole("TOOLSITE_MAX_REQUEST_SECONDS", defaults.request_seconds)?,
            job_fuel: fuel("TOOLSITE_MAX_JOB_FUEL", defaults.job_fuel)?,
            job_seconds: whole("TOOLSITE_MAX_JOB_SECONDS", defaults.job_seconds)?,
            query_rows: whole("TOOLSITE_MAX_QUERY_ROWS", defaults.query_rows)?,
            memory_mb: whole("TOOLSITE_MAX_MEMORY_MB", defaults.memory_mb)?,
        })
    }
}

/// The limits one app's calls run under now, read from its meta per call
/// so a redeployed `[limits]` applies to the next request.
pub async fn of(config: &Config, app: &str) -> Effective {
    let meta = crate::content::store::read_meta(config, app).await;
    config.limits.effective(meta.limits.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_app_that_asks_for_nothing_gets_the_limits_every_app_had() {
        let effective = Ceilings::default().effective(None);
        assert_eq!(effective.request, Guards::default());
        assert_eq!(effective.request.fuel, Some(DEFAULT_REQUEST_FUEL));
        assert_eq!(effective.request.wall_clock, Duration::from_secs(5));
        assert_eq!(effective.request.query_rows, 1_000);
        assert_eq!(effective.job.fuel, Some(DEFAULT_JOB_FUEL));
        assert_eq!(effective.job.wall_clock, Duration::from_secs(60));
        assert_eq!(effective.job.memory_bytes, 128 * 1024 * 1024);
    }

    #[test]
    fn what_an_app_asks_for_applies_up_to_the_ceiling_and_no_further() {
        let asked = Asked {
            request_seconds: Some(30),
            job_seconds: Some(3600),
            query_rows: Some(10_000),
            memory_mb: Some(4096),
            ..Asked::default()
        };
        let ceilings = Ceilings::default();
        let effective = ceilings.effective(Some(&asked));
        assert_eq!(effective.request.wall_clock, Duration::from_secs(30));
        assert_eq!(effective.job.wall_clock, Duration::from_secs(900));
        assert_eq!(effective.request.query_rows, 10_000);
        assert_eq!(effective.job.memory_bytes, 1024 * 1024 * 1024);
        let notes = ceilings.clamped(&asked);
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("job_seconds: asked for 3600, the site allows 900")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("memory_mb: asked for 4096, the site allows 1024")), "{notes:?}");
    }

    #[test]
    fn a_site_that_meters_no_fuel_runs_on_the_clock_alone() {
        let ceilings = Ceilings::from_env(|name| (name == "TOOLSITE_MAX_REQUEST_FUEL").then(|| "none".to_string())).unwrap();
        let asked = Asked { request_fuel: Some(5), ..Asked::default() };
        let effective = ceilings.effective(Some(&asked));
        assert_eq!(effective.request.fuel, None);
        assert_eq!(effective.job.fuel, Some(DEFAULT_JOB_FUEL));
        assert!(ceilings.clamped(&asked)[0].contains("meters no fuel"));
    }

    #[test]
    fn a_ceiling_below_a_default_lowers_the_default_too() {
        let ceilings = Ceilings { request_seconds: 2, ..Ceilings::default() };
        assert_eq!(ceilings.effective(None).request.wall_clock, Duration::from_secs(2));
    }

    #[test]
    fn a_zero_is_refused_rather_than_making_every_call_fail() {
        assert!(Asked { query_rows: Some(0), ..Asked::default() }.check().is_err());
        assert!(Ceilings::from_env(|name| (name == "TOOLSITE_MAX_QUERY_ROWS").then(|| "0".to_string())).is_err());
    }
}
