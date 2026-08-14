use std::path::{Path, PathBuf};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileUriError {
    path: PathBuf,
    message: String,
}

impl FileUriError {
    fn new(path: &Path, message: impl Into<String>) -> Self {
        Self {
            path: path.to_path_buf(),
            message: message.into(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Display for FileUriError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "failed to convert {} to file URI: {}",
            self.path.display(),
            self.message
        )
    }
}

impl std::error::Error for FileUriError {}

pub fn path_to_uri(path: &Path) -> Result<String, FileUriError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|err| FileUriError::new(path, err.to_string()))?
            .join(path)
    };

    Url::from_file_path(&absolute)
        .map(|url| url.to_string())
        .map_err(|_| FileUriError::new(&absolute, "path is not representable as a file URI"))
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let url = Url::parse(uri).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    url.to_file_path().ok()
}

#[cfg(test)]
#[path = "uri_tests.rs"]
mod tests;
