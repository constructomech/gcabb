use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing_subscriber::EnvFilter;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DiagnosticEvent {
    pub timestamp: String,
    pub category: String,
    pub operation: String,
    pub elapsed_ms: Option<u64>,
    pub session_id: Option<String>,
    pub success: bool,
    pub details: Value,
}

pub trait DiagnosticsSink: Send + Sync {
    fn record(&self, event: DiagnosticEvent);
}

#[derive(Default)]
pub struct TracingDiagnostics;

impl DiagnosticsSink for TracingDiagnostics {
    fn record(&self, event: DiagnosticEvent) {
        let details = redact(event.details);
        if event.success {
            tracing::info!(
                category = event.category,
                operation = event.operation,
                elapsed_ms = event.elapsed_ms,
                session_id = event.session_id,
                details = %details,
                "diagnostic operation completed"
            );
        } else {
            tracing::error!(
                category = event.category,
                operation = event.operation,
                elapsed_ms = event.elapsed_ms,
                session_id = event.session_id,
                details = %details,
                "diagnostic operation failed"
            );
        }
    }
}

#[derive(Clone, Default)]
pub struct MemoryDiagnostics {
    events: Arc<Mutex<Vec<DiagnosticEvent>>>,
}

impl MemoryDiagnostics {
    #[must_use]
    pub fn events(&self) -> Vec<DiagnosticEvent> {
        self.events.lock().map_or_else(
            |_| {
                tracing::error!("memory diagnostics lock poisoned");
                Vec::new()
            },
            |events| events.clone(),
        )
    }
}

impl DiagnosticsSink for MemoryDiagnostics {
    fn record(&self, mut event: DiagnosticEvent) {
        event.details = redact(event.details);
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        } else {
            tracing::error!("memory diagnostics lock poisoned");
        }
    }
}

/// Initializes the process-wide structured tracing subscriber.
///
/// # Errors
///
/// Returns an error when another global subscriber has already been installed.
/// Installs the structured tracing subscriber.
///
/// Logs go to stderr so that stdout stays reserved for a command's actual
/// output. Without this, `--version` and the update commands interleave log
/// lines with the value a caller is trying to read.
pub fn init_tracing(
    default_filter: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init()
}

#[must_use]
pub fn redact(value: Value) -> Value {
    match value {
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| {
                    if is_sensitive_key(&key) {
                        (key, Value::String("<redacted>".to_owned()))
                    } else {
                        (key, redact(value))
                    }
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(redact).collect()),
        other => other,
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
    normalized.contains("token")
        || normalized.contains("authorization")
        || normalized.contains("password")
        || normalized.contains("secret")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn recursively_redacts_sensitive_fields() {
        let redacted = redact(json!({
            "githubToken": "one",
            "nested": [{"authorization": "two", "safe": "visible"}]
        }));

        assert_eq!(redacted["githubToken"], "<redacted>");
        assert_eq!(redacted["nested"][0]["authorization"], "<redacted>");
        assert_eq!(redacted["nested"][0]["safe"], "visible");
    }
}
