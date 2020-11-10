// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod enrollment;
pub mod error;
mod evaluator;
pub use error::{Error, Result};
mod client;
mod config;
mod matcher;
mod persistence;
mod sampling;
#[cfg(debug_assertions)]
pub use evaluator::evaluate_enrollment;

use client::{create_client, SettingsClient};
pub use config::RemoteSettingsConfig;
use enrollment::{
    get_enrollments, get_global_user_participation, opt_in_with_branch, opt_out, reset_enrollment,
    set_global_user_participation, update_enrollments,
};
pub use matcher::AppContext;
use once_cell::sync::OnceCell;
use persistence::{Database, StoreId};
use serde_derive::*;
use std::path::PathBuf;
use uuid::Uuid;

const DEFAULT_TOTAL_BUCKETS: u32 = 10000;
const DB_KEY_NIMBUS_ID: &str = "nimbus-id";

/// Nimbus is the main struct representing the experiments state
/// It should hold all the information needed to communicate a specific user's
/// experimentation status
pub struct NimbusClient {
    settings_client: Box<dyn SettingsClient + Send>,
    available_randomization_units: AvailableRandomizationUnits,
    app_context: AppContext,
    db: OnceCell<Database>,
    db_path: PathBuf,
}

impl NimbusClient {
    // This constructor *must* not do any kind of I/O since it might be called on the main
    // thread in the gecko Javascript stack, hence the use of OnceCell for the db.
    pub fn new<P: Into<PathBuf>>(
        app_context: AppContext,
        db_path: P,
        config: RemoteSettingsConfig,
        available_randomization_units: AvailableRandomizationUnits,
    ) -> Result<Self> {
        let settings_client = create_client(config)?;
        Ok(Self {
            settings_client,
            available_randomization_units,
            app_context,
            db_path: db_path.into(),
            db: OnceCell::default(),
        })
    }

    // This is a little suspect but it's not clear what the right thing is.
    // Maybe it's OK we initially start with no experiments and just need to
    // wait for the app to schedule a regular `update_experiments()` call?
    // But for now, if we have no experiments we assume we have never
    // successfully hit our server, so should do that now.
    fn maybe_initial_experiment_fetch(&self) -> Result<()> {
        if !self.db()?.has_any(StoreId::Experiments)? {
            log::info!("No experiments in our database - fetching them");
            self.update_experiments()?;
        }
        Ok(())
    }

    pub fn get_experiment_branch(&self, slug: String) -> Result<Option<String>> {
        Ok(self
            .get_active_experiments()?
            .iter()
            .find(|e| e.slug == slug)
            .map(|e| e.branch_slug.clone()))
    }

    pub fn get_global_user_participation(&self) -> Result<bool> {
        // This is a bit smelly, but get_global_user_participation() needs a
        // writer so that the implementation of update_enrollments can pass one
        // and see the correct value.
        let db = self.db()?;
        let writer = db.write()?;
        get_global_user_participation(db, &writer)
    }

    pub fn set_global_user_participation(&self, flag: bool) -> Result<()> {
        let db = self.db()?;
        let mut writer = db.write()?;
        set_global_user_participation(db, &mut writer, flag)?;
        // Now update all enrollments based on this new opt in/out setting.
        update_enrollments(
            db,
            &mut writer,
            &self.nimbus_id()?,
            &self.available_randomization_units,
            &self.app_context,
        )?;
        writer.commit()?;
        Ok(())
    }

    pub fn get_active_experiments(&self) -> Result<Vec<EnrolledExperiment>> {
        self.maybe_initial_experiment_fetch()?;
        get_enrollments(self.db()?)
    }

    pub fn get_all_experiments(&self) -> Result<Vec<Experiment>> {
        self.maybe_initial_experiment_fetch()?;
        self.db()?.collect_all(StoreId::Experiments)
    }

    pub fn opt_in_with_branch(&self, experiment_slug: String, branch: String) -> Result<()> {
        opt_in_with_branch(self.db()?, &experiment_slug, &branch)
    }

    pub fn opt_out(&self, experiment_slug: String) -> Result<()> {
        opt_out(self.db()?, &experiment_slug)
    }

    pub fn reset_enrollment(&self, experiment_slug: String) -> Result<()> {
        let db = self.db()?;
        let mut writer = db.write()?;
        reset_enrollment(
            db,
            &mut writer,
            &experiment_slug,
            &self.nimbus_id()?,
            &self.available_randomization_units,
            &self.app_context,
        )?;
        writer.commit()?;
        Ok(())
    }

