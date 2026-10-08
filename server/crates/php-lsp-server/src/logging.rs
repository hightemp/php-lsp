//! Process logging with an explicit LSP override and the original startup filter.

use serde_json::Value;
use std::sync::{Arc, Mutex};
use tracing::Level;
use tracing_subscriber::{reload, EnvFilter, Registry};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LogLevelSetting {
    #[default]
    Inherit,
    Override(Level),
    Invalid,
}

impl LogLevelSetting {
    pub(crate) fn parse(value: Option<&Value>) -> Self {
        match value {
            None => Self::Inherit,
            Some(Value::String(raw)) => match raw.trim().to_ascii_lowercase().as_str() {
                "error" => Self::Override(Level::ERROR),
                "warn" => Self::Override(Level::WARN),
                "info" => Self::Override(Level::INFO),
                "debug" => Self::Override(Level::DEBUG),
                "trace" => Self::Override(Level::TRACE),
                _ => {
                    tracing::warn!(
                        "Ignoring invalid logLevel; expected error, warn, info, debug or trace"
                    );
                    Self::Invalid
                }
            },
            Some(_) => {
                tracing::warn!("Ignoring invalid logLevel; expected a level string");
                Self::Invalid
            }
        }
    }
}

#[derive(Default)]
struct AppliedFilter {
    generation: Option<u64>,
    level: Option<Level>,
}

struct FilterState {
    handle: reload::Handle<EnvFilter, Registry>,
    startup: EnvFilter,
    applied: Mutex<AppliedFilter>,
}

/// A handle belonging to one subscriber; it never installs a global subscriber.
#[derive(Clone)]
pub struct RuntimeLogFilter {
    state: Arc<FilterState>,
}

impl RuntimeLogFilter {
    /// Return the reloadable layer and its backend handle. Keep the layer in the
    /// application's subscriber and pass the handle to `PhpLspBackend::with_log_filter`.
    pub fn new(startup: EnvFilter) -> (reload::Layer<EnvFilter, Registry>, Self) {
        let (layer, handle) = reload::Layer::new(startup.clone());
        (
            layer,
            Self {
                state: Arc::new(FilterState {
                    handle,
                    startup,
                    applied: Mutex::new(AppliedFilter::default()),
                }),
            },
        )
    }

    pub(crate) fn apply(
        &self,
        generation: u64,
        setting: LogLevelSetting,
    ) -> Result<bool, reload::Error> {
        let mut current = self
            .state
            .applied
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if current
            .generation
            .is_some_and(|previous| generation <= previous)
        {
            return Ok(false);
        }
        let level = match setting {
            LogLevelSetting::Inherit => None,
            LogLevelSetting::Override(level) => Some(level),
            LogLevelSetting::Invalid => {
                current.generation = Some(generation);
                return Ok(false);
            }
        };
        let changed = current.level != level;
        if changed {
            let filter = level
                .map(|level| EnvFilter::new(level.as_str()))
                .unwrap_or_else(|| self.state.startup.clone());
            self.state.handle.reload(filter)?;
            current.level = level;
        }
        current.generation = Some(generation);
        Ok(changed)
    }
}

#[cfg(test)]
#[path = "logging_tests.rs"]
pub(crate) mod tests;
