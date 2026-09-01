//! Shared process context: config, registry, profile set, and device/store
//! resolution. Every subcommand and every service builds on this so that "which
//! device did you mean?" is answered exactly once, in one place (§3.1).

use anyhow::{Context as _, Result};
use conminer_core::clock::{self, SharedClock};
use conminer_core::config::Config;
use conminer_core::framer::ProfileSet;
use conminer_core::pipeline::Pipeline;
use conminer_core::store::{DeviceRow, DeviceStore, IdentityKind, Registry};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct App {
    pub config: Config,
    pub profiles: Arc<ProfileSet>,
    pub clock: SharedClock,
    pub data_dir: PathBuf,
}

impl App {
    pub fn load(config_path: Option<&Path>, data_dir: Option<&Path>) -> Result<Self> {
        let config = match config_path {
            Some(p) => Config::load(p).with_context(|| format!("loading {}", p.display()))?,
            None => Config::load_or_default()?,
        };
        let data_dir = data_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(|| config.paths.data_dir.clone());
        let profiles = Arc::new(ProfileSet::load(Some(&config.paths.profiles_dir))?);
        Ok(Self {
            config,
            profiles,
            clock: clock::system(),
            data_dir,
        })
    }

    pub fn registry(&self) -> Result<Registry> {
        Ok(Registry::open(&self.data_dir)?)
    }

    /// Resolve a selector to one device (§3.1), or create the synthetic device a
    /// file ingest belongs to.
    ///
    /// A file gets its own device keyed on its absolute path, so re-ingesting the
    /// same artifact lands in the same store and `diff_sessions` between two runs
    /// of the same job works without any extra bookkeeping.
    pub fn resolve_or_create(
        &self,
        reg: &mut Registry,
        selector: Option<&str>,
        for_file: Option<&Path>,
    ) -> Result<DeviceRow> {
        if let Some(sel) = selector {
            return Ok(reg.resolve(sel)?);
        }
        let path = for_file.context("a device selector is required")?;
        let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let canonical = format!("file:{}", abs.display());
        let now = self.clock.now_wall_ms();
        if let Some(d) = reg.device_by_canonical(&canonical)? {
            return Ok(d);
        }
        let d = reg.upsert_device(&canonical, None, IdentityKind::ById, None, now)?;
        Ok(d)
    }

    pub fn open_store(&self, reg: &Registry, dev: &DeviceRow) -> Result<DeviceStore> {
        let path = reg.device_db_path(dev);
        let fts = self.config.fts_for(dev.display_name());
        Ok(DeviceStore::open(&path, &dev.canonical, fts)?)
    }

    pub fn open_pipeline(
        &self,
        reg: &Registry,
        dev: &DeviceRow,
        profile_override: Option<&str>,
    ) -> Result<Pipeline> {
        let store = self.open_store(reg, dev)?;
        let pinned = profile_override.or(dev.pinned_profile.as_deref());
        Ok(Pipeline::new(
            store,
            self.profiles.clone(),
            self.config.clone(),
            dev.display_name(),
            pinned,
            self.clock.clone(),
        )?)
    }
}
