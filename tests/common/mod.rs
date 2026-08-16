use std::path::Path;

use tempfile::TempDir;

/// A scratch directory for one integration test, removed on drop.
///
/// Backed by `tempfile::TempDir`, which uses an OS-provided unique name rather
/// than the previous timestamp-plus-pid scheme. That closes a real (if
/// unlikely) collision window: two tests entering `new` within the same
/// nanosecond tick in the same process would previously have shared a
/// directory. It also removes the scratch tree even when the test panics while
/// holding it.
pub struct TestDirectory {
    directory: TempDir,
}

impl TestDirectory {
    pub fn new(name: &str) -> Self {
        let directory = tempfile::Builder::new()
            .prefix(&format!("stratadb-integration-{name}-"))
            .tempdir()
            .expect("create scratch directory");
        Self { directory }
    }

    pub fn path(&self) -> &Path {
        self.directory.path()
    }
}
