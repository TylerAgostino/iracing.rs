//! The parsed layout for an IBT file
use std::ops::Range;

use iracing_irsdk::{DiskSubHeader, Header};
use zerocopy::FromBytes;

use crate::{
    FrameRegion, FramesRegion, IRacingSDKError, Result, SessionInfoRegion, VariableHeadersRegion,
};

/// The layout of an IBT file
pub struct Layout {
    header: Header,
    disk: DiskSubHeader,
    session_info: Option<SessionInfoRegion>,
    variable_headers: Option<VariableHeadersRegion>,
    frames: FramesRegion,
}

impl Layout {
    pub const PREAMBLE_SIZE: usize =
        size_of::<iracing_irsdk::Header>() + size_of::<iracing_irsdk::DiskSubHeader>();

    pub const fn preamble_range() -> Range<usize> {
        0..Self::PREAMBLE_SIZE
    }

    /// Given a byte array the size of a preamble, attempts parse out the file structure.
    pub fn try_from_bytes(bytes: &[u8]) -> Result<Self> {
        let (header, subheader) = Header::read_from_prefix(bytes).map_err(IRacingSDKError::from)?;
        let (disk, []) =
            DiskSubHeader::read_from_prefix(subheader).map_err(IRacingSDKError::from)?
        else {
            return Err(IRacingSDKError::parse_error(
                "IbtLayout::try_from_bytes",
                "Buffer had trailing bytes",
            ));
        };

        Self::try_from_headers(header, disk)
    }

    pub fn try_from_headers(header: Header, disk: DiskSubHeader) -> Result<Self> {
        Self::try_from_parts(header, disk, None)
    }

    fn try_from_parts(
        header: Header,
        disk: DiskSubHeader,
        source_len: Option<usize>,
    ) -> Result<Self> {
        let session_info = SessionInfoRegion::try_from_header(&header)?;
        let variable_headers = VariableHeadersRegion::try_from_header(&header)?;

        let metadata_end = [
            variable_headers.map(|r| r.end()),
            session_info.map(|r| r.end()),
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(Self::PREAMBLE_SIZE);

        let buffer_length = usize::try_from(header.buffer_length).map_err(|_| {
            IRacingSDKError::parse_error(
                "ibt::Layout::try_from_headers",
                "buffer_length overflows usize",
            )
        })?;

        let variable_count = usize::try_from(header.variable_count).map_err(|_| {
            IRacingSDKError::parse_error(
                "ibt::Layout::try_from_headers",
                "variable_count overflows usize",
            )
        })?;

        let frame_region = FramesRegion::new(metadata_end, buffer_length, variable_count)?;

        if let Some(len) = source_len {
            // Frame data start and end is within bounds
            if frame_region.start() > len || frame_region.end() > len {
                return Err(IRacingSDKError::parse_error(
                    "IbtLayout::try_from_parts",
                    "Frame data is out of range",
                ));
            }
        }

        if disk.record_count > 0
            && frame_region.frame_count() > 0
            && usize::try_from(disk.record_count).ok() != Some(frame_region.frame_count())
        {
            tracing::warn!(
                "Frame count mismatch: disk header reports {} records, calculated {} frames from file size",
                disk.record_count,
                frame_region.frame_count()
            );
        }

        Ok(Self {
            header,
            disk,
            session_info,
            variable_headers,
            frames: frame_region,
        })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn disk_header(&self) -> &DiskSubHeader {
        &self.disk
    }

    pub fn frame_count(&self) -> usize {
        self.frames.frame_count()
    }

    pub fn frame_size(&self) -> usize {
        self.frames.frame_size()
    }

    pub fn session_info(&self) -> Option<&SessionInfoRegion> {
        self.session_info.as_ref()
    }

    pub fn variable_headers(&self) -> Option<&VariableHeadersRegion> {
        self.variable_headers.as_ref()
    }

    /// Returns the byte region occupied by the frame at `index`.
    ///
    /// # Errors
    ///
    /// Returns a parse error if `index` is outside the recorded frame range or
    /// if calculating the frame offset overflows `usize`.
    pub fn frame(&self, index: usize) -> Result<FrameRegion> {
        Ok(self.frames.frame(index)?)
    }
}
