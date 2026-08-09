//! Bevy 0.19 demo app using the recreated `with_hot_patch` API
//! (BevyFlock-style) for subsecond hot-patching on Android via cargo-apk.
//!
//! Two hot-patchable things, both edited without restart:
//!   - `desired_color()` is a #[hot] system-adjacent fn (routed via subsecond)
//!   - `paint_cube` is a system registered inside `with_hot_patch`, so editing
//!     its body re-runs the setup closure and rebuilds the schedule.

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
        .add_systems(Update, (rotate, paint_cube, alive_tick, probe_hotpatch))
        .run();
}

// =============================================================================
// Entry points (hand-rolled; same as before: subsecond needs `main` exported)
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
fn main() {
    run_app();
}

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
        base_color: Color::srgb(0.0, 0.0, 1.0),
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

/// HOT: change this to RED and the running app updates it live (#[hot] routes
/// the call through subsecond's jump table).
#[hot]
fn desired_color() -> Color {
    Color::srgb(0.0, 0.0, 1.0) // <<< BLUE baseline. Patch me to RED.
}

// =============================================================================
// Log-based hot-patch probe (screen-independent): if `#[hot]` works, editing
// hot_flag and saving makes probe_hotpatch log HOTPATCH-WORKED.
// =============================================================================
#[hot]
fn hot_flag() -> u32 {
    0 // <<< baseline. Patch me to 1 and watch the log.
}

fn alive_tick(time: Res<Time>, mut last_t: Local<f32>) {
    if time.elapsed_secs() - *last_t > 2.0 {
        *last_t = time.elapsed_secs();
        info!("ALIVE tick at {:.1}s", time.elapsed_secs());
    }
}

fn probe_hotpatch(mut last: Local<u32>) {
    let v = hot_flag();
    if *last != v {
        info!("HOTPATCH-WORKED: hot_flag changed {} -> {} via #[hot]", *last, v);
        *last = v;
    }
}

/// Registered via with_hot_patch: editing this BODY re-runs the closure and
/// rebuilds the schedule with the new system (hot without #[hot]).
fn paint_cube(
    mut materials: ResMut<Assets<StandardMaterial>>,
    q: Query<&MeshMaterial3d<StandardMaterial>, With<Cube>>,
) {
    if let Some(h) = q.iter().next().map(|h| h.0.clone()) {
        if let Some(mut m) = materials.get_mut(h.id()) {
            m.base_color = desired_color();
        }
    }
}
