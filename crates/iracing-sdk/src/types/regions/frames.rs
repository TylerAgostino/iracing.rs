use crate::{IRacingSDKError, Result, types::regions::bytes::UncheckedByteRegion};
use std::num::NonZeroUsize;

use super::{ByteRegion, FrameRegion};

/// Contiguous byte region containing zero or more fixed-size telemetry frames.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct FramesRegion {
    region: ByteRegion,
    frame_size: NonZeroUsize,
    frame_count: usize,
}

impl FramesRegion {
    /// Creates a region containing fixed-size telemetry frames.
    ///
    /// # Errors
    ///
    /// Returns a parse error if `frame_size` is zero.
    pub fn new(offset: usize, frame_size: usize, count: usize) -> Result<Self> {
        // Ensure the frame size is greater than 0
        let frame_size = NonZeroUsize::new(frame_size).ok_or_else(|| {
            IRacingSDKError::parse_error(
                "FramesRegion::new",
                "Frame size must be greater than zero",
            )
        })?;

        let region_len = frame_size.get().checked_mul(count).ok_or_else(|| {
            IRacingSDKError::parse_error(
                "FramesRegion::new",
                format!(
                    "Frame size ({}) multiplied by count ({}) overflowed usize",
                    frame_size, count
                ),
            )
        })?;

        let region = UncheckedByteRegion::new(offset, region_len)?;

        Ok(Self {
            region,
            frame_size,
            frame_count: count,
        })
    }

    /// Returns the complete byte region containing the frames.
    pub fn as_region(&self) -> ByteRegion {
        self.region
    }

    /// Returns the source-relative offset of the first frame byte.
    pub fn start(&self) -> usize {
        self.region.offset()
    }

    /// Returns the exclusive source-relative end offset of the frame data.
    pub fn end(&self) -> usize {
        self.region.end()
    }

    /// Returns the size of one frame in bytes.
    pub fn frame_size(&self) -> usize {
        self.frame_size.get()
    }

    /// Returns the number of complete frames in the region.
    pub fn frame_count(&self) -> usize {
        self.frame_count
    }

    /// Returns the total length of the frame region in bytes.
    pub fn len(&self) -> usize {
        self.region.len()
    }

    /// Returns whether the region contains no frames.
    pub fn is_empty(&self) -> bool {
        self.frame_count == 0
    }

    /// Returns a `FrameRegion` derived from the supplied index.
    ///
    /// # Errors
    ///
    /// Returns a parse error if the index is greater than or equal to the
    /// number of frames within the region, if the relative offset of the
    /// frame index overflows `usize`, or if adding the offset to the region
    /// offset overflows `usize`.
    pub fn frame(&self, index: usize) -> Result<FrameRegion> {
        if index >= self.frame_count {
            return Err(IRacingSDKError::parse_error(
                "FramesRegion::frame",
                format!(
                    "Frame index {index} out of bounds for 0..{}",
                    self.frame_count
                ),
            ));
        }

        let relative_offset = index.checked_mul(self.frame_size.get()).ok_or_else(|| {
            IRacingSDKError::parse_error(
                "FramesRegion::frame",
                "Frame offset calculation overflowed",
            )
        })?;

        let offset = self
            .region
            .offset()
            .checked_add(relative_offset)
            .ok_or_else(|| {
                IRacingSDKError::parse_error(
                    "FramesRegion::frame",
                    "Frame offset calculation overflowed",
                )
            })?;

        FrameRegion::new(offset, self.frame_size.get())
    }
}
