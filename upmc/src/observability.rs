//! Structured fallback events. Payloads must contain diagnostics, never credentials.
use std::collections::BTreeMap;
use std::fmt::Display;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

static SEQUENCE: AtomicU64 = AtomicU64::new(1);
static COUNTERS: OnceLock<Mutex<BTreeMap<&'static str, u64>>> = OnceLock::new();

// These degradations cannot authorize unverified content or change user intent.
// All unlisted identifiers deliberately default to ERROR.
const WARN_ALLOWLIST: &[&str] = &[
    "cleanup.committed", // Commit was verified; a journal/backup remains for later cleanup.
    "cleanup.optional",  // Only an unused temporary artifact remains.
    "install.legacy_location", // Same installation retained after a failed relocation.
];

pub fn event(
    identifier: &'static str,
    reason: &str,
    original_error: impl Display,
    primary_path: impl Display,
    fallback_path: impl Display,
    context_id: impl Display,
) {
    let count = {
        let mut counters = COUNTERS
            .get_or_init(Default::default)
            .lock()
            .expect("fallback counter mutex poisoned");
        let count = counters.entry(identifier).or_default();
        *count += 1;
        *count
    };
    let record = serde_json::json!({
        "event": "fallback", "identifier": identifier,
        "level": if WARN_ALLOWLIST.contains(&identifier) { "WARN" } else { "ERROR" },
        "reason": reason, "original_error": original_error.to_string(),
        "primary_path": primary_path.to_string(), "fallback_path": fallback_path.to_string(),
        "context_id": context_id.to_string(), "process_id": std::process::id(),
        "sequence": SEQUENCE.fetch_add(1, Ordering::Relaxed), "counter": count,
    });
    let line = record.to_string();
    // Release GUI processes do not own a console. DebugView/Windows debuggers
    // receive every record regardless of stderr availability.
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn OutputDebugStringW(output: *const u16);
        }
        let wide: Vec<u16> = line.encode_utf16().chain([0]).collect();
        unsafe {
            OutputDebugStringW(wide.as_ptr());
        }
    }
    eprintln!("{line}");
    #[cfg(test)]
    EVENTS.with(|events| events.borrow_mut().push(record));
}

/// Destructors and optional cleanup cannot return errors; record the failure.
pub fn cleanup<E: Display>(result: Result<(), E>, path: impl Display) {
    if let Err(error) = result {
        event(
            "cleanup.optional",
            "unused artifact cleanup failed",
            format!("{error:#}"),
            &path,
            "retain artifact for later cleanup",
            &path,
        );
    }
}

/// A failed validation can only disable reuse, never authorize an artifact.
pub fn validated<T>(
    result: anyhow::Result<T>,
    identifier: &'static str,
    path: impl Display,
) -> bool {
    match result {
        Ok(_) => true,
        Err(error) => {
            event(
                identifier,
                "validation failed; reuse denied",
                format!("{error:#}"),
                &path,
                "require repair or deny operation",
                &path,
            );
            false
        }
    }
}

/// Retain ownership records after poisoning so cleanup can still own its child.
/// Callers must validate real process/file state before authorizing any action.
pub fn poisoned<T>(error: std::sync::PoisonError<T>, identifier: &'static str) -> T {
    event(
        identifier,
        "mutex poisoned; ownership retained for validated recovery",
        error.to_string(),
        "unpoisoned state",
        "revalidate retained state",
        identifier,
    );
    error.into_inner()
}

/// Preserve both failures when recovering an already failed operation.
pub fn recovery<T>(
    original: anyhow::Error,
    result: anyhow::Result<T>,
    primary: impl Display,
    fallback: impl Display,
) -> anyhow::Error {
    event(
        "recovery.attempt",
        "operation failed; recovery attempted",
        format!("{original:#}"),
        &primary,
        &fallback,
        &primary,
    );
    match result {
        Ok(_) => original,
        Err(error) => {
            event(
                "recovery.failed",
                "recovery failed",
                format!("{error:#}"),
                &primary,
                &fallback,
                &primary,
            );
            anyhow::anyhow!("{original:#}; recovery also failed: {error:#}")
        }
    }
}

/// Wall clocks before the epoch are valid names, not synthetic zero timestamps.
/// A process-local sequence also prevents simultaneous name collisions.
pub fn unique_id() -> String {
    unique_id_at(std::time::SystemTime::now())
}

pub fn unique_id_at(instant: std::time::SystemTime) -> String {
    let now = match instant.duration_since(std::time::UNIX_EPOCH) {
        Ok(value) => value.as_nanos().to_string(),
        Err(error) => format!("before-{}", error.duration().as_nanos()),
    };
    format!(
        "{}-{now}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
thread_local! { static EVENTS: std::cell::RefCell<Vec<serde_json::Value>> = const { std::cell::RefCell::new(Vec::new()) }; }

#[cfg(test)]
pub fn take_events() -> Vec<serde_json::Value> {
    EVENTS.with(|events| std::mem::take(&mut *events.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_preserves_context_and_root_cause() {
        take_events();
        cleanup(
            Err::<(), _>(
                anyhow::anyhow!("injected filesystem denial").context("temporary cleanup context"),
            ),
            "fixture.tmp",
        );
        let events = take_events();
        let error = events[0]["original_error"].as_str().unwrap();
        assert!(error.contains("temporary cleanup context"));
        assert!(
            error.contains("injected filesystem denial"),
            "cleanup root cause discarded: {error}"
        );
    }

    #[test]
    fn schema_severity_counter_and_error_chains_are_observable() {
        take_events();
        event(
            "test.fallback",
            "test reason",
            "primary error",
            "primary",
            "fallback",
            "request-1",
        );
        event(
            "test.fallback",
            "test reason",
            "second error",
            "primary",
            "fallback",
            "request-1",
        );
        event(
            "cleanup.optional",
            "temporary only",
            "busy",
            "temp",
            "retain",
            "request-1",
        );
        let events = take_events();
        assert_eq!(events[0]["level"], "ERROR");
        assert_eq!(events[2]["level"], "WARN");
        assert_eq!(
            events[1]["counter"].as_u64(),
            events[0]["counter"].as_u64().map(|n| n + 1)
        );
        for key in [
            "identifier",
            "reason",
            "original_error",
            "primary_path",
            "fallback_path",
            "context_id",
        ] {
            assert!(!events[0][key].as_str().unwrap().is_empty());
        }
        let error = recovery(
            anyhow::anyhow!("original"),
            Err::<(), _>(anyhow::anyhow!("rollback")),
            "new",
            "old",
        );
        assert!(error.to_string().contains("original"));
        assert!(error.to_string().contains("rollback"));
        assert_eq!(take_events().len(), 2);
    }

    #[test]
    fn successful_cleanup_is_silent_and_names_are_unique_concurrently() {
        take_events();
        cleanup::<std::io::Error>(Ok(()), "temp");
        assert!(take_events().is_empty());
        let names: std::collections::HashSet<_> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| (0..100).map(|_| unique_id()).collect::<Vec<_>>()))
                .collect();
            threads
                .into_iter()
                .flat_map(|thread| thread.join().unwrap())
                .collect()
        });
        assert_eq!(names.len(), 800);
    }
}
