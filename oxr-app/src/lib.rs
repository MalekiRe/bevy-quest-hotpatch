//! Bevy 0.19 + OpenXR (Quest) sample, based on bevy_oxr's `android` example
//! (crates/bevy_openxr/examples/android), wired for subsecond hot-patching:
//! the cube is painted via a hot-patchable `desired_color()` every frame —
//! flip BLUE -> RED and save to see it live in the headset.
use bevy::prelude::*;
use bevy::render::view::NoIndirectDrawing;
use bevy_mod_openxr::{add_xr_plugins, init::OxrInitPlugin, types::OxrExtensions};

fn run_app() {
    App::new()
        .add_plugins(add_xr_plugins(DefaultPlugins).set(OxrInitPlugin {
            exts: {
                let mut exts = OxrExtensions::default();
                exts.enable_fb_passthrough();
                exts.enable_hand_tracking();
                exts
            },
            ..default()
        }))
        .add_plugins(bevy_mod_xr::hand_debug_gizmos::HandGizmosPlugin)
        .add_plugins(bevy_mod_openxr::features::fb_passthrough::OxrFbPassthroughPlugin)
        .add_systems(Startup, setup)
        .add_systems(Update, (modify_camera, paint_cube))
        .insert_resource(GlobalAmbientLight {
            color: Default::default(),
            brightness: 500.0,
            affects_lightmapped_meshes: false,
        })
        .insert_resource(ClearColor(Color::NONE))
        .run();
}

// =========================================================================
// Entries (hand-rolled to give subsecond its `main` anchor; same as the
// quest-hotpatch phone demo).
// =========================================================================
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
extern "C" fn android_main(android_app: bevy::android::android_activity::AndroidApp) {
    let _ = bevy::android::ANDROID_APP.set(android_app);
    run_app();
}

/// Sentinel `main` exported from the cdylib so subsecond's ASLR anchor
/// (`dlsym("main")`) resolves.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "C" fn main() {}

#[cfg(not(target_os = "android"))]
fn main() {
    run_app();
}

/// Patch-engine anchor symbol (kept in sync with the host tool).
#[unsafe(no_mangle)]
pub extern "C" fn whisker_aslr_anchor() {}

// =========================================================================
// Scene (from the bevy_oxr android example)
// =========================================================================
#[derive(Component)]
struct Cube;

#[derive(Component)]
struct CamModified;

fn modify_camera(
    cams: Query<Entity, (With<Camera>, Without<CamModified>)>,
    mut commands: Commands,
) {
    for cam in &cams {
        commands
            .entity(cam)
            .insert(Msaa::Off)
            .insert(NoIndirectDrawing)
            .insert(CamModified);
    }
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let mut white: StandardMaterial = Color::WHITE.into();
    white.unlit = true;
    // circular base / floor
    commands.spawn((
        Mesh3d(meshes.add(Circle::new(4.0))),
        MeshMaterial3d(materials.add(white)),
        Transform::from_rotation(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)),
    ));
    // cube (color repainted every frame from the hot function)
    let cube_mat: StandardMaterial = Color::srgb(0.0, 0.0, 1.0).into();
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::new(1.0, 1.0, 1.0))),
        MeshMaterial3d(materials.add(cube_mat)),
        Transform::from_xyz(0.0, 0.5, 0.0),
        Cube,
    ));
}

// =========================================================================
// The hot-patchable surface (same pattern as the phone demo: a plain
// tip-crate fn coerced to fn() to take subsecond's call_as_ptr path).
// =========================================================================
/// HOT: change the returned color and the running app updates it live.
fn desired_color() -> Color {
    Color::srgb(0.0, 0.0, 1.0) // <<< BLUE baseline. Patch me to RED.
}

fn paint_cube(
    mut materials: ResMut<Assets<StandardMaterial>>,
    q: Query<&MeshMaterial3d<StandardMaterial>, With<Cube>>,
) {
    if let Some(h) = q.iter().next().map(|h| h.0.clone()) {
        let f: fn() -> Color = desired_color;
        let color = bevy::app::hotpatch::call(f);
        if let Some(mut m) = materials.get_mut(h.id()) {
            m.base_color = color;
        }
    }
}
