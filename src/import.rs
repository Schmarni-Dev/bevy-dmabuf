#![warn(clippy::unwrap_used, clippy::expect_used)]
use std::{
    fmt::Debug,
    ops::Deref,
    os::fd::{IntoRawFd as _, OwnedFd},
    sync::{Arc, Mutex},
};

use ash::vk::{
    self, CommandBufferBeginInfo, CommandPoolCreateInfo, FormatFeatureFlags2,
    ImagePlaneMemoryRequirementsInfo, MemoryDedicatedRequirements, MemoryRequirements2,
    SubresourceLayout,
};
use bevy::{
    app::Plugin,
    asset::{Assets, Handle, RenderAssetUsages},
    ecs::{
        resource::Resource,
        schedule::{IntoScheduleConfigs as _, SystemSet},
        system::{Res, ResMut},
        world::World,
    },
    image::Image,
    pbr::{PreparedMaterial, StandardMaterial},
    platform::collections::HashMap,
    render::{
        Render, RenderApp, RenderSet,
        extract_resource::{ExtractResource, ExtractResourcePlugin},
        render_asset::{RenderAssetDependency as _, RenderAssets, prepare_assets},
        render_resource::{Texture, TextureView},
        renderer::RenderDevice,
        texture::GpuImage,
    },
    utils::default,
};
use drm_fourcc::DrmFourcc;
use thiserror::Error;
use tracing::{debug, debug_span, error, warn};
use wgpu::{
    TextureUsages, TextureViewDescriptor,
    hal::{Device, MemoryFlags, TextureDescriptor, TextureUses, vulkan::Api as Vulkan},
};

use crate::{
    dmatex::Dmatex,
    format_mapping::{
        drm_fourcc_to_vk_format, get_drm_image_modifier_info, get_drm_modifiers, vk_format_to_srgb,
    },
    wgpu_init::vulkan_to_wgpu,
};

pub struct DmabufImportPlugin;

impl Plugin for DmabufImportPlugin {
    fn build(&self, app: &mut bevy::app::App) {
        let handles = ImportedDmatexs(default());
        app.insert_resource(handles.clone());
        app.add_plugins(ExtractResourcePlugin::<ImportedDmatexs>::default());
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app.configure_sets(
                Render,
                (
                    DmatexRenderSystemSet::InsertIntoGpuImages
                        .in_set(RenderSet::PrepareAssets)
                        .after(prepare_assets::<GpuImage>)
                        .before(prepare_assets::<PreparedMaterial<StandardMaterial>>),
                    DmatexRenderSystemSet::AcquireDmatexs
                        .in_set(RenderSet::PrepareAssets)
                        .after(DmatexRenderSystemSet::InsertIntoGpuImages),
                    DmatexRenderSystemSet::ReleaseDmatexs.in_set(RenderSet::Cleanup),
                ),
            );
            render_app.add_systems(
                Render,
                insert_dmatex_into_gpu_images.in_set(DmatexRenderSystemSet::InsertIntoGpuImages),
            );
            render_app.add_systems(
                Render,
                (
                    acquire_dmatex_images.in_set(DmatexRenderSystemSet::AcquireDmatexs),
                    release_dmatex_images.in_set(DmatexRenderSystemSet::ReleaseDmatexs),
                ),
            );
        } else {
            warn!("unable to init dmabuf importing!");
        }
    }
}

#[derive(SystemSet, Hash, Debug, Clone, PartialEq, Eq, Copy)]
pub enum DmatexRenderSystemSet {
    InsertIntoGpuImages,
    AcquireDmatexs,
    ReleaseDmatexs,
}

#[derive(Resource, Clone, ExtractResource)]
pub struct ImportedDmatexs(Arc<Mutex<HashMap<Handle<Image>, DmaImage>>>);

#[derive(Debug)]
enum DmaImage {
    UnImported(Dmatex, DropCallback, DmatexUsage),
    Imported(ImportedTexture),
}

#[derive(Clone, Copy, Debug)]
pub enum DmatexUsage {
    Sampling,
}

pub struct DropCallback(pub Option<Box<dyn FnOnce() + 'static + Send + Sync>>);
impl Debug for DropCallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("DropCallback").finish()
    }
}
impl Drop for DropCallback {
    fn drop(&mut self) {
        if let Some(callback) = self.0.take() {
            callback();
        }
    }
}