    pub fn update_experiments(&self) -> Result<()> {
        // I suspect we need to take some action when we find experiments we
        // previously had no longer exist? For now though, just nuke them all.
        log::info!("updating experiment list");
        let experiments = self.settings_client.get_experiments()?;
        let db = self.db()?;
        let mut writer = db.write()?;
        let exp_store = db.get_store(StoreId::Experiments);
        exp_store.clear(&mut writer)?;
        for experiment in &experiments {
            log::debug!("found experiment {}", experiment.slug);
            exp_store.put(&mut writer, &experiment.slug, experiment)?;
        }
        // Now update all enrollments based on the new set.
        update_enrollments(
            &db,
            &mut writer,
            &self.nimbus_id()?, // XXX - this might write but not in its own transaction!?
            &self.available_randomization_units,
            &self.app_context,
        )?;
        writer.commit()?;
        Ok(())
    }

    pub fn nimbus_id(&self) -> Result<Uuid> {
        let db = self.db()?;
        let mut writer = db.write()?;
        let store = db.get_store(StoreId::Meta);
        Ok(match store.get(&writer, DB_KEY_NIMBUS_ID)? {
            Some(nimbus_id) => nimbus_id,
            None => {
                let nimbus_id = Uuid::new_v4();
                store.put(&mut writer, DB_KEY_NIMBUS_ID, &nimbus_id)?;
                writer.commit()?;
                nimbus_id
            }
        })
    }

    // Sets the nimbus ID - TEST ONLY - should not be exposed to real clients.
    // (Useful for testing so you can have some control over what experiments
    // are enrolled)
    pub fn set_nimbus_id(&self, uuid: &Uuid) -> Result<()> {
        let db = self.db()?;
        let mut writer = db.write()?;
        db.get_store(StoreId::Meta)
            .put(&mut writer, DB_KEY_NIMBUS_ID, uuid)?;
        writer.commit()?;
        Ok(())
    }

    fn db(&self) -> Result<&Database> {
        self.db.get_or_try_init(|| Database::new(&self.db_path))
    }
}

#[derive(Debug, Clone)]
pub struct EnrolledExperiment {
    pub slug: String,
    pub user_facing_name: String,
    pub user_facing_description: String,
    pub branch_slug: String,
}

/// This is the currently supported major schema version.
pub const SCHEMA_VERSION: u32 = 1;
// XXX: In the future it would be nice if this lived in its own versioned crate so that
// the schema could be decoupled from the sdk so that it can be iterated on while the
// sdk depends on a particular version of the schema through the cargo.toml.

#[derive(Deserialize, Serialize, Debug, Default, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Experiment {
    pub schema_version: String,
    pub slug: String,
    pub application: String,
    pub user_facing_name: String,
    pub user_facing_description: String,
    pub is_enrollment_paused: bool,
    pub bucket_config: BucketConfig,
    pub probe_sets: Vec<String>,
    pub branches: Vec<Branch>,
    pub targeting: Option<String>,
    pub start_date: Option<String>, // TODO: Use a date format here
    pub end_date: Option<String>,   // TODO: Use a date format here
    pub proposed_duration: Option<u32>,
    pub proposed_enrollment: u32,
    pub reference_branch: Option<String>,
    // N.B. records in RemoteSettings will have `id` and `filter_expression` fields,
    // but we ignore them because they're for internal use by RemoteSettings.
}

#[derive(Deserialize, Serialize, Debug, Default, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FeatureConfig {
    pub feature_id: String,
    pub enabled: bool,
    // There is a nullable `value` field that can contain key-value config options
    // that modify the behaviour of an application feature, but we don't support
    // it yet and the details are still being finalized, so we ignore it for now.
}

#[derive(Deserialize, Serialize, Debug, Default, Clone, PartialEq)]
pub struct Branch {
    pub slug: String,
    pub ratio: u32,
    pub feature: Option<FeatureConfig>,
}

fn default_buckets() -> u32 {
    DEFAULT_TOTAL_BUCKETS
}

#[derive(Deserialize, Serialize, Debug, Default, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BucketConfig {
    pub randomization_unit: RandomizationUnit,
    pub namespace: String,
    pub start: u32,
    pub count: u32,
    #[serde(default = "default_buckets")]
    pub total: u32,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RandomizationUnit {
    NimbusId,
    ClientId,
}

impl Default for RandomizationUnit {
    fn default() -> Self {
        Self::NimbusId
    }
}

#[derive(Default)]
pub struct AvailableRandomizationUnits {
    pub client_id: Option<String>,
    dummy: i8, // See comments in nimbus.idl for why this hacky item exists.
}

impl AvailableRandomizationUnits {
    // Use ::with_client_id when you want to specify one, or use
    // Default::default if you don't!
    pub fn with_client_id(client_id: &str) -> Self {
        Self {
            client_id: Some(client_id.to_string()),
            dummy: 0,
        }
    }

    pub fn get_value<'a>(
        &'a self,
        nimbus_id: &'a str,
        wanted: &'a RandomizationUnit,
    ) -> Option<&'a str> {
        match wanted {
            RandomizationUnit::NimbusId => Some(nimbus_id),
            RandomizationUnit::ClientId => self.client_id.as_deref(),
        }
    }
}

include!(concat!(env!("OUT_DIR"), "/nimbus.uniffi.rs"));
