//! Source/URI-scoped counters, compiled only for regression tests.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Work {
    Parse,
    Source,
    Symbols,
    References,
    IndexWrite,
}
struct Counts {
    marker: String,
    values: [AtomicUsize; 5],
    hook: Option<Box<dyn Fn(Work) + Send + Sync>>,
}
static PROBES: LazyLock<Mutex<Vec<Weak<Counts>>>> = LazyLock::new(Mutex::default);
pub struct Probe(Arc<Counts>);
impl Probe {
    pub fn new(marker: &str) -> Self {
        Self::create(marker, None)
    }
    pub fn with_hook(marker: &str, hook: impl Fn(Work) + Send + Sync + 'static) -> Self {
        Self::create(marker, Some(Box::new(hook)))
    }
    fn create(marker: &str, hook: Option<Box<dyn Fn(Work) + Send + Sync>>) -> Self {
        let counts = Arc::new(Counts {
            marker: marker.into(),
            hook,
            values: std::array::from_fn(|_| AtomicUsize::new(0)),
        });
        PROBES.lock().unwrap().push(Arc::downgrade(&counts));
        Self(counts)
    }
    pub fn count(&self, work: Work) -> usize {
        self.0.values[work as usize].load(Ordering::SeqCst)
    }
    pub fn all(&self) -> [usize; 5] {
        std::array::from_fn(|i| self.0.values[i].load(Ordering::SeqCst))
    }
}
pub fn record(work: Work, source_or_uri: &str) {
    let active = {
        let mut probes = PROBES.lock().unwrap();
        probes.retain(|probe| probe.strong_count() > 0);
        probes.iter().filter_map(Weak::upgrade).collect::<Vec<_>>()
    };
    for probe in active {
        if source_or_uri.contains(&probe.marker) {
            probe.values[work as usize].fetch_add(1, Ordering::SeqCst);
            if let Some(hook) = &probe.hook {
                hook(work);
            }
        }
    }
}
