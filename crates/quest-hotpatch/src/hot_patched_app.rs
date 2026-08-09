//! API for hot-patching new systems into your running app — **fully generic over
//! every schedule**. [`HotPatchedAppExt::with_hot_patch`] takes a closure that can
//! add systems to *any* schedules (built-in or custom, no hardcoded list). On each
//! hot-reload the closure re-runs on a fresh app; every schedule it populated gets
//! (1) a proxy inside the real schedule and (2) a mirror schedule that owns its
//! systems, so bevy's own systems in the real schedules are never disturbed.
//!
//! Mirrors are pre-created on demand from a pool of 1,000 distinct labels
//! (`HotPatchIdx(0)..HotPatchIdx(999)`), assigned to schedules as they appear.

use crate::HotPatched;
use bevy_app::{App, PreUpdate};
use bevy_derive::{Deref, DerefMut};
use bevy_ecs::prelude::*;
use bevy_ecs::schedule::{InternedScheduleLabel, Schedule, ScheduleGraph, Schedules};
use bevy_ecs::system::{NonSend, NonSendMarker};
use bevy_ecs_macros::ScheduleLabel;
use bevy_log::{debug, error};
use dioxus_devtools::subsecond::HotFn;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// Runtime pool of mirror schedule labels. Every distinct value is a distinct,
/// internally-interned schedule label, so values `0..999` give us 1,000
/// pre-creatable mirrors for (up to) 1,000 target schedules.
#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash, Default)]
struct HotPatchIdx(u32);

/// Per-`with_hot_patch` bookkeeping: which target schedule owns which mirror
/// index, and which schedules already have a proxy wired.
#[derive(Default)]
struct HotPatchState {
    next: u32,
    map: HashMap<InternedScheduleLabel, u32>,
    proxied: HashSet<InternedScheduleLabel>,
}

/// Wrapper around [`App`] used by [`HotPatchedAppExt::with_hot_patch`], kept
/// thread-safe so it can be dispatched through subsecond.
#[derive(Deref, DerefMut)]
struct HotPatchedApp(send_wrapper::SendWrapper<App>);

impl Default for HotPatchedApp {
    fn default() -> Self {
        HotPatchedApp(send_wrapper::SendWrapper::new(App::default()))
    }
}

/// Trait for [`App`] to add and remove systems at runtime.
pub trait HotPatchedAppExt {
    /// Register systems that can be swapped in/out without restarting the app.
    ///
    /// The closure may add systems to **any** schedule — built-in or custom:
    ///
    /// ```ignore
    /// App::new()
    ///     .add_plugins(DefaultPlugins)
    ///     .add_plugins(SimpleSubsecondPlugin::default())
    ///     .with_hot_patch(|app: &mut App| {
    ///         app.add_systems(Update, my_system);
    ///         app.add_systems(FixedUpdate, fixed_system);
    ///         app.add_systems(Last, final_system);
    ///         app.add_systems(MyCustomSchedule, custom_system);
    ///     })
    ///     .run();
    /// ```
    ///
    /// Each schedule used by the closure gets a mirror schedule (from a pool of
    /// 1,000) plus a proxy in the real schedule, and is hot-swapped on reload.
    fn with_hot_patch(&mut self, func: impl FnMut(&mut App) + Send + Sync + 'static) -> &mut App;
}

impl HotPatchedAppExt for App {
    fn with_hot_patch(
        &mut self,
        mut func: impl FnMut(&mut App) + Send + Sync + 'static,
    ) -> &mut App {
        // The closure is dispatched through the subsecond jump table, so a re-run
        // executes the *newest* build of the closure (and the freshly compiled
        // system bodies it registers).
        let reload = move |mut hot_patched_app: HotPatchedApp| -> HotPatchedApp {
            func(&mut hot_patched_app.0);
            hot_patched_app
        };
        let reloadable = Mutex::new(HotFn::current(reload));
        let state = Mutex::new(HotPatchState::default());

        self.add_systems(
            PreUpdate,
            move |_: Option<NonSend<NonSendMarker>>,
                  mut ran_once: Local<bool>,
                  mut schedules: ResMut<Schedules>,
                  hotreload_event: MessageReader<HotPatched>| {
                // Run once at startup to install the initial schedules, and again
                // on every hot-reload (when a `HotPatched` event exists).
                if hotreload_event.is_empty() {
                    if *ran_once {
                        return;
                    }
                    *ran_once = true;
                }

                let mut reload_app = match reloadable
                    .lock()
                    .unwrap()
                    .try_call((HotPatchedApp::default(),))
                {
                    Ok(app) => app,
                    Err(e) => {
                        error!("with_hot_patch: hotpatch function failed: {e:?}");
                        return;
                    }
                };

                // Harvest every schedule the closure populated (the fresh app starts
                // empty, so anything with systems here came from the closure).
                let mut populated: Vec<(InternedScheduleLabel, ScheduleGraph)> = Vec::new();
                {
                    let mut rs = reload_app
                        .world_mut()
                        .get_resource_mut::<Schedules>()
                        .unwrap();
                    for (_, schedule) in rs.iter_mut() {
                        if schedule.systems_len() > 0 {
                            populated.push((schedule.label(), std::mem::take(schedule.graph_mut())));
                        }
                    }
                }

                let mut state = state.lock().unwrap();
                for (label_id, graph) in populated {
                    if state.next >= 1000 {
                        error!(
                            "with_hot_patch: exceeded 1000 hot schedules; skipping {:?}",
                            label_id
                        );
                        continue;
                    }
                    // Assign a mirror index the first time we see this schedule.
                    let idx = match state.map.get(&label_id) {
                        Some(&i) => i,
                        None => {
                            let i = state.next;
                            state.next += 1;
                            state.map.insert(label_id, i);
                            i
                        }
                    };
                    // First sight: add a proxy inside the REAL schedule that ticks
                    // the mirror, so the user's systems run exactly where `add_systems`
                    // put them — with bevy's own systems untouched.
                    if state.proxied.insert(label_id) {
                        schedules.add_systems(label_id, move |world: &mut World| {
                            let _ = world.try_run_schedule(HotPatchIdx(idx));
                        });
                        debug!("with_hot_patch: wired proxy for schedule #{idx}");
                    }
                    // (Re)install the mirror with the fresh graph. `Schedule::run`
                    // re-initializes lazily when the graph changes, so no manual
                    // re-init is needed.
                    let mut mirror = Schedule::new(HotPatchIdx(idx));
                    *mirror.graph_mut() = graph;
                    schedules.insert(mirror);
                }
            },
        );

        self
    }
}
