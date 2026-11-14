//! File gates shared by controlled native stepping tests and fixtures.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A unique directory containing fixture gates and acknowledgements.
pub struct ControlDir(PathBuf);

impl Default for ControlDir
{
    fn default() -> Self
    {
        Self::new()
    }
}

impl ControlDir
{
    /// Create an empty directory for one scenario.
    ///
    /// # Panics
    /// Panics if creating the directory fails.
    #[must_use]
    pub fn new() -> Self
    {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ferros-step-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("create control directory");
        Self(path)
    }

    /// Path passed to the fixture.
    #[must_use]
    pub fn path(&self) -> &Path
    {
        &self.0
    }

    /// Open a gate or ask the fixture for an acknowledgement.
    ///
    /// # Panics
    /// Panics if writing the gate fails.
    pub fn release(&self, name: &str)
    {
        std::fs::write(self.0.join(name), b"go").expect("write gate");
    }

    /// Wait with a bounded deadline for a fixture acknowledgement.
    ///
    /// # Panics
    /// Panics if the acknowledgement does not arrive within ten seconds.
    pub fn wait(&self, name: &str)
    {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.0.join(name).exists() {
            assert!(Instant::now() < deadline, "missing fixture acknowledgement: {name}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Read an acknowledged result.
    ///
    /// # Panics
    /// Panics if reading the acknowledgement fails.
    #[must_use]
    pub fn read(&self, name: &str) -> String
    {
        std::fs::read_to_string(self.0.join(name)).expect("read acknowledgement")
    }
}

impl Drop for ControlDir
{
    fn drop(&mut self)
    {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
