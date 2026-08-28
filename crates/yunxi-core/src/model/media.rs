//! Host-supplied media resolution and bounded image validation.

use crate::{Attachment, AttachmentKind};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use thiserror::Error;

pub const DEFAULT_MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
pub const DEFAULT_MAX_IMAGE_PIXELS: u64 = 4_000_000;
pub const DEFAULT_MAX_IMAGES_PER_TURN: usize = 1;

pub type ModelMediaFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ResolvedImage, ModelMediaError>> + Send + 'a>>;

/// Core receives bytes only after the host has authorized and resolved an
/// opaque attachment reference. Core never interprets a platform URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImage {
    pub bytes: Arc<[u8]>,
    pub media_type: Option<String>,
    pub width: u32,
    pub height: u32,
}

impl ResolvedImage {
    pub fn from_bytes(
        bytes: impl Into<Arc<[u8]>>,
        media_type: Option<String>,
    ) -> Result<Self, ModelMediaError> {
        let bytes = bytes.into();
        let (width, height) = detect_dimensions(&bytes, media_type.as_deref())?;
        Ok(Self {
            bytes,
            media_type,
            width,
            height,
        })
    }

    #[must_use]
    pub fn with_dimensions(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    pub fn validate(&self, max_bytes: usize, max_pixels: u64) -> Result<(), ModelMediaError> {
        if self.bytes.is_empty() {
            return Err(ModelMediaError::Empty);
        }
        if self.bytes.len() > max_bytes {
            return Err(ModelMediaError::TooManyBytes {
                length: self.bytes.len(),
                maximum: max_bytes,
            });
        }
        if self.width == 0 || self.height == 0 {
            return Err(ModelMediaError::InvalidDimensions {
                width: self.width,
                height: self.height,
            });
        }
        let pixels = u64::from(self.width) * u64::from(self.height);
        if pixels > max_pixels {
            return Err(ModelMediaError::TooManyPixels {
                pixels,
                maximum: max_pixels,
            });
        }
        if let Some(media_type) = self.media_type.as_deref()
            && !is_supported_image_type(media_type)
        {
            return Err(ModelMediaError::UnsupportedMediaType {
                media_type: media_type.to_owned(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelMediaLimits {
    pub max_bytes: usize,
    pub max_pixels: u64,
    pub max_images_per_turn: usize,
}

impl Default for ModelMediaLimits {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_IMAGE_BYTES,
            max_pixels: DEFAULT_MAX_IMAGE_PIXELS,
            max_images_per_turn: DEFAULT_MAX_IMAGES_PER_TURN,
        }
    }
}

impl ModelMediaLimits {
    pub fn validate(self) -> Result<(), ModelMediaError> {
        if self.max_bytes == 0 || self.max_bytes > 64 * 1024 * 1024 {
            return Err(ModelMediaError::InvalidLimit { field: "max_bytes" });
        }
        if self.max_pixels == 0 || self.max_pixels > 64_000_000 {
            return Err(ModelMediaError::InvalidLimit {
                field: "max_pixels",
            });
        }
        if self.max_images_per_turn == 0 || self.max_images_per_turn > 4 {
            return Err(ModelMediaError::InvalidLimit {
                field: "max_images_per_turn",
            });
        }
        Ok(())
    }
}

pub trait ModelMediaResolver: Send + Sync {
    fn resolve_image<'a>(&'a self, attachment: &'a Attachment) -> ModelMediaFuture<'a>;
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ModelMediaError {
    #[error("model media reference is not an image")]
    NotImage,
    #[error("resolved image is empty")]
    Empty,
    #[error("image is {length} bytes, above maximum {maximum}")]
    TooManyBytes { length: usize, maximum: usize },
    #[error("image has {pixels} pixels, above maximum {maximum}")]
    TooManyPixels { pixels: u64, maximum: u64 },
    #[error("image dimensions are invalid: {width}x{height}")]
    InvalidDimensions { width: u32, height: u32 },
    #[error("image media type is unsupported: {media_type}")]
    UnsupportedMediaType { media_type: String },
    #[error("turn contains {count} images, maximum {maximum}")]
    TooManyImages { count: usize, maximum: usize },
    #[error("image bytes do not match a supported format")]
    InvalidFormat,
    #[error("model media limit `{field}` is invalid")]
    InvalidLimit { field: &'static str },
    #[error("host media resolver failed: {message}")]
    ResolverFailed { message: String },
}

pub fn validate_resolved_image(
    attachment: &Attachment,
    image: &ResolvedImage,
    limits: ModelMediaLimits,
) -> Result<(), ModelMediaError> {
    if attachment.kind() != AttachmentKind::Image {
        return Err(ModelMediaError::NotImage);
    }
    limits.validate()?;
    image.validate(limits.max_bytes, limits.max_pixels)
}

fn is_supported_image_type(media_type: &str) -> bool {
    matches!(
        media_type.to_ascii_lowercase().as_str(),
        "image/png" | "image/jpeg" | "image/jpg" | "image/webp" | "image/gif"
    )
}

fn detect_dimensions(
    bytes: &[u8],
    media_type: Option<&str>,
) -> Result<(u32, u32), ModelMediaError> {
    if bytes.len() >= 24 && bytes.starts_with(b"\x89PNG\r\n\x1a\n") && &bytes[12..16] == b"IHDR" {
        return Ok((
            u32::from_be_bytes(bytes[16..20].try_into().expect("PNG width slice")),
            u32::from_be_bytes(bytes[20..24].try_into().expect("PNG height slice")),
        ));
    }
    if bytes.len() >= 10 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return Ok((
            u32::from(u16::from_le_bytes([bytes[6], bytes[7]])),
            u32::from(u16::from_le_bytes([bytes[8], bytes[9]])),
        ));
    }
    if bytes.len() >= 30
        && &bytes[..4] == b"RIFF"
        && &bytes[8..12] == b"WEBP"
        && &bytes[12..16] == b"VP8X"
    {
        let width =
            1 + u32::from(bytes[24]) + (u32::from(bytes[25]) << 8) + (u32::from(bytes[26]) << 16);
        let height =
            1 + u32::from(bytes[27]) + (u32::from(bytes[28]) << 8) + (u32::from(bytes[29]) << 16);
        return Ok((width, height));
    }
    if bytes.len() >= 4 && bytes[..2] == [0xff, 0xd8] {
        return jpeg_dimensions(bytes).ok_or(ModelMediaError::InvalidFormat);
    }
    if media_type.is_some_and(is_supported_image_type) {
        // A host may have a decoder that can provide dimensions later. The
        // explicit constructor still requires dimensions before inference.
        return Err(ModelMediaError::InvalidFormat);
    }
    Err(ModelMediaError::InvalidFormat)
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut index = 2;
    while index + 9 < bytes.len() {
        if bytes[index] != 0xff {
            index += 1;
            continue;
        }
        while index < bytes.len() && bytes[index] == 0xff {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        let marker = bytes[index];
        index += 1;
        if matches!(marker, 0xd8 | 0xd9 | 0x01) || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        if index + 2 > bytes.len() {
            break;
        }
        let segment_length = usize::from(u16::from_be_bytes([bytes[index], bytes[index + 1]]));
        if segment_length < 2 || index + segment_length > bytes.len() {
            break;
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf)
            && segment_length >= 7
        {
            let height = u32::from(u16::from_be_bytes([bytes[index + 3], bytes[index + 4]]));
            let width = u32::from(u16::from_be_bytes([bytes[index + 5], bytes[index + 6]]));
            return Some((width, height));
        }
        index += segment_length;
    }
    None
}
