//! A whole conminer in a temp directory: registry, device store, pipeline.
//!
//! Store and ingest tests need a real database — an in-memory SQLite would hide
//! exactly the failures those tests exist to catch (WAL recovery, file growth,
//! prune-vs-cursor races). So the rig is always on disk, in a `tempfile::TempDir`
//! that cleans itself up.

use conminer_core::clock::{SharedClock, StepClock};
use conminer_core::config::Config;
use conminer_core::framer::ProfileSet;
use conminer_core::pipeline::Pipeline;
use conminer_core::store::{DeviceRow, DeviceStore, IdentityKind, Registry};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Rig {
    pub dir: tempfile::TempDir,
    pub config: Config,
    pub profiles: Arc<ProfileSet>,
    pub clock: SharedClock,
    /// The same clock, concretely, so a test can move time.
    ///
    /// Some states are only reachable by waiting -- "silent past the hung
    /// threshold", "quiet long enough that `still producing output` is a lie" --
    /// and a `SharedClock` can only be read. Holding the `StepClock` too lets a
    /// suite reach them without sleeping.
    step: Arc<StepClock>,
}

impl Default for Rig {
    fn default() -> Self {
        Self::new()
    }
}

impl Rig {
    pub fn new() -> Self {
        Self::with_config(Config::default())
    }

    pub fn with_config(mut config: Config) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        config.paths.data_dir = dir.path().to_path_buf();
        let step = Arc::new(StepClock::default());
        Self {
            dir,
            config,
            profiles: Arc::new(ProfileSet::builtin().expect("built-in profiles")),
            // Deterministic clock: a corpus replayed twice must produce equal
            // fingerprints (§8.4), which a wall clock would break.
            clock: step.clone(),
            step,
        }
    }

    /// Move time forward without reading the clock.
    pub fn advance_ms(&self, ms: i64) {
        self.step.advance_ms(ms);
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn registry(&self) -> Registry {
        Registry::open(self.dir.path()).expect("registry")
    }

    /// Register a device and return its row.
    pub fn device(&self, canonical: &str) -> DeviceRow {
        let mut reg = self.registry();
        reg.upsert_device(canonical, None, IdentityKind::ById, None, 1_000)
            .expect("upsert")
    }

    pub fn store_path(&self, dev: &DeviceRow) -> PathBuf {
        self.dir.path().join(&dev.db_file)
    }

    pub fn store(&self, dev: &DeviceRow) -> DeviceStore {
        DeviceStore::open(
            &self.store_path(dev),
            &dev.canonical,
            self.config.search.fts,
        )
        .expect("device store")
    }

    /// A pipeline on a fresh device, ready to `feed`.
    pub fn pipeline(&self, canonical: &str, profile: Option<&str>) -> Pipeline {
        let dev = self.device(canonical);
        let store = self.store(&dev);
        Pipeline::new(
            store,
            self.profiles.clone(),
            self.config.clone(),
            &dev.canonical,
            profile,
            self.clock.clone(),
        )
        .expect("pipeline")
    }

    /// Feed text through a fresh pipeline in one live session and hand back the
    /// store for querying.
    pub fn ingest_text(&self, canonical: &str, profile: Option<&str>, text: &str) -> DeviceStore {
        let mut p = self.pipeline(canonical, profile);
        p.begin_session(
            conminer_core::store::SessionSource::File,
            Some("test"),
            None,
            None,
        )
        .expect("session");
        p.feed(text.as_bytes()).expect("feed");
        p.finish().expect("finish");
        p.into_store()
    }

    /// Write a file into the rig's temp dir and return its path.
    pub fn write_file(&self, name: &str, contents: &[u8]) -> PathBuf {
        let p = self.dir.path().join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&p, contents).expect("write");
        p
    }

    /// Write a gzip-compressed file and return its path.
    pub fn write_gzip(&self, name: &str, contents: &[u8]) -> PathBuf {
        use flate2::write::GzEncoder;
        use std::io::Write;
        let p = self.dir.path().join(name);
        let f = std::fs::File::create(&p).expect("create");
        let mut e = GzEncoder::new(f, flate2::Compression::fast());
        e.write_all(contents).expect("gz write");
        e.finish().expect("gz finish");
        p
    }
}
