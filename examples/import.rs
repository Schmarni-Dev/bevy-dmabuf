use std::sync::{Mutex, mpsc};

use bevy::{
    DefaultPlugins,
    app::{App, AppExit, PostUpdate, PreUpdate, Startup},
    asset::{Assets, Handle},
    color::Color,
    core_pipeline::core_3d::Camera3d,
    ecs::{
        resource::Resource,
        system::{Commands, Res, ResMut},
    },
    image::Image,
    log::{error, info},
    math::{
        Quat, Vec3,
        primitives::{Circle, Cuboid},
    },
    pbr::{MeshMaterial3d, PointLight, StandardMaterial},
    render::{
        mesh::{Mesh, Mesh3d},
        pipelined_rendering::PipelinedRenderingPlugin,
    },
    transform::components::Transform,
    utils::default,
};
use bevy_dmabuf::{
    dmatex::Dmatex,
    import::{DmabufImportPlugin, DmatexUsage, ImportedDmatexs},
    wgpu_init::add_dmabuf_init_plugin,
};

#[tokio::main]
async fn main() -> AppExit {
    let (tx, rx) = mpsc::channel();
    let _conn = zbus::connection::Builder::session()
        .unwrap()
        .name("dev.schmarni.bevy_dmabuf.dmatex")
        .unwrap()
        .serve_at(
            "/dev/schmarni/bevy_dmabuf/dmatex",
            TestInterface { dmatex_channel: tx },
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    App::new()
        .insert_resource(Receiver(rx.into()))
        .init_resource::<PendingDmatex>()
        .add_plugins(add_dmabuf_init_plugin(DefaultPlugins, []).disable::<PipelinedRenderingPlugin>())
        .add_plugins(DmabufImportPlugin)
        .add_systems(Startup, setup)
        .add_systems(PreUpdate, update_tex)
        .add_systems(PostUpdate, import_tex)
        .run()
}

fn update_tex(
    handle: Res<CubeMat>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut pending: ResMut<PendingDmatex>,
) {
    if let Some(image) = pending.0.take() {
        let mat = materials.get_mut(&handle.0).unwrap();
        mat.base_color_texture = Some(image);
    }
}

fn import_tex(
    dmatexs: Res<ImportedDmatexs>,
    mut receiv: ResMut<Receiver>,
    mut pending: ResMut<PendingDmatex>,
    mut images: ResMut<Assets<Image>>,
) {
    if let Some(buf) = receiv.0.get_mut().unwrap().try_iter().last() {
        info!("got dmatex");
        match dmatexs.set(&mut images, buf, DmatexUsage::Sampling, None) {
            Ok(image) => {
                pending.0 = Some(image);
            }
            Err(err) => {
                error!("error while importing dmatex: {err}");
            }
        }
    }
}

#[derive(Resource, Default)]
struct PendingDmatex(Option<Handle<Image>>);

// set up a simple 3D scene
fn setup(
    mut cmds: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let mat_handle = materials.add(StandardMaterial {
        base_color: Color::WHITE,
        ..default()
    });
    // cube
    cmds.spawn((
        Mesh3d(meshes.add(Cuboid::new(1.0, 1.0, 1.0))),
        MeshMaterial3d(mat_handle.clone()),
        Transform::from_xyz(0.0, 0.5, 0.0),
    ));
    cmds.insert_resource(CubeMat(mat_handle));
    // circular base
    cmds.spawn((
        Mesh3d(meshes.add(Circle::new(4.0))),
        MeshMaterial3d(materials.add(Color::WHITE)),
        Transform::from_rotation(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)),
    ));
    // light
    cmds.spawn((
        PointLight {
            shadows_enabled: true,
            ..default()
        },
        Transform::from_xyz(4.0, 8.0, 4.0),
    ));
    cmds.spawn((
        Camera3d::default(),
        Transform::from_xyz(-2.5, 4.5, 9.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

#[derive(Resource)]
struct Receiver(Mutex<mpsc::Receiver<Dmatex>>);
#[derive(Resource)]
struct CubeMat(Handle<StandardMaterial>);

pub struct TestInterface {
    pub dmatex_channel: mpsc::Sender<Dmatex>,
}

#[zbus::interface(name = "dev.schmarni.bevy_dmabuf.dmatex")]
impl TestInterface {
    fn dmatex(&self, dmabuf: Dmatex) {
        _ = self.dmatex_channel.send(dmabuf);
    }
}
