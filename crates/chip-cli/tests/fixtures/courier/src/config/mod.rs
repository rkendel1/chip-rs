//! Layered configuration.
//!
//! Text is parsed into a [`RawConfig`] (sections of string keys), layers are merged key by key
//! (defaults, then the file, then the environment), and the merged result is read into the typed
//! [`Config`]. Unknown sections and keys are reported as warnings and otherwise ignored, so a
//! configuration written for a newer release still loads on an older one.

pub mod defaults;
pub mod env;
pub mod layers;
pub mod parser;
pub mod render;
pub mod schema;
pub mod validate;

pub use layers::Layers;
pub use parser::RawConfig;
pub use schema::{
    AuthSection, CircuitSection, ClientSection, Config, JournalSection, RateLimitSection,
    RouteSection, SectionSpec, SECTIONS,
};

use crate::error::CourierError;

/// A configuration plus everything that was ignored while reading it.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub config: Config,
    pub warnings: Vec<String>,
}

/// Defaults, then `file_text`, then the given environment pairs.
pub fn load(file_text: &str, env: &[(String, String)]) -> Result<Loaded, CourierError> {
    let file = parser::parse(file_text)?;
    let mut layers = Layers::new();
    layers.push("defaults", defaults::default_raw());
    layers.push("file", file);
    layers.push("env", env::from_pairs(env));
    let merged = layers.merge();
    let loaded = schema::Config::from_raw(&merged)?;
    validate::check(&loaded.config)?;
    Ok(loaded)
}

/// Defaults only.
pub fn load_defaults() -> Loaded {
    load("", &[]).expect("the built-in defaults are valid")
}