impl ImportedDmatexs {
    pub fn set(
        &self,
        images: &mut Assets<Image>,
        buf: Dmatex,
        usage: DmatexUsage,
        on_drop: Option<Box<dyn FnOnce() + 'static + Send + Sync>>,
    ) -> Result<Handle<Image>, ImportError> {
        let handle = get_handle(images, &buf)?;
        #[expect(clippy::unwrap_used)]
        self.0.lock().unwrap().insert(
            handle.clone_weak(),
            DmaImage::UnImported(buf, DropCallback(on_drop), usage),
        );
        Ok(handle)
    }
    pub fn insert_imported_dmatex(
        &self,
        images: &mut Assets<Image>,
        tex: ImportedTexture,
    ) -> Handle<Image> {
        let handle = debug_span!("creating dummy image").in_scope(|| {
            images.add(Image::new_uninit(
                tex.texture.size(),
                tex.texture.dimension(),
                tex.texture.format(),
                RenderAssetUsages::RENDER_WORLD,
            ))
        });

        let _span = debug_span!("inserting image handle").entered();
        #[expect(clippy::unwrap_used)]
        self.0
            .lock()
            .unwrap()
            .insert(handle.clone_weak(), DmaImage::Imported(tex));
        handle
    }
}

fn acquire_dmatex_images(world: &mut World) {
    let device = world.resource::<RenderDevice>();
    let dmatexs = world.resource::<ImportedDmatexs>();
    memory_barrier(device, dmatexs, ImageQueueTransfer::Acquire);
}
fn release_dmatex_images(world: &mut World) {
    let device = world.resource::<RenderDevice>();
    let dmatexs = world.resource::<ImportedDmatexs>();
    memory_barrier(device, dmatexs, ImageQueueTransfer::Release);
}

enum ImageQueueTransfer {
    Acquire,
    Release,
}

