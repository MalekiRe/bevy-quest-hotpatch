#![warn(missing_docs)]
#![allow(clippy::type_complexity)]
#![doc = include_str!("../README.md")]

#[cfg(debug_assertions)]
use __macros_internal::__HotPatchedSystems as HotPatchedSystems;
use bevy_app::{App, Last, Plugin, PreUpdate};
use bevy_ecs::prelude::*;
pub use dioxus_devtools;
#[cfg(debug_assertions)]
use dioxus_devtools::{subsecond::apply_patch, *};
pub mod hot_patched_app;

/// Everything you need to use hotpatching
pub mod prelude {
    pub use super::{
        HotPatched, SimpleSubsecondPlugin,
        hot_patched_app::HotPatchedAppExt as _,
    };
}

/// The plugin you need to add to your app:
///
/// ```ignore
/// use bevy::prelude::*;
/// use quest_hotpatch::prelude::*;
///
/// App::new()
///     .add_plugins(DefaultPlugins)
///     .add_plugins(SimpleSubsecondPlugin)
///     .with_hot_patch(|app: &mut App| {
///         app.add_systems(Update, my_system);
///     })
///     .run();
/// ```
///
/// Connects to the quest-hotpatch devserver, applies subsecond jump tables, and
/// (together with [`HotPatchedAppExt::with_hot_patch`]) rebuilds the schedules
/// of hot-patched systems at runtime.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct SimpleSubsecondPlugin;

impl Plugin for SimpleSubsecondPlugin {
    fn build(&self, app: &mut App) {
        app.configure_sets(
            PreUpdate,
            (SimpleSubsecondSystemSet::UpdateFunctionPtrs,).chain(),
        );
        #[cfg(not(debug_assertions))]
        {
            return;
        }
        #[cfg(debug_assertions)]
        {
            let (sender, receiver) = crossbeam_channel::bounded::<HotPatched>(1);

            // The device dials ws://127.0.0.1:8080/_dioxus (adb reverse on Android);
            // apply any jump table the host pushes and wake the schedule rebuilder.
            connect(move |msg| {
                if let DevserverMsg::HotReload(hot_reload_msg) = msg {
                    if let Some(jumptable) = hot_reload_msg.jump_table {
                        // SAFETY: any code using the updated jump table becomes unsafe;
                        // the table itself must be built carefully by the host.
                        unsafe { apply_patch(jumptable).unwrap() };
                        sender.send(HotPatched).unwrap();
                    }
                }
            });

            app.init_resource::<HotPatchedSystems>();

            app.add_message::<HotPatched>().add_systems(
                Last,
                move |mut events: MessageWriter<HotPatched>| {
                    if receiver.try_recv().is_ok() {
                        events.write_default();
                    }
                },
            );
        }
    }
}

/// Message sent when the hotpatch is applied.
#[derive(Message, Default)]
pub struct HotPatched;

/// Systems in this set refresh their function pointers after a hot patch.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum SimpleSubsecondSystemSet {
    /// Update the pointers to the current function definitions.
    UpdateFunctionPtrs,
}

#[doc(hidden)]
pub mod __macros_internal {
    pub use bevy_app::PreUpdate;
    use bevy_derive::{Deref, DerefMut};
    pub use bevy_ecs::{
        schedule::Schedules,
        system::{IntoSystem, SystemId, SystemState},
        world::World,
    };
    pub use bevy_ecs_macros::Resource;
    pub use bevy_log::debug;
    use bevy_platform::collections::{HashMap, HashSet};
    use dioxus_devtools::subsecond::HotFnPtr;
    use std::any::TypeId;

    #[derive(Resource, Default)]
    pub struct __HotPatchedSystems(pub HashMap<TypeId, __HotPatchedSystem>);

    #[doc(hidden)]
    pub struct __HotPatchedSystem {
        pub current_ptr: HotFnPtr,
        pub last_ptr: HotFnPtr,
    }

    #[doc(hidden)]
    #[derive(Deref, DerefMut, Resource, Default, Debug)]
    pub struct __ReloadPositions(pub HashSet<(&'static str, u32, u32)>);
}
