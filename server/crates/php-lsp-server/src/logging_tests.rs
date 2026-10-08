use super::*;
use std::io::{self, Write};
use tracing_subscriber::prelude::*;

#[derive(Clone, Default)]
pub(crate) struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Capture {
    pub(crate) fn take(&self) -> String {
        String::from_utf8(std::mem::take(&mut *self.0.lock().unwrap())).unwrap()
    }
}

pub(crate) fn captured_filter(startup: &str) -> (RuntimeLogFilter, tracing::Dispatch, Capture) {
    let (layer, filter) = RuntimeLogFilter::new(EnvFilter::new(startup));
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::registry().with(layer).with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone()),
    );
    (filter, tracing::Dispatch::new(subscriber), capture)
}

fn apply(
    filter: &RuntimeLogFilter,
    dispatch: &tracing::Dispatch,
    generation: u64,
    setting: LogLevelSetting,
) -> Result<bool, reload::Error> {
    // Reload rebuilds callsite interest in the active subscriber's context.
    // Production uses a global subscriber; these tests keep it scoped.
    tracing::dispatcher::with_default(dispatch, || filter.apply(generation, setting))
}

fn probe(dispatch: &tracing::Dispatch) {
    tracing::dispatcher::with_default(dispatch, || {
        tracing::error!(target: "probe", "level-error");
        tracing::warn!(target: "probe", "level-warn");
        tracing::info!(target: "probe", "level-info");
        tracing::debug!(target: "probe", "level-debug");
        tracing::trace!(target: "probe", "level-trace");
    });
}

#[test]
fn all_advertised_levels_filter_actual_events() {
    let levels = [
        Level::ERROR,
        Level::WARN,
        Level::INFO,
        Level::DEBUG,
        Level::TRACE,
    ];
    let (filter, dispatch, capture) = captured_filter("off");
    for (i, level) in levels.iter().enumerate() {
        apply(
            &filter,
            &dispatch,
            i as u64,
            LogLevelSetting::Override(*level),
        )
        .unwrap();
        probe(&dispatch);
        let logs = capture.take();
        for (j, label) in ["error", "warn", "info", "debug", "trace"]
            .into_iter()
            .enumerate()
        {
            assert_eq!(
                logs.contains(&format!("level-{label}")),
                j <= i,
                "{level}: {logs}"
            );
        }
    }
}

#[test]
fn removal_restores_the_complete_startup_directives() {
    let (filter, dispatch, capture) = captured_filter("error,probe=debug");
    apply(
        &filter,
        &dispatch,
        1,
        LogLevelSetting::Override(Level::ERROR),
    )
    .unwrap();
    probe(&dispatch);
    assert!(!capture.take().contains("level-debug"));
    apply(&filter, &dispatch, 2, LogLevelSetting::Inherit).unwrap();
    probe(&dispatch);
    let logs = capture.take();
    assert!(logs.contains("level-debug"), "{logs}");
    assert!(!logs.contains("level-trace"), "{logs}");
    tracing::dispatcher::with_default(
        &dispatch,
        || tracing::debug!(target: "other", "other-debug"),
    );
    assert!(capture.take().is_empty());
}

#[test]
fn malformed_settings_preserve_the_last_valid_filter() {
    let (filter, dispatch, capture) = captured_filter("off");
    apply(
        &filter,
        &dispatch,
        0,
        LogLevelSetting::Override(Level::DEBUG),
    )
    .unwrap();
    for (generation, value) in [
        serde_json::json!(""),
        serde_json::json!("warning"),
        serde_json::json!("debug,probe=trace"),
        serde_json::json!("off"),
        serde_json::json!("4"),
        serde_json::json!(42),
        Value::Null,
    ]
    .iter()
    .enumerate()
    {
        let setting = LogLevelSetting::parse(Some(value));
        assert_eq!(setting, LogLevelSetting::Invalid, "{value}");
        assert!(!apply(&filter, &dispatch, generation as u64 + 1, setting).unwrap());
        probe(&dispatch);
        let logs = capture.take();
        assert!(logs.contains("level-debug"), "{value}: {logs}");
        assert!(!logs.contains("level-trace"));
    }
}

#[test]
fn level_normalization_and_absent_setting_are_distinct() {
    for (raw, expected) in [
        (" error ", Level::ERROR),
        ("WARN", Level::WARN),
        ("Info", Level::INFO),
        (" DEBUG ", Level::DEBUG),
        ("trace", Level::TRACE),
    ] {
        assert_eq!(
            LogLevelSetting::parse(Some(&serde_json::json!(raw))),
            LogLevelSetting::Override(expected)
        );
    }
    assert_eq!(LogLevelSetting::parse(None), LogLevelSetting::Inherit);
}

#[test]
fn a_superseded_update_cannot_restore_an_older_filter() {
    let (filter, dispatch, capture) = captured_filter("off");
    apply(
        &filter,
        &dispatch,
        2,
        LogLevelSetting::Override(Level::DEBUG),
    )
    .unwrap();
    for setting in [
        LogLevelSetting::Inherit,
        LogLevelSetting::Override(Level::ERROR),
    ] {
        assert!(!apply(&filter, &dispatch, 1, setting).unwrap());
        assert!(!apply(&filter, &dispatch, 2, setting).unwrap());
    }
    probe(&dispatch);
    assert!(capture.take().contains("level-debug"));
}

#[test]
fn independent_backend_filters_do_not_mutate_each_other() {
    let (first, first_dispatch, first_capture) = captured_filter("error");
    let (second, second_dispatch, second_capture) = captured_filter("error");
    apply(
        &first,
        &first_dispatch,
        1,
        LogLevelSetting::Override(Level::TRACE),
    )
    .unwrap();
    apply(
        &second,
        &second_dispatch,
        1,
        LogLevelSetting::Override(Level::WARN),
    )
    .unwrap();
    probe(&first_dispatch);
    probe(&second_dispatch);
    assert!(first_capture.take().contains("level-trace"));
    let second_logs = second_capture.take();
    assert!(second_logs.contains("level-warn"));
    assert!(!second_logs.contains("level-info"));
}

#[test]
fn concurrent_delayed_old_update_cannot_undo_newer_generation() {
    let (filter, dispatch, capture) = captured_filter("off");
    let entered = Arc::new(std::sync::Barrier::new(2));
    let resume = Arc::new(std::sync::Barrier::new(2));
    let old_filter = filter.clone();
    let old_dispatch = dispatch.clone();
    let old_entered = entered.clone();
    let old_resume = resume.clone();
    let old = std::thread::spawn(move || {
        old_entered.wait();
        old_resume.wait();
        apply(
            &old_filter,
            &old_dispatch,
            1,
            LogLevelSetting::Override(Level::TRACE),
        )
        .unwrap()
    });
    entered.wait();
    apply(
        &filter,
        &dispatch,
        2,
        LogLevelSetting::Override(Level::ERROR),
    )
    .unwrap();
    resume.wait();
    assert!(!old.join().unwrap());
    probe(&dispatch);
    let logs = capture.take();
    assert!(logs.contains("level-error"));
    assert!(!logs.contains("level-warn"), "stale update leaked: {logs}");
}
