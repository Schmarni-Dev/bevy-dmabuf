use zvariant::{self, OwnedFd};

/// Dmabuf Backed Texture
#[derive(Debug, serde::Serialize, serde::Deserialize, zvariant::Type)]
pub struct Dmatex {
    pub planes: Vec<DmatexPlane>,
    pub res: Resolution,
    pub format: u32,
    /// TODO: implement this, or remove it
    pub flip_y: bool,
    /// if the format has an srgb version, use that
    pub srgb: bool,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, zvariant::Type, Copy, Clone)]
pub struct Resolution {
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, zvariant::Type)]
pub struct DmatexPlane {
    pub dmabuf_fd: OwnedFd,
    pub modifier: u64,
    pub offset: u32,
    pub stride: i32,
}
