use crate::{app::App, plugin::Plugin};
use shipyard::*;
use std::{
    any::{type_name, TypeId},
    collections::hash_map::Entry,
    collections::HashMap,
};
use tracing::*;

mod plugin_id;
mod workloads;
use plugin_id::PluginId;
use workloads::Workloads;

/// Name of app stage responsible for doing most app logic. Systems should be registered here by default.
pub const DEFAULT_STAGE: &str = "default";

/// Configure [App]s using the builder pattern
pub struct AppBuilder<'a> {
    pub app: &'a App,
    stage_workloads: Workloads,
    resets: Vec<WorkloadSystem>,
    /// track the plugins previously added to enable checking that plugin peer dependencies are satisified
    track_added_plugins: HashMap<TypeId, PluginId>,
    /// track the currently being used plugin ([PluginId] is a stack since some plugins add other plugins creating a nest)
    // TODO: Track "Plugin"s for each thing
    track_current_plugin: PluginId,
    /// take a record of type names as we come across them for diagnostics
    track_type_names: HashMap<TypeId, &'static str>,
    /// unique type id to list of plugin type ids that provided a value for it it
    track_uniques: HashMap<TypeId, Vec<PluginId>>,
    /// unique type id to list of (plugin type id, reason string)
    track_unique_dependencies: HashMap<TypeId, Vec<(PluginId, &'static str)>>,
    /// update component storage type id to list of (plugin type id, reason string)
    track_update_packed: HashMap<TypeId, Vec<(PluginId, &'static str)>>,
}

impl<'a> AppBuilder<'a> {
    pub fn new(app: &App) -> AppBuilder<'_> {
        let mut app_builder = AppBuilder::empty(app);
        app_builder.add_default_stages();
        app_builder
    }

    fn add_default_stages(&mut self) -> &mut Self {
        self.add_stage(DEFAULT_STAGE)
    }
}

