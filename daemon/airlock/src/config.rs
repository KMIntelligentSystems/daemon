use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub budget: Budget,
    pub models: Models,
    pub hosts: Hosts,
    #[serde(default)]
    pub schedule: Schedule,
    pub sources: HashMap<String, Source>,
    #[serde(default)]
    pub series: Vec<SeriesCfg>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Budget {
    pub max_tool_calls: u32,
    pub wall_clock_secs: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Models {
    pub allowlist: Vec<String>,
    pub default: String,
}

#[derive(Debug, Deserialize)]
pub struct Hosts {
    pub allowlist: Vec<String>,
}

/// Config for the built-in --schedule interval loop.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct Schedule {
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
    #[serde(default)]
    pub entries: Vec<ScheduleEntry>,
}

fn default_poll_secs() -> u64 {
    3600
}

#[derive(Debug, Deserialize, Clone)]
pub struct ScheduleEntry {
    pub source: String,
    pub targets: Vec<String>,
    #[serde(default)]
    pub series: Option<Vec<String>>,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Source {
    pub host: String,
    pub base_url: String,
    pub api_key_env: String,
    #[serde(default)]
    pub default_series: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SeriesCfg {
    pub id: String,
    pub source: String,
    pub provider_series_id: String,
    pub unit: String,
    pub seasonal_adjustment: String,
    pub lead_time_months: f64,
    pub min_value: f64,
    pub max_value: f64,
}

impl Config {
    pub fn load(path: &str) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {path}"))?;
        let cfg: Config = toml::from_str(&text).context("parsing config.toml")?;
        Ok(cfg)
    }

    pub fn series_by_id(&self, id: &str) -> Option<&SeriesCfg> {
        self.series.iter().find(|s| s.id == id)
    }

    pub fn host_allowed(&self, host: &str) -> bool {
        self.hosts.allowlist.iter().any(|h| h == host)
    }

    pub fn model_allowed(&self, model: &str) -> bool {
        self.models.allowlist.iter().any(|m| m == model)
    }
}
