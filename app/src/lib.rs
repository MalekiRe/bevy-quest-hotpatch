//! Bevy 0.19 demo using the recreated `with_hot_patch` API (BevyFlock-style)
//! for subsecond hot-patching on Android via cargo-apk.
//!
//! Hot-patching mechanism (deliberately NO per-frame #[hot] jump-table dispatch,
//! which crashes bevy's render prep on MTE phones): systems are registered inside
//! `with_hot_patch`. On hot-reload it re-runs the closure => processes the patch
//! ONE time through subsecond's jump table, re-registers the (new) system with a
//! fresh function pointer, and rebuilds the schedule. After that the system runs
//! DIRECTLY (no per-frame table consult). Edit `paint_cube`'s body to recolor live.

use bevy::prelude::*;
use bevy::window::WindowPlugin;
use quest_hotpatch::prelude::*;

fn run_app() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window { resizable: false, ..default() }),
            ..default()
        }))
        .add_plugins(SimpleSubsecondPlugin::default())
        .add_systems(Startup, setup)
        .with_hot_patch(|app| {
            // THREE schedules at once: Update, FixedUpdate, Last — all hot.
            app.add_systems(Update, (rotate, alive_tick, paint_cube));
            app.add_systems(FixedUpdate, fixed_probe);
            app.add_systems(Last, last_probe);
        })
        .run();
}

// =============================================================================
// Entry points
// =============================================================================
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
extern "C" fn android_main(android_app: bevy::android::android_activity::AndroidApp) {
    let _ = bevy::android::ANDROID_APP.set(android_app);
    run_app();
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "C" fn main() {}

#[cfg(not(target_os = "android"))]
fn main() { run_app(); }

#[unsafe(no_mangle)]
pub extern "C" fn whisker_aslr_anchor() {}

// =============================================================================
// Scene
// =============================================================================
#[derive(Component)]
struct Cube;

#[derive(Component)]
struct Rotator;

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.spawn(Camera3d::default());
    commands.spawn((
        DirectionalLight::default(),
        Transform::from_xyz(4.0, 8.0, 4.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    let mat = materials.add(StandardMaterial {
        unlit: true,
        base_color: Color::srgb(0.0, 0.0, 1.0), // BLUE baseline
        ..default()
    });
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::new(1.0, 1.0, 1.0))),
        MeshMaterial3d(mat),
        Transform::from_xyz(0.0, 0.0, -4.0),
        Cube,
        Rotator,
    ));
}

fn rotate(time: Res<Time>, mut q: Query<&mut Transform, With<Rotator>>) {
    for mut t in &mut q {
        t.rotate_y(0.06 * time.delta_secs());
    }
}

fn alive_tick(time: Res<Time>, mut last_t: Local<f32>) {
    if time.elapsed_secs() - *last_t > 2.0 {
        *last_t = time.elapsed_secs();
        info!("ALIVE tick at {:.1}s", time.elapsed_secs());
    }
}

/// Registered via with_hot_patch: edit this BODY (e.g. the color) and save while
/// the app runs -> the closure re-runs once -> schedule rebuilt -> cube recolors.
fn paint_cube(
    mut materials: ResMut<Assets<StandardMaterial>>,
    q: Query<&MeshMaterial3d<StandardMaterial>, With<Cube>>,
) {
    let new_color = Color::srgb(0.0, 1.0, 0.0); // <<< LIVE GREEN
    if let Some(h) = q.iter().next() {
        if let Some(mut m) = materials.get_mut(h.id()) {
            if m.base_color != new_color {
                m.base_color = new_color;
                info!("PAINTED cube -> {:?}", new_color);
            }
        }
    }
}

/// In FixedUpdate (mirror): bump this marker to prove FixedUpdate is hot too.
fn fixed_probe(mut last: Local<u32>) {
    let marker: u32 = 3; // <<< LIVE2 fixed marker
    if *last != marker {
        *last = marker;
        info!("FIXED-PROBE marker={marker}");
    }
}

/// In Last (mirror): bump this marker to prove Last is hot too.
fn last_probe(mut last: Local<u32>) {
    let marker: u32 = 3; // <<< LIVE2 last marker
    if *last != marker {
        *last = marker;
        info!("LAST-PROBE marker={marker}");
    }
}
