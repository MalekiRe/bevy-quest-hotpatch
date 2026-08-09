//! Bevy 0.19 **native** hotpatching test on Android (cargo-apk).
//! Uses bevy's own `hotpatching` feature -> `HotPatchPlugin` is auto-wired into
//! DefaultPlugins, connects to the quest-hotpatch devserver (same dioxus/_dioxus
//! protocol), applies our jump table, and `FunctionSystem::refresh_hotpatch` re-points
//! systems. Edit `probe`'s marker (BEFORE -> AFTER) and it changes in flight.

use bevy::prelude::*;
use bevy::window::WindowPlugin;

fn run_app() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window { resizable: false, ..default() }),
            ..default()
        }))
        .add_systems(Startup, setup)
        .add_systems(Update, (rotate, alive_tick, probe))
        .run();
}

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
    let mat = materials.add(StandardMaterial { unlit: true, base_color: Color::srgb(0.0, 0.0, 1.0), ..default() });
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::new(1.0, 1.0, 1.0))),
        MeshMaterial3d(mat),
        Transform::from_xyz(0.0, 0.0, -4.0),
        Cube, Rotator,
    ));
}

fn rotate(time: Res<Time>, mut q: Query<&mut Transform, With<Rotator>>) {
    for mut t in &mut q { t.rotate_y(0.06 * time.delta_secs()); }
}

fn alive_tick(time: Res<Time>, mut last_t: Local<f32>) {
    if time.elapsed_secs() - *last_t > 2.0 {
        *last_t = time.elapsed_secs();
        info!("ALIVE tick at {:.1}s", time.elapsed_secs());
    }
}

/// Plain bevy system, NATIVE hotpatching: edit the marker string in this body
/// and save; bevy's refresh_hotpatch re-points the system via our jump table.
fn probe(time: Res<Time>, mut last: Local<f32>) {
    if time.elapsed_secs() - *last > 3.0 {
        *last = time.elapsed_secs();
        info!("HOTPATCH-NATIVE marker=AFTER"); // <<< now AFTER
    }
}
