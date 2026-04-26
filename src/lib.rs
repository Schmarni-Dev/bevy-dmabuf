use std::ffi::CStr;

pub mod wgpu_init;
// pub mod export;
pub mod dmatex;
pub mod format_mapping;
pub mod import;

pub fn required_device_extensions() -> Vec<&'static CStr> {
    vec![
        ash::ext::image_drm_format_modifier::NAME,
        ash::ext::external_memory_dma_buf::NAME,
        ash::khr::external_memory_fd::NAME,
        ash::khr::external_memory::NAME,
        ash::khr::swapchain::NAME,
    ]
}
