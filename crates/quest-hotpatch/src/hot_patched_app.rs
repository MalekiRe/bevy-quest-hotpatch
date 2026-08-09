//! API for hot-patching new systems into your running app.
//! See [`HotPatchedAppExt::with_hot_patch`] for the main API.

use crate::__macros_internal::__ReloadPositions as ReloadPositions;
use bevy_app::{
    App, First, FixedLast, FixedMain, FixedPostUpdate, FixedPreUpdate, FixedUpdate, Last,
    PostStartup, PostUpdate, PreStartup, PreUpdate, Startup, Update,
};
use bevy_derive::{Deref, DerefMut};
#[cfg(debug_assertions)]
use bevy_ecs::system::{Commands, Res};
use bevy_ecs::{prelude::*, system::NonSendMarker};
use bevy_ecs_macros::ScheduleLabel;
use bevy_log::{debug, error};

use crate::HotPatched;

/// Wrapper around [`App`] used by [`HotPatchedAppExt::with_hot_patch`], which allows you to add and remove systems at runtime.
#[derive(Deref, DerefMut)]
struct HotPatchedApp(send_wrapper::SendWrapper<App>);

impl Default for HotPatchedApp {
    fn default() -> Self {
        HotPatchedApp(send_wrapper::SendWrapper::new(App::default()))
    }
}

/// The [`Startup`] schedule, but rerun on hot-reload.
/// Only valid inside the context of [`HotPatchedAppExt::with_hot_patch`].
#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct StartupRerunHotPatch;

/// One mirror schedule per hot-patchable schedule: the *real* schedule keeps
/// bevy's systems (and a proxy that runs the mirror), so whole-schedule
/// replacement can never drop bevy's internals.
macro_rules! hot_patch_mirrors {
    ($($label:ident => $mirror:ident),* $(,)?) => {
        $(
            #[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash, Default)]
            struct $mirror;
        )*
    }
}
hot_patch_mirrors!(
    First        => HotPatchFirst,
    PreUpdate    => HotPatchPreUpdate,
    Update       => HotPatchUpdate,
    PostUpdate   => HotPatchPostUpdate,
    Last         => HotPatchLast,
    FixedMain    => HotPatchFixedMain,
    FixedPreUpdate => HotPatchFixedPreUpdate,
    FixedUpdate  => HotPatchFixedUpdate,
    FixedPostUpdate => HotPatchFixedPostUpdate,
    FixedLast    => HotPatchFixedLast,
);

