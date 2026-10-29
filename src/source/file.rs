use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};

use super::FrameSource;

pub struct FileSource {
    path: PathBuf,
}

impl FileSource {
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
        }
    }
}

impl FrameSource for FileSource {
    fn capture(&self, _timeout: Duration) -> Result<Arc<[u8]>> {
        let jpeg =
            fs::read(&self.path).with_context(|| format!("reading {}", self.path.display()))?;
        ensure!(
            jpeg.starts_with(&[0xFF, 0xD8]),
            "{} is not a JPEG",
            self.path.display()
        );
        Ok(jpeg.into())
    }
}
