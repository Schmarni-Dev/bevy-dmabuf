/// Plugin to init the vulkan session with the required extensions,
/// probably not needed when using bevy_mod_openxr
#[derive(Default)]
pub struct DmabufWgpuInitPlugin {
    pub additional_device_extensions: Vec<&'static std::ffi::CStr>,
}

pub fn add_dmabuf_init_plugin<G: PluginGroup>(
    plugins: G,
    additional_device_extensions: impl IntoIterator<Item = &'static std::ffi::CStr>,
) -> PluginGroupBuilder {
    plugins
        .build()
        .disable::<RenderPlugin>()
        .add_before::<RenderPlugin>(DmabufWgpuInitPlugin {
            additional_device_extensions: additional_device_extensions.into_iter().collect(),
        })
}

impl Plugin for DmabufWgpuInitPlugin {
    fn build(&self, app: &mut bevy::app::App) {
        let (device, queue, adapter_info, adapter, instance) =
            init_graphics(&self.additional_device_extensions).unwrap();
        app.add_plugins(RenderPlugin {
            render_creation: RenderCreation::Manual(RenderResources(
                device.into(),
                RenderQueue(Arc::new(WgpuWrapper::new(queue))),
                RenderAdapterInfo(WgpuWrapper::new(adapter_info)),
                RenderAdapter(Arc::new(WgpuWrapper::new(adapter))),
                RenderInstance(Arc::new(WgpuWrapper::new(instance))),
            )),
            synchronous_pipeline_compilation: false,
            debug_flags: RenderDebugFlags::default(),
        });
    }
}

use std::sync::Arc;

use ash::vk::PhysicalDeviceType;
use bevy::app::{Plugin, PluginGroup, PluginGroupBuilder};
use tracing::debug;
use bevy::render::renderer::{
    RenderAdapter, RenderAdapterInfo, RenderInstance, RenderQueue, WgpuWrapper,
};
use bevy::render::settings::{RenderCreation, RenderResources};
use bevy::render::{RenderDebugFlags, RenderPlugin};
use color_eyre::eyre::bail;
use wgpu::hal::Api;
use wgpu::hal::api::Vulkan;

use crate::required_device_extensions;

#[cfg(not(target_os = "android"))]
const VK_TARGET_VERSION_ASH: u32 = ash::vk::make_api_version(0, 1, 2, 0);
#[cfg(target_os = "android")]
const VK_TARGET_VERSION_ASH: u32 = ash::vk::make_api_version(0, 1, 1, 0);

