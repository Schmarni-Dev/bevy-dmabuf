use std::sync::{Mutex, mpsc};

use bevy::{
    DefaultPlugins,
    app::{App, AppExit, PostUpdate, Startup, Update},
    asset::{Assets, Handle},
    color::Color,
    core_pipeline::core_3d::Camera3d,
    ecs::{
        resource::Resource,
        system::{Commands, Local, Res, ResMut},
    },
    image::Image,
    log::{error, info},
    math::{
        Quat, Vec3,
        primitives::{Circle, Cuboid},
    },
    pbr::{MeshMaterial3d, PointLight, StandardMaterial},
    prelude::{Deref, DerefMut},
    render::{
        mesh::{Mesh, Mesh3d},
        pipelined_rendering::PipelinedRenderingPlugin,
    },
    transform::components::Transform,
    utils::default,
};
use bevy_dmabuf::{
    dmatex::Dmatex,
    import::{DmabufImportPlugin, ImportedDmatexs},
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
        .init_resource::<Imported>()
        .add_plugins(add_dmabuf_init_plugin(DefaultPlugins, []).disable::<PipelinedRenderingPlugin>())
        .add_plugins(DmabufImportPlugin)
        .add_systems(Startup, setup)
        .add_systems(PostUpdate, (update_tex, update))
        .run()
}

#[derive(Resource, DerefMut, Deref, Default)]
struct Imported(Vec<Handle<Image>>);
fn update(
    cube_mat: Res<CubeMat>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    imported: Res<Imported>,
    mut index: Local<usize>,
) {
    if imported.is_empty() {
        return;
    }
    let mat = materials.get_mut(&cube_mat.0).unwrap();
    mat.base_color_texture = Some(imported[*index].clone());
    *index = (*index + 1) % imported.len();
}
fn update_tex(
    dmatexs: Res<ImportedDmatexs>,
    mut receiv: ResMut<Receiver>,
    mut images: ResMut<Assets<Image>>,
    mut imported: ResMut<Imported>,
) {
    if let Some(buf) = receiv.0.get_mut().unwrap().try_iter().last() {
        info!("got dmatex");
        match dmatexs.set(&mut images, buf, None) {
            Ok(image) => {
                imported.push(image);
            }
            Err(err) => {
                error!("error while importing dmatex: {err}");
            }
        }
    }
}

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