pub struct AppWorkload(std::borrow::Cow<'static, str>);

impl AppWorkload {
    #[track_caller]
    pub fn run(&self, app: &App) {
        app.world.run_workload(&self.0).unwrap();
    }
}

impl<'a> AppBuilder<'a> {
    /// The general approach to running a Shipyard App is to create a new shipyard [World],
    /// then pass that world into [App::build]. Then, after adding your plugins, you can call this [AppBuilder::finish] to get an [App].
    ///
    /// With this App, you can:
    ///  1. Update any Uniques first or use [World::run_with_data] to prime the rest of the systems, then
    ///  2. Call the [App::update()] function, and
    ///  3. Pull any data you need out from the [World], and repeat.
    ///
    /// # Panics
    /// May panic if there are unmet unique dependencies or if there is an error adding workloads to shipyard.
    #[track_caller]
    pub fn finish(self) -> AppWorkload {
        self.finish_with_info().0
    }

    /// Finish [App] and report back each of the update stages with their [shipyard::info::WorkloadInfo].
    #[track_caller]
    pub fn finish_with_info(self) -> (AppWorkload, info::WorkloadInfo) {
        self.finish_with_info_named("update".into())
    }
    /// Finish [App] and report back each of the update stages with their [shipyard::info::WorkloadInfo].
    #[track_caller]
    pub(crate) fn finish_with_info_named(
        self,
        update_stage: std::borrow::Cow<'static, str>,
    ) -> (AppWorkload, info::WorkloadInfo) {
        let AppBuilder {
            app,
            resets,
            stage_workloads,
            track_added_plugins: _,
            track_current_plugin: _,
            track_type_names,
            track_update_packed: _,
            track_uniques,
            mut track_unique_dependencies,
        } = self;

        // trace! out Unique dependencies for diagnostics
        for (unique_type_id, provided_by) in track_uniques {
            let depended_on_by: Vec<(PluginId, &'static str)> = track_unique_dependencies
                .remove(&unique_type_id)
                .unwrap_or_default();

            let unique_type_name = *track_type_names.get(&unique_type_id).unwrap();
            if provided_by.len() > 1 {
                warn!(name = ?unique_type_name, ?provided_by, ?depended_on_by, "Unique defined by multiple Plugins, only the last registered plugin's unique will be used at startup");
            }

            // good to go
            trace!(name = ?unique_type_name, ?provided_by, ?depended_on_by, "Unique");
        }

        // assert there are no remaining unique dependencies
        let remaining_unique_deps = track_unique_dependencies
            .into_iter()
            .map(|(unique_type_id, dependents)| {
                let unique_type_name = *track_type_names.get(&unique_type_id).unwrap();

                format!("- {} required by: {:?}", unique_type_name, dependents)
            })
            .collect::<Vec<String>>();

        if !remaining_unique_deps.is_empty() {
            panic!(
                "Failed to finish app due to unmet unique dependencies:\n{}\n\n{}",
                remaining_unique_deps.join("\n"),
                " * You can add the unique using AppBuilder::add_unique or remove the AppBuilder::add_unique_dependency(s) to resolve this issue."
            );
        }

        let mut resets_workload = WorkloadBuilder::default();
        for reset_system in resets {
            resets_workload.with_system(reset_system);
        }

        let update_info: info::WorkloadInfo = stage_workloads
            .ordered
            .into_iter()
            .map(|(_, wb)| wb)
            .chain(std::iter::once(resets_workload))
            .fold(
                WorkloadBuilder::new(update_stage.clone()),
                |mut acc: WorkloadBuilder, mut wb: WorkloadBuilder| {
                    acc.append(&mut wb);
                    acc
                },
            )
            .add_to_world_with_info(&app.world)
            .unwrap();

        (AppWorkload(update_stage), update_info)
    }

    fn empty(app: &App) -> AppBuilder<'_> {
        AppBuilder {
            app,
            resets: Vec::new(),
            stage_workloads: Workloads::new(),
            track_added_plugins: Default::default(),
            track_current_plugin: Default::default(),
            track_type_names: Default::default(),
            track_uniques: Default::default(),
            track_unique_dependencies: Default::default(),
            track_update_packed: Default::default(),
        }
    }

    /// Lookup the type id while simultaneously storing the type name to be referenced later
    fn tracked_type_id_of<T: 'static>(&mut self) -> TypeId {
        let type_id = TypeId::of::<T>();
        self.track_type_names
            .entry(type_id)
            .or_insert_with(type_name::<T>);

        type_id
    }

    /// Update component `T`'s storage to be update_pack, and add [shipyard::sparse_set::SparseSet::clear_all_inserted_and_modified] as the last system.
    #[track_caller]
    pub fn update_pack<T: 'static + Send + Sync>(&mut self, reason: &'static str) -> &mut Self {
        let type_id = self.tracked_type_id_of::<T>();

        match self.track_update_packed.entry(type_id) {
            Entry::Occupied(mut list) => {
                // no need to pack again
                list.get_mut()
                    .push((self.track_current_plugin.clone(), reason));
            }
            Entry::Vacant(list) => {
                list.insert(vec![(self.track_current_plugin.clone(), reason)]);
                self.app.world.borrow::<ViewMut<T>>().unwrap().update_pack();
                self.resets.push(system!(reset_update_pack::<T>));
            }
        }

        self
    }

    /// Add a unique component
    #[track_caller]
    pub fn add_unique<T>(&mut self, component: T) -> &mut Self
    where
        T: Send + Sync + 'static,
    {
        self.app.world.add_unique(component).unwrap();
        let unique_type_id = self.tracked_type_id_of::<T>();
        self.track_uniques
            .entry(unique_type_id)
            .or_default()
            .push(self.track_current_plugin.clone());
        self
    }

    /// Declare that this builder has a dependency on the following unique.
    ///
    /// If the unique dependency is not satisfied by the time [AppBuilder::finish] is called, then the finish call will panic.
    #[track_caller]
    pub fn depends_on_unique<T>(&mut self, dependency_reason: &'static str) -> &mut Self
    where
        T: Send + Sync + 'static,
    {
        let unique_type_id = self.tracked_type_id_of::<T>();
        self.track_unique_dependencies
            .entry(unique_type_id)
            .or_default()
            .push((self.track_current_plugin.clone(), dependency_reason));
        self
    }

    /// Declare that this builder has a dependency on the following plugin.
    #[track_caller]
    pub fn depends_on_plugin<T>(&mut self, dependency_reason: &'static str) -> &mut Self
    where
        T: Plugin,
    {
        let plugin_type_id = self.tracked_type_id_of::<T>();
        if !self.track_added_plugins.contains_key(&plugin_type_id) {
            panic!(
                "\"{}\" depends on \"{}\": {}",
                self.track_current_plugin,
                type_name::<T>(),
                dependency_reason
            );
        }
        self
    }

    fn add_stage(&mut self, stage_name: &'static str) -> &mut Self {
        self.stage_workloads.add_stage(stage_name);
        self
    }

    // pub fn add_stage_after(&mut self, target: &'static str, stage_name: &'static str) -> &mut Self {
    //     self.stage_workloads.add_stage_after(target, stage_name);
    //     self
    // }

    // pub fn add_stage_before(
    //     &mut self,
    //     target: &'static str,
    //     stage_name: &'static str,
    // ) -> &mut Self {
    //     self.stage_workloads.add_stage_before(target, stage_name);
    //     self
    // }

    #[track_caller]
    pub fn add_system(&mut self, system: WorkloadSystem) -> &mut Self {
        self.stage_workloads
            .add_system_to_stage(DEFAULT_STAGE, system);

        self
    }

    /// Ensure that this system is among the absolute last systems
    #[track_caller]
    pub fn add_reset_system(&mut self, system: WorkloadSystem) -> &mut Self {
        self.resets.push(system);

        self
    }

    pub fn add_plugin<T>(&mut self, plugin: T) -> &mut Self
    where
        T: Plugin,
    {
        let plugin_type_id = self.tracked_type_id_of::<T>();
        if let Some(plugin_id) = self.track_added_plugins.get(&plugin_type_id) {
            panic!(
                "Plugin ({}) cannot add plugin as it's already added as \"{}\"",
                self.track_current_plugin, plugin_id
            );
        }

        if self.track_current_plugin.contains(plugin_type_id) {
            panic!(
                "Plugin ({}) cannot add plugin ({}) as it would cause a cycle",
                self.track_current_plugin,
                self.track_type_names.get(&plugin_type_id).unwrap_or(&""),
            );
        }

        self.track_current_plugin.push::<T>();
        plugin.build(self);
        trace!("added plugin: {}", self.track_current_plugin);
        self.track_added_plugins
            .insert(plugin_type_id, self.track_current_plugin.clone());
        self.track_current_plugin.pop();
        self
    }
}

fn reset_update_pack<T>(mut vm_to_clear: ViewMut<T>) {
    vm_to_clear.clear_all_inserted_and_modified();
    vm_to_clear.take_removed_and_deleted();
}