/// Trait for [`App`] to add and remove systems at runtime.
pub trait HotPatchedAppExt {
    /// Call this with plugins and systems and it will auto-add and remove systems in the `Update` schedule to your running app.
    ///
    /// # Example
    ///
    /// ```ignore
    /// # use bevy::prelude::*;
    /// # use quest_hotpatch::prelude::*;
    ///
    /// App::new()
    ///     .add_plugins(DefaultPlugins)
    ///     .add_plugins(SimpleSubsecondPlugin::default())
    ///     .with_hot_patch(|app: &mut App| {
    ///         // Systems in the `StartupRerunHotPatch` schedule will be rerun on hot-reload.
    ///         // They require `#[hot(hot_patch_signature = true)]`
    ///         app.add_systems(StartupRerunHotPatch, setup);
    ///         // All other systems do not require `#[hot]`.
    ///         app.add_systems(Update, my_system);
    ///         app.add_systems(PostUpdate, second_system);
    ///     });
    ///
    /// #[hot(hot_patch_signature = true)]
    /// fn setup(mut commands: Commands) {
    ///     commands.spawn(Camera2d::default());
    ///     commands.spawn(Text::new("Hello, world!"));
    /// }
    ///
    /// fn my_system() {
    ///     info!("Hello, world!");
    /// }
    ///
    /// fn second_system() {
    ///     info!("Goodbye, world!");
    /// }
    /// ```
    fn with_hot_patch(&mut self, func: impl FnMut(&mut App) + Send + Sync + 'static) -> &mut App;
}

/// Swap the fresh graph of schedule `L` into its mirror `M` (creating/replacing
/// the mirror schedule). The running schedule `L` is untouched apart from its
/// proxy, so bevy's own systems survive.
fn swap_into_mirror<L, M>(
    reload_schedules: &mut Schedules,
    schedules: &mut Schedules,
    commands: &mut Commands,
    log_name: &'static str,
) where
    L: bevy_ecs::schedule::ScheduleLabel + Default,
    M: bevy_ecs::schedule::ScheduleLabel + Default,
{
    let Some(mut fresh) = reload_schedules.remove(L::default()) else {
        return;
    };
    if fresh.systems_len() == 0 {
        return;
    }
    schedules.remove(M::default());
    let hot = schedules.entry(M::default());
    *hot.graph_mut() = std::mem::take(fresh.graph_mut());
    // NB: the cached system must be a ZST (bevy const-asserts size_of::<S>() == 0),
    // so it cannot capture `log_name`; keep it capture-free and log outside.
    commands.run_system_cached(|world: &mut World| {
        world.schedule_scope(M::default(), |world, schedule| {
            if let Err(e) = schedule.initialize(world) {
                error!("with_hot_patch: failed to initialize a hot mirror");
            }
        });
    });
    debug!("with_hot_patch: swapped {log_name} mirror");
}

impl HotPatchedAppExt for App {
    fn with_hot_patch(
        &mut self,
        mut func: impl FnMut(&mut App) + Send + Sync + 'static,
    ) -> &mut App {
        let mut app = App::new();
        app.init_schedule(Startup);
        app.init_schedule(PostStartup);
        app.init_schedule(PreStartup);
        // Ensure the real App has them too — on some plugin orders these aren't
        // registered yet, and the `.unwrap()` below would panic at startup.
        self.init_schedule(Startup);
        self.init_schedule(PostStartup);
        self.init_schedule(PreStartup);
        std::mem::swap(
            app.get_schedule_mut(Startup).unwrap(),
            self.get_schedule_mut(Startup).unwrap(),
        );
        std::mem::swap(
            app.get_schedule_mut(PreStartup).unwrap(),
            self.get_schedule_mut(PreStartup).unwrap(),
        );
        std::mem::swap(
            app.get_schedule_mut(PostStartup).unwrap(),
            self.get_schedule_mut(PostStartup).unwrap(),
        );

        func(&mut app);

        std::mem::swap(
            app.get_schedule_mut(Startup).unwrap(),
            self.get_schedule_mut(Startup).unwrap(),
        );
        std::mem::swap(
            app.get_schedule_mut(PreStartup).unwrap(),
            self.get_schedule_mut(PreStartup).unwrap(),
        );
        std::mem::swap(
            app.get_schedule_mut(PostStartup).unwrap(),
            self.get_schedule_mut(PostStartup).unwrap(),
        );

        // A proxy per hot-patchable schedule: runs the mirror right where the
        // real schedule runs, so hot systems get the same ordering/context.
        macro_rules! add_proxy {
            ($label:ty => $mirror:ty) => {
                self.add_systems(<$label>::default(), move |world: &mut World| {
                    let _ = world.try_run_schedule(<$mirror>::default());
                });
            };
        }
        add_proxy!(First => HotPatchFirst);
        add_proxy!(PreUpdate => HotPatchPreUpdate);
        add_proxy!(Update => HotPatchUpdate);
        add_proxy!(PostUpdate => HotPatchPostUpdate);
        add_proxy!(Last => HotPatchLast);
        add_proxy!(FixedMain => HotPatchFixedMain);
        add_proxy!(FixedPreUpdate => HotPatchFixedPreUpdate);
        add_proxy!(FixedUpdate => HotPatchFixedUpdate);
        add_proxy!(FixedPostUpdate => HotPatchFixedPostUpdate);
        add_proxy!(FixedLast => HotPatchFixedLast);

        self.add_systems(Startup, |world: &mut World| {
            world.insert_resource(ReloadPositions::default());
        });

        let hot_patched_func = move |mut hot_patched_app: HotPatchedApp| -> HotPatchedApp {
            func(&mut hot_patched_app.0);
            hot_patched_app
        };
        let reloadable_section =
            std::sync::Mutex::new(dioxus_devtools::subsecond::HotFn::current(hot_patched_func));
        self.add_systems(
            PreUpdate,
            move |_: Option<NonSend<NonSendMarker>>,
                  mut ran_once: Local<bool>,
                  mut schedules: ResMut<Schedules>,
                  mut commands: Commands,
                  hotreload_event: MessageReader<HotPatched>| {
                if hotreload_event.is_empty() {
                    if *ran_once {
                        return;
                    }
                    *ran_once = true;
                }

                let reload_app = reloadable_section
                    .lock()
                    .unwrap()
                    .try_call((HotPatchedApp::default(),));

                let mut reload_app = match reload_app {
                    Ok(reload_app) => reload_app,
                    Err(e) => {
                        error!("Failed to call hotpatch function: {e:?}");
                        return;
                    }
                };

                let mut reload_schedules = reload_app
                    .world_mut()
                    .get_resource_mut::<Schedules>()
                    .unwrap();

                swap_into_mirror::<First, HotPatchFirst>(&mut reload_schedules, &mut schedules, &mut commands, "First");
                swap_into_mirror::<PreUpdate, HotPatchPreUpdate>(&mut reload_schedules, &mut schedules, &mut commands, "PreUpdate");
                swap_into_mirror::<Update, HotPatchUpdate>(&mut reload_schedules, &mut schedules, &mut commands, "Update");
                swap_into_mirror::<PostUpdate, HotPatchPostUpdate>(&mut reload_schedules, &mut schedules, &mut commands, "PostUpdate");
                swap_into_mirror::<Last, HotPatchLast>(&mut reload_schedules, &mut schedules, &mut commands, "Last");
                swap_into_mirror::<FixedMain, HotPatchFixedMain>(&mut reload_schedules, &mut schedules, &mut commands, "FixedMain");
                swap_into_mirror::<FixedPreUpdate, HotPatchFixedPreUpdate>(&mut reload_schedules, &mut schedules, &mut commands, "FixedPreUpdate");
                swap_into_mirror::<FixedUpdate, HotPatchFixedUpdate>(&mut reload_schedules, &mut schedules, &mut commands, "FixedUpdate");
                swap_into_mirror::<FixedPostUpdate, HotPatchFixedPostUpdate>(&mut reload_schedules, &mut schedules, &mut commands, "FixedPostUpdate");
                swap_into_mirror::<FixedLast, HotPatchFixedLast>(&mut reload_schedules, &mut schedules, &mut commands, "FixedLast");

                if let Some(mut auto_reload_startup) = reload_schedules.remove(StartupRerunHotPatch)
                {
                    schedules.remove(StartupRerunHotPatch);
                    let schedule: &mut Schedule = schedules.entry(StartupRerunHotPatch);
                    *schedule.graph_mut() = std::mem::take(auto_reload_startup.graph_mut());
                    commands.run_system_cached(|world: &mut World| {
                        world.schedule_scope(StartupRerunHotPatch, |world, auto_reload_startup| {
                            let result = auto_reload_startup.initialize(world);
                            if let Err(e) = result {
                                error!("Failed to initialize hotpatch auto_reload_startup: {e}");
                            }
                        });
                    });

                    commands.run_system_cached(
                        |mut commands: Commands,
                         query: Query<Entity>,
                         reload_positions: Res<ReloadPositions>,
                         world: &World| {
                            for e in query.iter() {
                                let Some(location) = world
                                    .entities()
                                    .entity_get_spawned_or_despawned_by(e)
                                    .into_option()
                                else {
                                    continue;
                                };
                                let Some(location) = location else { continue };
                                for (file, line_start, line_end) in reload_positions.iter() {
                                    if location.file() != *file {
                                        continue;
                                    }
                                    if location.line() > *line_start && location.line() < *line_end
                                    {
                                        debug!("despawning an entity at: {location:?}");
                                        commands.entity(e.entity()).despawn();
                                    }
                                }
                            }
                        },
                    );
                    commands.run_system_cached(|world: &mut World| {
                        // we clear our reload positions every time so we can fill them up with new stuff.
                        world.insert_resource(ReloadPositions::default());
                        if let Err(e) = world.try_run_schedule(StartupRerunHotPatch) {
                            error!("Failed to auto-reload startup: {e:?}");
                        }
                    })
                }
            },
        );
        self
    }
}