fn memory_barrier(
    device: &RenderDevice,
    dmatexs: &ImportedDmatexs,
    queue_transfer_direction: ImageQueueTransfer,
) {
    unsafe {
        device.wgpu_device().as_hal::<Vulkan, _, _>(|dev| {
            let Some(dev) = dev else {
                return;
            };
            let vk_dev = dev.raw_device();
            let Ok(command_pool) = vk_dev
                .create_command_pool(
                    &vk::CommandPoolCreateInfo {
                        flags: vk::CommandPoolCreateFlags::TRANSIENT,
                        queue_family_index: dev.queue_family_index(),
                        ..Default::default()
                    },
                    None,
                )
                .inspect_err(|e| error!("Unable to create command pool: {e}"))
            else {
                return;
            };

            let Ok(Some(buffer)) = dev
                .raw_device()
                .allocate_command_buffers(&vk::CommandBufferAllocateInfo {
                    command_pool,
                    level: vk::CommandBufferLevel::PRIMARY,
                    command_buffer_count: 1,
                    ..Default::default()
                })
                .inspect_err(|e| error!("Unable to allocate command buffer: {e}"))
                .map(|v| v.into_iter().next())
            else {
                vk_dev.destroy_command_pool(command_pool, None);
                return;
            };
            let Ok(texes) = dmatexs
                .0
                .lock()
                .inspect_err(|e| error!("Unable to lock dmatexs: {e}"))
            else {
                vk_dev.destroy_command_pool(command_pool, None);
                return;
            };

            vk_dev
                .begin_command_buffer(
                    buffer,
                    &CommandBufferBeginInfo {
                        flags: vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT,
                        ..Default::default()
                    },
                )
                .unwrap();

            let vk_submit_span = debug_span!("VK dmatex image acquire").entered();
            for image in texes
                .iter()
                .filter_map(|v| match v.1 {
                    DmaImage::UnImported(_, _, _) => None,
                    DmaImage::Imported(imported_texture) => Some(imported_texture),
                })
                .filter_map(|i| {
                    i.texture
                        .as_hal::<Vulkan, _, _>(|i| i.map(|i| i.raw_handle()))
                })
            {
                vk_dev.cmd_pipeline_barrier(
                    buffer,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[vk::ImageMemoryBarrier {
                        src_access_mask: vk::AccessFlags::NONE,
                        dst_access_mask: vk::AccessFlags::NONE,
                        old_layout: vk::ImageLayout::GENERAL,
                        new_layout: vk::ImageLayout::GENERAL,
                        // TODO: might want to use vk::QUEUE_FAMILY_FOREIGN_EXT instead
                        src_queue_family_index: match queue_transfer_direction {
                            ImageQueueTransfer::Acquire => vk::QUEUE_FAMILY_EXTERNAL,
                            ImageQueueTransfer::Release => dev.queue_family_index(),
                        },
                        dst_queue_family_index: match queue_transfer_direction {
                            ImageQueueTransfer::Acquire => dev.queue_family_index(),
                            ImageQueueTransfer::Release => vk::QUEUE_FAMILY_EXTERNAL,
                        },
                        image,
                        subresource_range: vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            base_mip_level: 0,
                            level_count: 1,
                            base_array_layer: 0,
                            layer_count: 1,
                        },
                        ..Default::default()
                    }],
                );
            }
            drop(vk_submit_span);
            vk_dev.end_command_buffer(buffer).unwrap();
            let fence = vk_dev
                .create_fence(
                    &vk::FenceCreateInfo {
                        flags: vk::FenceCreateFlags::empty(),
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();
            vk_dev
                .queue_submit(
                    dev.raw_queue(),
                    &[vk::SubmitInfo::default().command_buffers(&[buffer])],
                    fence,
                )
                .unwrap();
            vk_dev.wait_for_fences(&[fence], true, u64::MAX).unwrap();
            vk_dev.destroy_command_pool(command_pool, None);
        })
    };
}

fn insert_dmatex_into_gpu_images(
    mut gpu_images: ResMut<RenderAssets<GpuImage>>,
    imported: Res<ImportedDmatexs>,
    device: Res<RenderDevice>,
) {
    #[expect(clippy::unwrap_used)]
    let mut imported = imported.0.lock().unwrap();
    let handles = imported.keys().cloned().collect::<Vec<_>>();
    for handle in handles {
        // filter out outdated dmatexs
        if gpu_images.get(&handle).is_none() {
            imported.remove(&handle);
            continue;
        }
        if matches!(imported.get(&handle), Some(DmaImage::UnImported(_, _, _))) {
            if let Some(DmaImage::UnImported(dmabuf, on_drop, usage)) = imported.remove(&handle) {
                match import_texture(&device, dmabuf, on_drop, usage) {
                    Ok(tex) => {
                        debug!("imported dmatex");
                        imported.insert(handle.clone(), DmaImage::Imported(tex));
                    }
                    Err(err) => {
                        error!("failed to import dmatex: {err}");
                        continue;
                    }
                }
            }
        }
        let Some(render_tex) = gpu_images.get_mut(&handle) else {
            warn!("invalid texture handle (unreachable)");
            continue;
        };

        if let Some(DmaImage::Imported(tex)) = imported.get(&handle) {
            debug!("setting texture view!");
            render_tex.texture_view = tex.texture_view.clone();
            render_tex.size = tex.texture.size();
            render_tex.mip_level_count = tex.texture.mip_level_count();
            render_tex.texture = tex.texture.clone();
        } else {
            error!("unreachable");
        }
    }
}

fn get_handle(images: &mut Assets<Image>, buf: &Dmatex) -> Result<Handle<Image>, ImportError> {
    let desc = get_imported_descriptor(buf)?;
    Ok(images.add(Image::new_uninit(
        desc.size,
        desc.dimension,
        desc.format,
        RenderAssetUsages::RENDER_WORLD,
    )))
}

#[derive(Error, Debug, Clone, Copy)]
pub enum ImportError {
    #[error("Format is not compatible with Vulkan")]
    VulkanIncompatibleFormat,
    #[error("Format is not compatible with Wgpu")]
    WgpuIncompatibleFormat,
    #[error("Unsupported Modifier for Format")]
    ModifierInvalid,
    #[error("Unable to create Vulkan Image: {0}")]
    VulkanImageCreationFailed(vk::Result),
    #[error("Unrecognized Fourcc/Format")]
    UnrecognizedFourcc(#[from] drm_fourcc::UnrecognizedFourcc),
    #[error("RenderDevice is not a Vulkan Device")]
    NotVulkan,
    #[error("Unable to find valid Gpu Memory type index")]
    NoValidMemoryTypes,
    #[error("Unable to allocate Vulkan Gpu Memory: {0}")]
    VulkanMemoryAllocFailed(vk::Result),
    #[error("Unable to bind Vulkan Gpu Memory to Vulkan Image: {0}")]
    VulkanImageMemoryBindFailed(vk::Result),
    #[error(
        "The number of DmaTex planes does not equal the number of planes defined by the drm modifier"
    )]
    IncorrectNumberOfPlanes,
    #[error("No Planes to Import")]
    NoPlanes,
}

