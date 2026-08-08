//! Bevy + Subsecond live hot-patch demo (cargo-apk / Android, Quest-ready).
//!
//! A cube is painted a solid color EVERY FRAME through `bevy::app::hotpatch::call`
//! (which routes through subsecond's jump table). Editing BLUE->RED and saving
//! hot-patches the running app so the cube turns red live — the demonstration.

use bevy::prelude::*;
use bevy::window::WindowPlugin;

fn run_app() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window { resizable: false, ..default() }),
            ..default()
        }))
        .add_systems(Startup, setup)
        .add_systems(Update, (rotate, paint_cube).chain())
        .run();
}

// =============================================================================
// Entry points (hand-rolled; matches what `#[bevy_main]` generates, plus the
// subsecond `main` anchor for dlsym and whisker's `whisker_aslr_anchor`.
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


// ---------------------------------------------------------------------------
// ASLR anchor for the subsecond patch engine (REAL symbol, present in app .so
// and every patch .so). Do not remove.
// ---------------------------------------------------------------------------
#[unsafe(no_mangle)]
pub extern "C" fn whisker_aslr_anchor() {}

#[cfg(not(target_os = "android"))]
fn main() {
    run_app();
}

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
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::new(1.0, 1.0, 1.0))),
        MeshMaterial3d(materials.add(StandardMaterial {
            unlit: true,
            base_color: Color::srgb(0.0, 0.0, 1.0), // BLUE baseline
            ..default()
        })),
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

/// Hot-patchable color source: a PLAIN tip-crate fn, routed through the jump
/// table via subsecond::call (fn-pointer / call_as_ptr path). Change BLUE->RED
/// and save to live-patch the color without a rebuild.
fn desired_color() -> Color {
    Color::srgb(0.0, 0.0, 1.0) // <<< BLUE baseline. Patch me to RED.
}

/// Applies the hot-patchable color every frame. (Bevy 0.19's FunctionSystem
/// routing doesn't re-read the jump table, so we route the hot function
/// through subsecond::call ourselves with a coerce-to-fn-pointer.)
fn paint_cube(
    mut materials: ResMut<Assets<StandardMaterial>>,
    q: Query<&MeshMaterial3d<StandardMaterial>, With<Cube>>,
) {
    if let Some(h) = q.iter().next().map(|h| h.0.clone()) {
        let f: fn() -> Color = desired_color; // size-8 fn pointer -> call_as_ptr path
        let color = bevy::app::hotpatch::call(f);
        if let Some(mut m) = materials.get_mut(h.id()) {
            m.base_color = Color::srgb(0.0, 1.0, 0.0); // SYSTEM-BODY EDIT -> GREEN (desired_color untouched)
        }
    }
}