fn init_graphics(
    additional_device_extensions: &[&'static std::ffi::CStr],
) -> color_eyre::Result<(
    wgpu::Device,
    wgpu::Queue,
    wgpu::AdapterInfo,
    wgpu::Adapter,
    wgpu::Instance,
)> {
    let vk_entry = unsafe { ash::Entry::load() }?;
    let flags = wgpu::InstanceFlags::default().with_env();
    let mut instance_extensions =
        <Vulkan as Api>::Instance::desired_extensions(&vk_entry, VK_TARGET_VERSION_ASH, flags)?;
    instance_extensions.dedup();
    let mut device_extensions = vec![
        // #[cfg(target_os = "android")]
        ash::khr::draw_indirect_count::NAME,
        ash::khr::timeline_semaphore::NAME,
        ash::khr::imageless_framebuffer::NAME,
        ash::khr::image_format_list::NAME,
        #[cfg(target_os = "macos")]
        ash::khr::portability_subset::NAME,
        #[cfg(target_os = "macos")]
        ash::ext::metal_objects::NAME,
    ];
    device_extensions.extend(required_device_extensions());
    device_extensions.extend(additional_device_extensions);
    device_extensions.dedup();

    let vk_instance = unsafe {
        let extensions_cchar: Vec<_> = instance_extensions.iter().map(|s| s.as_ptr()).collect();

        let app_name = c"bevy app";
        let vk_app_info = ash::vk::ApplicationInfo::default()
            .application_name(app_name)
            .application_version(1)
            .engine_name(c"bevy")
            .engine_version(15)
            .api_version(VK_TARGET_VERSION_ASH);

        vk_entry.create_instance(
            &ash::vk::InstanceCreateInfo::default()
                .application_info(&vk_app_info)
                .enabled_extension_names(&extensions_cchar),
            None,
        )?
    };
    let api_layers = unsafe { vk_entry.enumerate_instance_layer_properties()? };
    let has_nv_optimus = api_layers.iter().any(|v| {
        v.layer_name_as_c_str()
            .is_ok_and(|v| v == c"VK_LAYER_NV_optimus")
    });

    drop(api_layers);
    let version = { unsafe { vk_entry.try_enumerate_instance_version() } };
    let instance_api_version = match version {
        // Vulkan 1.1+
        Ok(Some(version)) => version,
        Ok(None) => ash::vk::API_VERSION_1_0,
        Err(err) => {
            bail!("try_enumerate_instance_version() failed: {err}")
        }
    };

    // the android_sdk_version stuff is copied from wgpu
    #[cfg(target_os = "android")]
    let android_sdk_version = {
        let properties = android_system_properties::AndroidSystemProperties::new();
        // See: https://developer.android.com/reference/android/os/Build.VERSION_CODES
        if let Some(val) = properties.get("ro.build.version.sdk") {
            match val.parse::<u32>() {
                Ok(sdk_ver) => sdk_ver,
                Err(err) => {
                    error!(
                        concat!(
                            "Couldn't parse Android's ",
                            "ro.build.version.sdk system property ({}): {}",
                        ),
                        val, err,
                    );
                    0
                }
            }
        } else {
            error!("Couldn't read Android's ro.build.version.sdk system property");
            0
        }
    };
    #[cfg(not(target_os = "android"))]
    let android_sdk_version = 0;

    let wgpu_vk_instance = unsafe {
        <Vulkan as Api>::Instance::from_raw(
            vk_entry.clone(),
            vk_instance.clone(),
            instance_api_version,
            android_sdk_version,
            None,
            instance_extensions,
            flags,
            has_nv_optimus,
            None,
        )?
    };
    let vk_physical_device = {
        let mut devices = unsafe { vk_instance.enumerate_physical_devices()? };
        devices.sort_by_key(|physical_device| {
            match unsafe {
                vk_instance
                    .get_physical_device_properties(*physical_device)
                    .device_type
            } {
                PhysicalDeviceType::DISCRETE_GPU => 1,
                PhysicalDeviceType::INTEGRATED_GPU => 2,
                PhysicalDeviceType::OTHER => 3,
                PhysicalDeviceType::VIRTUAL_GPU => 4,
                PhysicalDeviceType::CPU => 5,
                _ => 6,
            }
        });
        let Some(phys_dev) = devices.into_iter().next() else {
            bail!("unable to find physical device");
        };
        phys_dev
    };
    let Some(wgpu_exposed_adapter) = wgpu_vk_instance.expose_adapter(vk_physical_device) else {
        bail!("WGPU failed to provide an adapter");
    };
    let wgpu_features = wgpu_exposed_adapter.features;
    debug!("wgpu features: {wgpu_features:#?}");

    let enabled_extensions = wgpu_exposed_adapter
        .adapter
        .required_device_extensions(wgpu_features);

    let wgpu_open_device = {
        let extensions_cchar: Vec<_> = device_extensions.iter().map(|s| s.as_ptr()).collect();
        let mut enabled_phd_features = wgpu_exposed_adapter
            .adapter
            .physical_device_features(&enabled_extensions, wgpu_features);
        let family_index = 0;
        let family_info = ash::vk::DeviceQueueCreateInfo::default()
            .queue_family_index(family_index)
            .queue_priorities(&[1.0]);
        let family_infos = [family_info];
        let mut physical_device_multiview_features = ash::vk::PhysicalDeviceMultiviewFeatures {
            multiview: ash::vk::TRUE,
            ..Default::default()
        };
        let info = enabled_phd_features
            .add_to_device_create(
                ash::vk::DeviceCreateInfo::default()
                    .queue_create_infos(&family_infos)
                    .push_next(&mut physical_device_multiview_features),
            )
            .enabled_extension_names(&extensions_cchar);
        let vk_device = unsafe { vk_instance.create_device(vk_physical_device, &info, None)? };

        unsafe {
            wgpu_exposed_adapter.adapter.device_from_raw(
                vk_device,
                None,
                &enabled_extensions,
                wgpu_features,
                &wgpu::MemoryHints::Performance,
                family_info.queue_family_index,
                0,
            )
        }?
    };

    let wgpu_instance =
        unsafe { wgpu::Instance::from_hal::<wgpu::hal::api::Vulkan>(wgpu_vk_instance) };
    let wgpu_adapter = unsafe { wgpu_instance.create_adapter_from_hal(wgpu_exposed_adapter) };
    let limits = wgpu_adapter.limits();
    debug!("wgpu_limits: {limits:#?}");
    let (wgpu_device, wgpu_queue) = unsafe {
        wgpu_adapter.create_device_from_hal(
            wgpu_open_device,
            &wgpu::DeviceDescriptor {
                label: None,
                required_features: wgpu_features,
                required_limits: limits,
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        )
    }?;

    Ok((
        wgpu_device,
        wgpu_queue,
        wgpu_adapter.get_info(),
        wgpu_adapter,
        wgpu_instance,
    ))
}

pub fn vulkan_to_wgpu(format: ash::vk::Format) -> Option<wgpu::TextureFormat> {
    use ash::vk::Format as F;
    use wgpu::TextureFormat as Tf;
    use wgpu::{AstcBlock, AstcChannel};
    Some(match format {
        F::R8_UNORM => Tf::R8Unorm,
        F::R8_SNORM => Tf::R8Snorm,
        F::R8_UINT => Tf::R8Uint,
        F::R8_SINT => Tf::R8Sint,
        F::R16_UINT => Tf::R16Uint,
        F::R16_SINT => Tf::R16Sint,
        F::R16_UNORM => Tf::R16Unorm,
        F::R16_SNORM => Tf::R16Snorm,
        F::R16_SFLOAT => Tf::R16Float,
        F::R8G8_UNORM => Tf::Rg8Unorm,
        F::R8G8_SNORM => Tf::Rg8Snorm,
        F::R8G8_UINT => Tf::Rg8Uint,
        F::R8G8_SINT => Tf::Rg8Sint,
        F::R16G16_UNORM => Tf::Rg16Unorm,
        F::R16G16_SNORM => Tf::Rg16Snorm,
        F::R32_UINT => Tf::R32Uint,
        F::R32_SINT => Tf::R32Sint,
        F::R32_SFLOAT => Tf::R32Float,
        F::R16G16_UINT => Tf::Rg16Uint,
        F::R16G16_SINT => Tf::Rg16Sint,
        F::R16G16_SFLOAT => Tf::Rg16Float,
        F::R8G8B8A8_UNORM => Tf::Rgba8Unorm,
        F::R8G8B8A8_SRGB => Tf::Rgba8UnormSrgb,
        F::B8G8R8A8_SRGB => Tf::Bgra8UnormSrgb,
        F::R8G8B8A8_SNORM => Tf::Rgba8Snorm,
        F::B8G8R8A8_UNORM => Tf::Bgra8Unorm,
        F::B8G8R8A8_UINT => Tf::Bgra8Unorm,
        F::R8G8B8A8_UINT => Tf::Rgba8Uint,
        F::R8G8B8A8_SINT => Tf::Rgba8Sint,
        F::A2B10G10R10_UINT_PACK32 => Tf::Rgb10a2Uint,
        F::A2B10G10R10_UNORM_PACK32 => Tf::Rgb10a2Unorm,
        F::B10G11R11_UFLOAT_PACK32 => Tf::Rg11b10Ufloat,
        F::R32G32_UINT => Tf::Rg32Uint,
        F::R32G32_SINT => Tf::Rg32Sint,
        F::R32G32_SFLOAT => Tf::Rg32Float,
        F::R16G16B16A16_UINT => Tf::Rgba16Uint,
        F::R16G16B16A16_SINT => Tf::Rgba16Sint,
        F::R16G16B16A16_UNORM => Tf::Rgba16Unorm,
        F::R16G16B16A16_SNORM => Tf::Rgba16Snorm,
        F::R16G16B16A16_SFLOAT => Tf::Rgba16Float,
        F::R32G32B32A32_UINT => Tf::Rgba32Uint,
        F::R32G32B32A32_SINT => Tf::Rgba32Sint,
        F::R32G32B32A32_SFLOAT => Tf::Rgba32Float,
        F::D32_SFLOAT => Tf::Depth32Float,
        F::D32_SFLOAT_S8_UINT => Tf::Depth32FloatStencil8,
        F::D16_UNORM => Tf::Depth16Unorm,
        F::G8_B8R8_2PLANE_420_UNORM => Tf::NV12,
        F::E5B9G9R9_UFLOAT_PACK32 => Tf::Rgb9e5Ufloat,
        F::BC1_RGBA_UNORM_BLOCK => Tf::Bc1RgbaUnorm,
        F::BC1_RGBA_SRGB_BLOCK => Tf::Bc1RgbaUnormSrgb,
        F::BC2_UNORM_BLOCK => Tf::Bc2RgbaUnorm,
        F::BC2_SRGB_BLOCK => Tf::Bc2RgbaUnormSrgb,
        F::BC3_UNORM_BLOCK => Tf::Bc3RgbaUnorm,
        F::BC3_SRGB_BLOCK => Tf::Bc3RgbaUnormSrgb,
        F::BC4_UNORM_BLOCK => Tf::Bc4RUnorm,
        F::BC4_SNORM_BLOCK => Tf::Bc4RSnorm,
        F::BC5_UNORM_BLOCK => Tf::Bc5RgUnorm,
        F::BC5_SNORM_BLOCK => Tf::Bc5RgSnorm,
        F::BC6H_UFLOAT_BLOCK => Tf::Bc6hRgbUfloat,
        F::BC6H_SFLOAT_BLOCK => Tf::Bc6hRgbFloat,
        F::BC7_UNORM_BLOCK => Tf::Bc7RgbaUnorm,
        F::BC7_SRGB_BLOCK => Tf::Bc7RgbaUnormSrgb,
        F::ETC2_R8G8B8_UNORM_BLOCK => Tf::Etc2Rgb8Unorm,
        F::ETC2_R8G8B8_SRGB_BLOCK => Tf::Etc2Rgb8UnormSrgb,
        F::ETC2_R8G8B8A1_UNORM_BLOCK => Tf::Etc2Rgb8A1Unorm,
        F::ETC2_R8G8B8A1_SRGB_BLOCK => Tf::Etc2Rgb8A1UnormSrgb,
        F::ETC2_R8G8B8A8_UNORM_BLOCK => Tf::Etc2Rgba8Unorm,
        F::ETC2_R8G8B8A8_SRGB_BLOCK => Tf::Etc2Rgba8UnormSrgb,
        F::EAC_R11_UNORM_BLOCK => Tf::EacR11Unorm,
        F::EAC_R11_SNORM_BLOCK => Tf::EacR11Snorm,
        F::EAC_R11G11_UNORM_BLOCK => Tf::EacRg11Unorm,
        F::EAC_R11G11_SNORM_BLOCK => Tf::EacRg11Snorm,
        F::ASTC_4X4_UNORM_BLOCK => Tf::Astc {
            block: AstcBlock::B4x4,
            channel: AstcChannel::Unorm,
        },
        F::ASTC_5X4_UNORM_BLOCK => Tf::Astc {
            block: AstcBlock::B5x4,
            channel: AstcChannel::Unorm,
        },
        F::ASTC_5X5_UNORM_BLOCK => Tf::Astc {
            block: AstcBlock::B5x5,
            channel: AstcChannel::Unorm,
        },
        F::ASTC_6X5_UNORM_BLOCK => Tf::Astc {
            block: AstcBlock::B6x5,
            channel: AstcChannel::Unorm,
        },
        F::ASTC_6X6_UNORM_BLOCK => Tf::Astc {
            block: AstcBlock::B6x6,
            channel: AstcChannel::Unorm,
        },
        F::ASTC_8X5_UNORM_BLOCK => Tf::Astc {
            block: AstcBlock::B8x5,
            channel: AstcChannel::Unorm,
        },
        F::ASTC_8X6_UNORM_BLOCK => Tf::Astc {
            block: AstcBlock::B8x6,
            channel: AstcChannel::Unorm,
        },
        F::ASTC_8X8_UNORM_BLOCK => Tf::Astc {
            block: AstcBlock::B8x8,
            channel: AstcChannel::Unorm,
        },
        F::ASTC_10X5_UNORM_BLOCK => Tf::Astc {
            block: AstcBlock::B10x5,
            channel: AstcChannel::Unorm,
        },
        F::ASTC_10X6_UNORM_BLOCK => Tf::Astc {
            block: AstcBlock::B10x6,
            channel: AstcChannel::Unorm,
        },
        F::ASTC_10X8_UNORM_BLOCK => Tf::Astc {
            block: AstcBlock::B10x8,
            channel: AstcChannel::Unorm,
        },
        F::ASTC_10X10_UNORM_BLOCK => Tf::Astc {
            block: AstcBlock::B10x10,
            channel: AstcChannel::Unorm,
        },
        F::ASTC_12X10_UNORM_BLOCK => Tf::Astc {
            block: AstcBlock::B12x10,
            channel: AstcChannel::Unorm,
        },
        F::ASTC_12X12_UNORM_BLOCK => Tf::Astc {
            block: AstcBlock::B12x12,
            channel: AstcChannel::Unorm,
        },
        F::ASTC_4X4_SRGB_BLOCK => Tf::Astc {
            block: AstcBlock::B4x4,
            channel: AstcChannel::UnormSrgb,
        },
        F::ASTC_5X4_SRGB_BLOCK => Tf::Astc {
            block: AstcBlock::B5x4,
            channel: AstcChannel::UnormSrgb,
        },
        F::ASTC_5X5_SRGB_BLOCK => Tf::Astc {
            block: AstcBlock::B5x5,
            channel: AstcChannel::UnormSrgb,
        },
        F::ASTC_6X5_SRGB_BLOCK => Tf::Astc {
            block: AstcBlock::B6x5,
            channel: AstcChannel::UnormSrgb,
        },
        F::ASTC_6X6_SRGB_BLOCK => Tf::Astc {
            block: AstcBlock::B6x6,
            channel: AstcChannel::UnormSrgb,
        },
        F::ASTC_8X5_SRGB_BLOCK => Tf::Astc {
            block: AstcBlock::B8x5,
            channel: AstcChannel::UnormSrgb,
        },
        F::ASTC_8X6_SRGB_BLOCK => Tf::Astc {
            block: AstcBlock::B8x6,
            channel: AstcChannel::UnormSrgb,
        },
        F::ASTC_8X8_SRGB_BLOCK => Tf::Astc {
            block: AstcBlock::B8x8,
            channel: AstcChannel::UnormSrgb,
        },
        F::ASTC_10X5_SRGB_BLOCK => Tf::Astc {
            block: AstcBlock::B10x5,
            channel: AstcChannel::UnormSrgb,
        },
        F::ASTC_10X6_SRGB_BLOCK => Tf::Astc {
            block: AstcBlock::B10x6,
            channel: AstcChannel::UnormSrgb,
        },
        F::ASTC_10X8_SRGB_BLOCK => Tf::Astc {
            block: AstcBlock::B10x8,
            channel: AstcChannel::UnormSrgb,
        },
        F::ASTC_10X10_SRGB_BLOCK => Tf::Astc {
            block: AstcBlock::B10x10,
            channel: AstcChannel::UnormSrgb,
        },
        F::ASTC_12X10_SRGB_BLOCK => Tf::Astc {
            block: AstcBlock::B12x10,
            channel: AstcChannel::UnormSrgb,
        },
        F::ASTC_12X12_SRGB_BLOCK => Tf::Astc {
            block: AstcBlock::B12x12,
            channel: AstcChannel::UnormSrgb,
        },
        F::ASTC_4X4_SFLOAT_BLOCK_EXT => Tf::Astc {
            block: AstcBlock::B4x4,
            channel: AstcChannel::Hdr,
        },
        F::ASTC_5X4_SFLOAT_BLOCK_EXT => Tf::Astc {
            block: AstcBlock::B5x4,
            channel: AstcChannel::Hdr,
        },
        F::ASTC_5X5_SFLOAT_BLOCK_EXT => Tf::Astc {
            block: AstcBlock::B5x5,
            channel: AstcChannel::Hdr,
        },
        F::ASTC_6X5_SFLOAT_BLOCK_EXT => Tf::Astc {
            block: AstcBlock::B6x5,
            channel: AstcChannel::Hdr,
        },
        F::ASTC_6X6_SFLOAT_BLOCK_EXT => Tf::Astc {
            block: AstcBlock::B6x6,
            channel: AstcChannel::Hdr,
        },
        F::ASTC_8X5_SFLOAT_BLOCK_EXT => Tf::Astc {
            block: AstcBlock::B8x5,
            channel: AstcChannel::Hdr,
        },
        F::ASTC_8X6_SFLOAT_BLOCK_EXT => Tf::Astc {
            block: AstcBlock::B8x6,
            channel: AstcChannel::Hdr,
        },
        F::ASTC_8X8_SFLOAT_BLOCK_EXT => Tf::Astc {
            block: AstcBlock::B8x8,
            channel: AstcChannel::Hdr,
        },
        F::ASTC_10X5_SFLOAT_BLOCK_EXT => Tf::Astc {
            block: AstcBlock::B10x5,
            channel: AstcChannel::Hdr,
        },
        F::ASTC_10X6_SFLOAT_BLOCK_EXT => Tf::Astc {
            block: AstcBlock::B10x6,
            channel: AstcChannel::Hdr,
        },
        F::ASTC_10X8_SFLOAT_BLOCK_EXT => Tf::Astc {
            block: AstcBlock::B10x8,
            channel: AstcChannel::Hdr,
        },
        F::ASTC_10X10_SFLOAT_BLOCK_EXT => Tf::Astc {
            block: AstcBlock::B10x10,
            channel: AstcChannel::Hdr,
        },
        F::ASTC_12X10_SFLOAT_BLOCK_EXT => Tf::Astc {
            block: AstcBlock::B12x10,
            channel: AstcChannel::Hdr,
        },
        F::ASTC_12X12_SFLOAT_BLOCK_EXT => Tf::Astc {
            block: AstcBlock::B12x12,
            channel: AstcChannel::Hdr,
        },
        _ => return None,
    })
}

// fn wgpu_to_vulkan(format: wgpu::TextureFormat) -> Option<ash::vk::Format> {
//     // Copied with minor modification from:
//     // https://github.com/gfx-rs/wgpu/blob/a7defb723f856d946d6d220e9897d20dbb7b8f61/wgpu-hal/src/vulkan/conv.rs#L5-L151
//     // license: MIT OR Apache-2.0
//     use ash::vk::Format as F;
//     use wgpu::TextureFormat as Tf;
//     use wgpu::{AstcBlock, AstcChannel};
//     Some(match format {
//         Tf::R8Unorm => F::R8_UNORM,
//         Tf::R8Snorm => F::R8_SNORM,
//         Tf::R8Uint => F::R8_UINT,
//         Tf::R8Sint => F::R8_SINT,
//         Tf::R16Uint => F::R16_UINT,
//         Tf::R16Sint => F::R16_SINT,
//         Tf::R16Unorm => F::R16_UNORM,
//         Tf::R16Snorm => F::R16_SNORM,
//         Tf::R16Float => F::R16_SFLOAT,
//         Tf::Rg8Unorm => F::R8G8_UNORM,
//         Tf::Rg8Snorm => F::R8G8_SNORM,
//         Tf::Rg8Uint => F::R8G8_UINT,
//         Tf::Rg8Sint => F::R8G8_SINT,
//         Tf::Rg16Unorm => F::R16G16_UNORM,
//         Tf::Rg16Snorm => F::R16G16_SNORM,
//         Tf::R32Uint => F::R32_UINT,
//         Tf::R32Sint => F::R32_SINT,
//         Tf::R32Float => F::R32_SFLOAT,
//         Tf::R64Uint => F::R64_UINT,
//         Tf::Rg16Uint => F::R16G16_UINT,
//         Tf::Rg16Sint => F::R16G16_SINT,
//         Tf::Rg16Float => F::R16G16_SFLOAT,
//         Tf::Rgba8Unorm => F::R8G8B8A8_UNORM,
//         Tf::Rgba8UnormSrgb => F::R8G8B8A8_SRGB,
//         Tf::Bgra8UnormSrgb => F::B8G8R8A8_SRGB,
//         Tf::Rgba8Snorm => F::R8G8B8A8_SNORM,
//         Tf::Bgra8Unorm => F::B8G8R8A8_UNORM,
//         Tf::Rgba8Uint => F::R8G8B8A8_UINT,
//         Tf::Rgba8Sint => F::R8G8B8A8_SINT,
//         Tf::Rgb10a2Uint => F::A2B10G10R10_UINT_PACK32,
//         Tf::Rgb10a2Unorm => F::A2B10G10R10_UNORM_PACK32,
//         Tf::Rg11b10Ufloat => F::B10G11R11_UFLOAT_PACK32,
//         Tf::Rg32Uint => F::R32G32_UINT,
//         Tf::Rg32Sint => F::R32G32_SINT,
//         Tf::Rg32Float => F::R32G32_SFLOAT,
//         Tf::Rgba16Uint => F::R16G16B16A16_UINT,
//         Tf::Rgba16Sint => F::R16G16B16A16_SINT,
//         Tf::Rgba16Unorm => F::R16G16B16A16_UNORM,
//         Tf::Rgba16Snorm => F::R16G16B16A16_SNORM,
//         Tf::Rgba16Float => F::R16G16B16A16_SFLOAT,
//         Tf::Rgba32Uint => F::R32G32B32A32_UINT,
//         Tf::Rgba32Sint => F::R32G32B32A32_SINT,
//         Tf::Rgba32Float => F::R32G32B32A32_SFLOAT,
//         Tf::Depth32Float => F::D32_SFLOAT,
//         Tf::Depth32FloatStencil8 => F::D32_SFLOAT_S8_UINT,
//         Tf::Depth24Plus | Tf::Depth24PlusStencil8 | Tf::Stencil8 => return None, // Dependent on device properties
//         Tf::Depth16Unorm => F::D16_UNORM,
//         Tf::NV12 => F::G8_B8R8_2PLANE_420_UNORM,
//         Tf::Rgb9e5Ufloat => F::E5B9G9R9_UFLOAT_PACK32,
//         Tf::Bc1RgbaUnorm => F::BC1_RGBA_UNORM_BLOCK,
//         Tf::Bc1RgbaUnormSrgb => F::BC1_RGBA_SRGB_BLOCK,
//         Tf::Bc2RgbaUnorm => F::BC2_UNORM_BLOCK,
//         Tf::Bc2RgbaUnormSrgb => F::BC2_SRGB_BLOCK,
//         Tf::Bc3RgbaUnorm => F::BC3_UNORM_BLOCK,
//         Tf::Bc3RgbaUnormSrgb => F::BC3_SRGB_BLOCK,
//         Tf::Bc4RUnorm => F::BC4_UNORM_BLOCK,
//         Tf::Bc4RSnorm => F::BC4_SNORM_BLOCK,
//         Tf::Bc5RgUnorm => F::BC5_UNORM_BLOCK,
//         Tf::Bc5RgSnorm => F::BC5_SNORM_BLOCK,
//         Tf::Bc6hRgbUfloat => F::BC6H_UFLOAT_BLOCK,
//         Tf::Bc6hRgbFloat => F::BC6H_SFLOAT_BLOCK,
//         Tf::Bc7RgbaUnorm => F::BC7_UNORM_BLOCK,
//         Tf::Bc7RgbaUnormSrgb => F::BC7_SRGB_BLOCK,
//         Tf::Etc2Rgb8Unorm => F::ETC2_R8G8B8_UNORM_BLOCK,
//         Tf::Etc2Rgb8UnormSrgb => F::ETC2_R8G8B8_SRGB_BLOCK,
//         Tf::Etc2Rgb8A1Unorm => F::ETC2_R8G8B8A1_UNORM_BLOCK,
//         Tf::Etc2Rgb8A1UnormSrgb => F::ETC2_R8G8B8A1_SRGB_BLOCK,
//         Tf::Etc2Rgba8Unorm => F::ETC2_R8G8B8A8_UNORM_BLOCK,
//         Tf::Etc2Rgba8UnormSrgb => F::ETC2_R8G8B8A8_SRGB_BLOCK,
//         Tf::EacR11Unorm => F::EAC_R11_UNORM_BLOCK,
//         Tf::EacR11Snorm => F::EAC_R11_SNORM_BLOCK,
//         Tf::EacRg11Unorm => F::EAC_R11G11_UNORM_BLOCK,
//         Tf::EacRg11Snorm => F::EAC_R11G11_SNORM_BLOCK,
//         Tf::Astc { block, channel } => match channel {
//             AstcChannel::Unorm => match block {
//                 AstcBlock::B4x4 => F::ASTC_4X4_UNORM_BLOCK,
//                 AstcBlock::B5x4 => F::ASTC_5X4_UNORM_BLOCK,
//                 AstcBlock::B5x5 => F::ASTC_5X5_UNORM_BLOCK,
//                 AstcBlock::B6x5 => F::ASTC_6X5_UNORM_BLOCK,
//                 AstcBlock::B6x6 => F::ASTC_6X6_UNORM_BLOCK,
//                 AstcBlock::B8x5 => F::ASTC_8X5_UNORM_BLOCK,
//                 AstcBlock::B8x6 => F::ASTC_8X6_UNORM_BLOCK,
//                 AstcBlock::B8x8 => F::ASTC_8X8_UNORM_BLOCK,
//                 AstcBlock::B10x5 => F::ASTC_10X5_UNORM_BLOCK,
//                 AstcBlock::B10x6 => F::ASTC_10X6_UNORM_BLOCK,
//                 AstcBlock::B10x8 => F::ASTC_10X8_UNORM_BLOCK,
//                 AstcBlock::B10x10 => F::ASTC_10X10_UNORM_BLOCK,
//                 AstcBlock::B12x10 => F::ASTC_12X10_UNORM_BLOCK,
//                 AstcBlock::B12x12 => F::ASTC_12X12_UNORM_BLOCK,
//             },
//             AstcChannel::UnormSrgb => match block {
//                 AstcBlock::B4x4 => F::ASTC_4X4_SRGB_BLOCK,
//                 AstcBlock::B5x4 => F::ASTC_5X4_SRGB_BLOCK,
//                 AstcBlock::B5x5 => F::ASTC_5X5_SRGB_BLOCK,
//                 AstcBlock::B6x5 => F::ASTC_6X5_SRGB_BLOCK,
//                 AstcBlock::B6x6 => F::ASTC_6X6_SRGB_BLOCK,
//                 AstcBlock::B8x5 => F::ASTC_8X5_SRGB_BLOCK,
//                 AstcBlock::B8x6 => F::ASTC_8X6_SRGB_BLOCK,
//                 AstcBlock::B8x8 => F::ASTC_8X8_SRGB_BLOCK,
//                 AstcBlock::B10x5 => F::ASTC_10X5_SRGB_BLOCK,
//                 AstcBlock::B10x6 => F::ASTC_10X6_SRGB_BLOCK,
//                 AstcBlock::B10x8 => F::ASTC_10X8_SRGB_BLOCK,
//                 AstcBlock::B10x10 => F::ASTC_10X10_SRGB_BLOCK,
//                 AstcBlock::B12x10 => F::ASTC_12X10_SRGB_BLOCK,
//                 AstcBlock::B12x12 => F::ASTC_12X12_SRGB_BLOCK,
//             },
//             AstcChannel::Hdr => match block {
//                 AstcBlock::B4x4 => F::ASTC_4X4_SFLOAT_BLOCK_EXT,
//                 AstcBlock::B5x4 => F::ASTC_5X4_SFLOAT_BLOCK_EXT,
//                 AstcBlock::B5x5 => F::ASTC_5X5_SFLOAT_BLOCK_EXT,
//                 AstcBlock::B6x5 => F::ASTC_6X5_SFLOAT_BLOCK_EXT,
//                 AstcBlock::B6x6 => F::ASTC_6X6_SFLOAT_BLOCK_EXT,
//                 AstcBlock::B8x5 => F::ASTC_8X5_SFLOAT_BLOCK_EXT,
//                 AstcBlock::B8x6 => F::ASTC_8X6_SFLOAT_BLOCK_EXT,
//                 AstcBlock::B8x8 => F::ASTC_8X8_SFLOAT_BLOCK_EXT,
//                 AstcBlock::B10x5 => F::ASTC_10X5_SFLOAT_BLOCK_EXT,
//                 AstcBlock::B10x6 => F::ASTC_10X6_SFLOAT_BLOCK_EXT,
//                 AstcBlock::B10x8 => F::ASTC_10X8_SFLOAT_BLOCK_EXT,
//                 AstcBlock::B10x10 => F::ASTC_10X10_SFLOAT_BLOCK_EXT,
//                 AstcBlock::B12x10 => F::ASTC_12X10_SFLOAT_BLOCK_EXT,
//                 AstcBlock::B12x12 => F::ASTC_12X12_SFLOAT_BLOCK_EXT,
//             },
//         },
//     })
// }