fn get_imported_descriptor(buf: &Dmatex) -> Result<wgpu::TextureDescriptor<'static>, ImportError> {
    let vulkan_format = drm_fourcc_to_vk_format(
        DrmFourcc::try_from(buf.format).map_err(ImportError::UnrecognizedFourcc)?,
    )
    .ok_or(ImportError::VulkanIncompatibleFormat)?;
    let vulkan_format = buf
        .srgb
        .then(|| vk_format_to_srgb(vulkan_format))
        .flatten()
        .unwrap_or(vulkan_format);
    Ok(wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d {
            width: buf.res.x,
            height: buf.res.y,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: vulkan_to_wgpu(vulkan_format).ok_or(ImportError::WgpuIncompatibleFormat)?,
        usage: TextureUsages::RENDER_ATTACHMENT
            | TextureUsages::TEXTURE_BINDING
            | TextureUsages::COPY_SRC
            | TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

#[derive(Clone, Debug)]
pub struct ImportedTexture {
    texture: Texture,
    texture_view: TextureView,
    usage: DmatexUsage,
}

#[tracing::instrument(level = "debug", skip(device, on_drop))]
pub fn import_texture(
    device: &RenderDevice,
    buf: Dmatex,
    on_drop: DropCallback,
    usage: DmatexUsage,
) -> Result<ImportedTexture, ImportError> {
    let vulkan_format = drm_fourcc_to_vk_format(
        DrmFourcc::try_from(buf.format).map_err(ImportError::UnrecognizedFourcc)?,
    )
    .ok_or(ImportError::VulkanIncompatibleFormat)?;
    let vulkan_format = buf
        .srgb
        .then(|| vk_format_to_srgb(vulkan_format))
        .flatten()
        .unwrap_or(vulkan_format);
    let wgpu_desc = get_imported_descriptor(&buf)?;
    let (image, mem) = unsafe {
        device
            .wgpu_device()
            .as_hal::<Vulkan, _, _>(|dev| -> Result<_, ImportError> {
                let dev = dev.ok_or(ImportError::NotVulkan)?;
                let (_format_properties, drm_format_properties) = get_drm_modifiers(
                    dev.shared_instance().raw_instance(),
                    dev.raw_physical_device(),
                    vulkan_format,
                );
                let mut disjoint = false;
                for plane in buf.planes.iter() {
                    let used_modifier = drm_format_properties
                        .iter()
                        .find(|v| v.drm_format_modifier == plane.modifier)
                        .ok_or(ImportError::ModifierInvalid)?;
                    disjoint |= used_modifier
                        .drm_format_modifier_tiling_features
                        .contains(FormatFeatureFlags2::DISJOINT_KHR);
                }
                let image_type = vk::ImageType::TYPE_2D;
                let usage_flags = vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::TRANSFER_DST;
                let create_flags = match disjoint {
                    true => vk::ImageCreateFlags::DISJOINT,
                    false => vk::ImageCreateFlags::empty(),
                };
                for plane in buf.planes.iter() {
                    let _format_info = get_drm_image_modifier_info(
                        dev.shared_instance().raw_instance(),
                        dev.raw_physical_device(),
                        vulkan_format,
                        image_type,
                        usage_flags,
                        create_flags,
                        plane.modifier,
                    )
                    .ok_or(ImportError::ModifierInvalid)?;
                }
                let plane_layouts = buf
                    .planes
                    .iter()
                    .map(|p| SubresourceLayout {
                        offset: p.offset as _,
                        row_pitch: p.stride as _,
                        array_pitch: 0,
                        depth_pitch: 0,
                        // per spec this has to be ignored by the impl
                        size: 0,
                    })
                    .collect::<Vec<_>>();
                let modifiers = buf.planes.iter().map(|p| p.modifier).collect::<Vec<_>>();
                if buf.planes.is_empty() {
                    return Err(ImportError::NoPlanes);
                }
                let mut drm_explicit_create_info = (buf.planes.len() == 1).then(|| {
                    vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
                        .drm_format_modifier(modifiers[0])
                        .plane_layouts(&plane_layouts)
                });
                let mut drm_list_create_info = (buf.planes.len() > 1).then(|| {
                    vk::ImageDrmFormatModifierListCreateInfoEXT::default()
                        .drm_format_modifiers(&modifiers)
                });
                let mut external_memory_info = vk::ExternalMemoryImageCreateInfo::default()
                    .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

                let mut image_create_info = vk::ImageCreateInfo::default()
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .image_type(image_type)
                    .usage(usage_flags)
                    .flags(create_flags)
                    .format(vulkan_format)
                    .extent(vk::Extent3D {
                        width: buf.res.x,
                        height: buf.res.y,
                        depth: 1,
                    })
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .array_layers(1)
                    .mip_levels(1)
                    .initial_layout(vk::ImageLayout::UNDEFINED)
                    .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                    .push_next(&mut external_memory_info);
                if let Some(info) = drm_explicit_create_info.as_mut() {
                    image_create_info = image_create_info.push_next(info);
                }
                if let Some(info) = drm_list_create_info.as_mut() {
                    image_create_info = image_create_info.push_next(info);
                }
                let image = dev
                    .raw_device()
                    .create_image(&image_create_info, None)
                    .map_err(ImportError::VulkanImageCreationFailed)?;

                let mem_properties = {
                    dev.shared_instance()
                        .raw_instance()
                        .get_physical_device_memory_properties(dev.raw_physical_device())
                };
                let memory_types = &mem_properties.memory_types_as_slice();
                let valid_memory_types =
                    memory_types
                        .iter()
                        .enumerate()
                        .fold(u32::MAX, |u, (i, mem)| {
                            if (vk::MemoryPropertyFlags::RDMA_CAPABLE_NV
                                | vk::MemoryPropertyFlags::DEVICE_COHERENT_AMD
                                | vk::MemoryPropertyFlags::PROTECTED
                                | vk::MemoryPropertyFlags::LAZILY_ALLOCATED)
                                .intersects(mem.property_flags)
                            {
                                u & !(1 << i)
                            } else {
                                u
                            }
                        });
                let index = memory_types
                    .iter()
                    .zip(0u32..)
                    .find(|(t, _)| {
                        t.property_flags
                            .intersects(vk::MemoryPropertyFlags::from_raw(valid_memory_types))
                    })
                    .ok_or(ImportError::NoValidMemoryTypes)?
                    .1;
                let mut plane_mems = Vec::with_capacity(4);
                match disjoint {
                    true => {
                        for (i, v) in buf.planes.into_iter().enumerate() {
                            let fd = OwnedFd::from(v.dmabuf_fd);
                            let aspect_flags = match i {
                                0 => vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
                                1 => vk::ImageAspectFlags::MEMORY_PLANE_1_EXT,
                                2 => vk::ImageAspectFlags::MEMORY_PLANE_2_EXT,
                                3 => vk::ImageAspectFlags::MEMORY_PLANE_3_EXT,
                                _ => return Err(ImportError::IncorrectNumberOfPlanes),
                            };
                            let mut dedicated_req = MemoryDedicatedRequirements::default();
                            let mut plane_req_info = ImagePlaneMemoryRequirementsInfo::default()
                                .plane_aspect(aspect_flags);
                            let mem_req_info = vk::ImageMemoryRequirementsInfo2::default()
                                .image(image)
                                .push_next(&mut plane_req_info);
                            let mut mem_reqs =
                                MemoryRequirements2::default().push_next(&mut dedicated_req);
                            dev.raw_device()
                                .get_image_memory_requirements2(&mem_req_info, &mut mem_reqs);
                            let needs_dedicated = dedicated_req.requires_dedicated_allocation != 0;
                            let layout = dev.raw_device().get_image_subresource_layout(
                                image,
                                vk::ImageSubresource::default().aspect_mask(aspect_flags),
                            );

                            let mut external_fd_info = vk::ImportMemoryFdInfoKHR::default()
                                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                                .fd(fd.into_raw_fd());

                            let mut dedicated =
                                vk::MemoryDedicatedAllocateInfo::default().image(image);
                            let mut alloc_info = vk::MemoryAllocateInfo::default()
                                .allocation_size(layout.size)
                                .memory_type_index(index)
                                .push_next(&mut external_fd_info);
                            if needs_dedicated {
                                alloc_info = alloc_info.push_next(&mut dedicated);
                            }

                            let mem = dev
                                .raw_device()
                                .allocate_memory(&alloc_info, None)
                                .inspect_err(|_| dev.raw_device().destroy_image(image, None))
                                .map_err(ImportError::VulkanMemoryAllocFailed)?;
                            plane_mems.push((
                                mem,
                                Some(
                                    vk::BindImagePlaneMemoryInfo::default()
                                        .plane_aspect(aspect_flags),
                                ),
                            ));
                        }
                    }
                    false => {
                        let fd = OwnedFd::from(
                            buf.planes
                                .into_iter()
                                .next()
                                .ok_or(ImportError::NoPlanes)?
                                .dmabuf_fd,
                        );
                        let mut dedicated_req = MemoryDedicatedRequirements::default();
                        let mut mem_reqs =
                            MemoryRequirements2::default().push_next(&mut dedicated_req);
                        let mem_req_info = vk::ImageMemoryRequirementsInfo2::default().image(image);
                        dev.raw_device()
                            .get_image_memory_requirements2(&mem_req_info, &mut mem_reqs);
                        let size = mem_reqs.memory_requirements.size;

                        let needs_dedicated = dedicated_req.requires_dedicated_allocation != 0;

                        let mut external_fd_info = vk::ImportMemoryFdInfoKHR::default()
                            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                            .fd(fd.into_raw_fd());
                        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
                        let mut alloc_info = vk::MemoryAllocateInfo::default()
                            .allocation_size(size)
                            .memory_type_index(index)
                            .push_next(&mut external_fd_info);
                        if needs_dedicated {
                            alloc_info = alloc_info.push_next(&mut dedicated);
                        }
                        let mem = dev
                            .raw_device()
                            .allocate_memory(&alloc_info, None)
                            .inspect_err(|_| dev.raw_device().destroy_image(image, None))
                            .map_err(ImportError::VulkanMemoryAllocFailed)?;
                        plane_mems.push((mem, None));
                    }
                }
                let bind_infos = plane_mems
                    .iter_mut()
                    .map(|(mem, info)| match info {
                        Some(info) => vk::BindImageMemoryInfo::default()
                            .image(image)
                            .memory(*mem)
                            .push_next(info),
                        None => vk::BindImageMemoryInfo::default().image(image).memory(*mem),
                    })
                    .collect::<Vec<_>>();
                dev.raw_device()
                    .bind_image_memory2(&bind_infos)
                    .map_err(ImportError::VulkanImageMemoryBindFailed)?;

                Ok((image, plane_mems))
            })
    }?;
    let descriptor = TextureDescriptor {
        label: None,
        size: wgpu::Extent3d {
            width: buf.res.x,
            height: buf.res.y,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: vulkan_to_wgpu(vulkan_format).ok_or(ImportError::WgpuIncompatibleFormat)?,
        usage: TextureUses::COLOR_TARGET | TextureUses::PRESENT,
        memory_flags: MemoryFlags::empty(),
        view_formats: vec![],
    };
    let texture = unsafe {
        wgpu::hal::vulkan::Device::texture_from_raw(
            image,
            &descriptor,
            Some({
                let dev = device.clone();
                Box::new(move || {
                    let _on_drop = on_drop;
                    dev.wgpu_device().as_hal::<Vulkan, _, _>(move |dev| {
                        if let Some(dev) = dev {
                            for (mem, _) in mem {
                                dev.raw_device().free_memory(mem, None);
                            }
                            dev.raw_device().destroy_image(image, None);
                        }
                    });
                })
            }),
        )
    };
    let wgpu_texture = unsafe {
        device
            .wgpu_device()
            .create_texture_from_hal::<Vulkan>(texture, &wgpu_desc)
    };
    let texture = Texture::from(wgpu_texture);
    let texture_view = texture.create_view(&TextureViewDescriptor {
        label: None,
        format: Some(texture.format()),
        dimension: Some(wgpu::TextureViewDimension::D2),
        usage: Some(texture.usage()),
        aspect: wgpu::TextureAspect::All,
        base_mip_level: 0,
        mip_level_count: Some(texture.mip_level_count()),
        base_array_layer: 0,
        array_layer_count: Some(texture.depth_or_array_layers()),
    });
    Ok(ImportedTexture {
        texture,
        texture_view,
        usage,
    })
}
