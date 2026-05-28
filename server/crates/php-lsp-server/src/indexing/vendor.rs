//! Vendor indexing helpers.

use super::super::*;

#[derive(Debug, Clone)]
pub(crate) struct VendorAutoloadCacheEntry {
    pub(crate) map: VendorAutoloadMap,
}

#[derive(Debug, Default)]
pub(crate) struct VendorAutoloadCache {
    pub(crate) by_vendor_dir: HashMap<PathBuf, VendorAutoloadCacheEntry>,
}

impl VendorAutoloadCache {
    pub(crate) fn clear(&mut self) {
        self.by_vendor_dir.clear();
    }
}

#[derive(Debug)]
pub(crate) struct VendorFileLru {
    pub(crate) capacity: usize,
    uris: VecDeque<String>,
}

impl Default for VendorFileLru {
    fn default() -> Self {
        Self {
            capacity: VENDOR_FILE_LRU_CAPACITY,
            uris: VecDeque::new(),
        }
    }
}

impl VendorFileLru {
    #[cfg(test)]
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            uris: VecDeque::new(),
        }
    }

    pub(crate) fn touch(&mut self, uri: String) -> Vec<String> {
        if let Some(position) = self.uris.iter().position(|existing| existing == &uri) {
            self.uris.remove(position);
        }
        self.uris.push_back(uri);

        let mut evicted = Vec::new();
        while self.uris.len() > self.capacity {
            if let Some(uri) = self.uris.pop_front() {
                evicted.push(uri);
            }
        }
        evicted
    }

    pub(crate) fn remove(&mut self, uri: &str) {
        if let Some(position) = self.uris.iter().position(|existing| existing == uri) {
            self.uris.remove(position);
        }
    }

    pub(crate) fn clear(&mut self) -> Vec<String> {
        self.uris.drain(..).collect()
    }
}
