use std::path::Path;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use crate::config::{DEFAULT_MODEL, DEFAULT_PRESERVE_N};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Compaction {
    #[serde(default = "default_preserve_n")]
    pub preserve_n: usize,
}

fn default_preserve_n() -> usize {
    DEFAULT_PRESERVE_N
}

impl Default for Compaction {
    fn default() -> Self {
        Self {
            preserve_n: DEFAULT_PRESERVE_N,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    // api_key is intentionally ALWAYS serialized (no skip_serializing_if),
    // even when empty, so the on-disk round-trip is unambiguous.
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub compaction: Compaction,
}

fn default_model() -> String {
    DEFAULT_MODEL.to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: DEFAULT_MODEL.to_string(),
            name: String::new(),
            compaction: Compaction::default(),
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_vec_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }
}
