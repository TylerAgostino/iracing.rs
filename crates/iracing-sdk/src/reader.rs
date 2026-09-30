pub mod disk {
    use crate::{
        Result, SessionInfoBuffer, VariableHeadersBuffer,
        error::IRacingSDKError,
        telemetry_source::{TelemetrySource, disk::IbtSource},
        types::ByteRange,
    };

    use iracing_irsdk::{DiskSubHeader, Header, constants::IRSDK_VER};
    use memmap2::Mmap;
    use std::{borrow::Cow, fs::File, num::NonZeroUsize, path::Path};
    use zerocopy::FromBytes;

    struct IbtFrameRegion {
        range: ByteRange,
        frame_size: usize,
        frame_count: usize,
    }

    impl IbtFrameRegion {
        fn new(range: ByteRange, header: &Header) -> Result<Self> {
            let frame_size = usize::try_from(header.buffer_length)
                .ok()
                .and_then(NonZeroUsize::new)
                .ok_or_else(|| {
                    IRacingSDKError::parse_error(
                        "IbtMeta::try_from_headers",
                        format!("Frame size must be positive: {}", header.buffer_length),
                    )
                })?;

            if range.len() % frame_size.get() != 0 {
                return Err(IRacingSDKError::parse_error(
                    "IbtReader::from_source",
                    "IBT is malformed",
                ));
            }

            let frame_count = range.len() / frame_size.get();

            Ok(Self {
                range,
                frame_size: frame_size.get(),
                frame_count,
            })
        }

        fn frame(&self, index: usize) -> Result<ByteRange> {
            if index >= self.frame_count {
                return Err(IRacingSDKError::parse_error(
                    "IbtFrameRegion::frame",
                    format!(
                        "Frame index {index} out of bounds for 0..{}",
                        self.frame_count
                    ),
                ));
            }

            let relative_offset = index.checked_mul(self.frame_size).ok_or_else(|| {
                IRacingSDKError::parse_error(
                    "IbtFrameRegion::frame",
                    "Frame offset calculation overflowed",
                )
            })?;

            let start = self
                .range
                .as_range()
                .start
                .checked_add(relative_offset)
                .ok_or_else(|| {
                    IRacingSDKError::parse_error(
                        "IbtFrameRegion::frame",
                        "Frame start calculation overflowed",
                    )
                })?;

            let end = start.checked_add(self.frame_size).ok_or_else(|| {
                IRacingSDKError::parse_error(
                    "IbtFrameRegion::frame",
                    "Frame end calculation overflowed",
                )
            })?;

            Ok(ByteRange::new(start..end))
        }
    }

    struct IbtMeta {
        header: Header,
        subheader: DiskSubHeader,

        session_info: Option<ByteRange>,
        variable_headers: Option<ByteRange>,

        frames: IbtFrameRegion,

        tick_rate: usize,
        variable_count: usize,
        lap_count: usize,
    }

    impl IbtMeta {
        fn try_from_headers(
            header: Header,
            subheader: DiskSubHeader,
            source_len: usize,
        ) -> Result<Self> {
            if header.version != IRSDK_VER {
                return Err(IRacingSDKError::parse_error(
                    "IbtMeta::try_from_headers",
                    format!("Unsupported IBT header version: {}", header.version),
                ));
            }
            if header.session_info_length < 0 || header.variable_count < 0 {
                return Err(IRacingSDKError::parse_error(
                    "IbtMeta::try_from_headers",
                    "Metadata lengths and counts must be nonnegative",
                ));
            }

            let Some(session_info) = header
                .session_info_range()
                .map(ByteRange::try_from)
                .transpose()?
            else {
                return Err(IRacingSDKError::parse_error(
                    "IbtMeta::try_from_headers",
                    "Could not find session info byte range",
                ));
            };
            let session_info = (header.session_info_length > 0).then_some(session_info);

            let Some(variable_headers) = header
                .variable_headers_range()
                .map(ByteRange::try_from)
                .transpose()?
            else {
                return Err(IRacingSDKError::parse_error(
                    "IbtMeta::try_from_headers",
                    "Could not find variable headers byte range",
                ));
            };
            let variable_headers = (header.variable_count > 0).then_some(variable_headers);

            let session_range = session_info.as_ref().map(ByteRange::as_range);
            let variable_range = variable_headers.as_ref().map(ByteRange::as_range);
            for region in session_range.iter().chain(variable_range.iter()) {
                if region.start < IbtReader::PREAMBLE_SIZE {
                    return Err(IRacingSDKError::parse_error(
                        "IbtMeta::try_from_headers",
                        "Metadata region overlaps IBT preamble",
                    ));
                }
            }
            if let (Some(session_range), Some(variable_range)) = (&session_range, &variable_range) {
                if session_range.start < variable_range.end
                    && variable_range.start < session_range.end
                {
                    return Err(IRacingSDKError::parse_error(
                        "IbtMeta::try_from_headers",
                        "Metadata regions overlap",
                    ));
                }
            }

            let metadata_end = IbtReader::PREAMBLE_SIZE
                .max(session_range.as_ref().map_or(0, |range| range.end))
                .max(variable_range.as_ref().map_or(0, |range| range.end));

            if metadata_end > source_len {
                return Err(IRacingSDKError::parse_error(
                    "IbtMeta::try_from_headers",
                    "Frame data is out of range",
                ));
            }

            let frames = IbtFrameRegion::new(ByteRange::new(metadata_end..source_len), &header)?;

            if subheader.record_count > 0
                && usize::try_from(subheader.record_count).ok() != Some(frames.frame_count)
            {
                tracing::warn!(
                    "Frame count mismatch: disk header reports {} records, calculated {} frames from file size",
                    subheader.record_count,
                    frames.frame_count
                );
            }

            Ok(Self {
                header,
                subheader,
                session_info,
                variable_headers,
                frames,
                tick_rate: usize::try_from(header.tick_rate).map_err(|_| {
                    IRacingSDKError::parse_error(
                        "IbtMeta::try_from_headers",
                        format!("{} overflows usize", header.tick_rate),
                    )
                })?,
                variable_count: usize::try_from(header.variable_count).map_err(|_| {
                    IRacingSDKError::parse_error(
                        "IbtMeta::try_from_headers",
                        format!("{} overflows usize", header.variable_count),
                    )
                })?,
                lap_count: usize::try_from(subheader.lap_count).map_err(|_| {
                    IRacingSDKError::parse_error(
                        "IbtMeta::try_from_headers",
                        format!("{} overflows usize", subheader.lap_count),
                    )
                })?,
            })
        }
    }

    pub struct IbtReader {
        source: IbtSource,
        meta: IbtMeta,
    }

    impl IbtReader {
        const PREAMBLE_SIZE: usize = size_of::<Header>() + size_of::<DiskSubHeader>();

        pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
            let path = path.as_ref().to_path_buf();
            let file = File::open(&path).map_err(|source| IRacingSDKError::File {
                path: path.clone(),
                source,
            })?;

            // SAFETY: This maps a completed recording read-only. The documented
            // open contract requires the backing file to remain unchanged for the
            // reader's lifetime. Mmap owns the mapping independently of `file` and
            // unmaps it on drop; no mapped references escape this reader.
            let mapped = unsafe { Mmap::map(&file) }.map_err(|error| IRacingSDKError::File {
                path,
                source: std::io::Error::new(
                    error.kind(),
                    format!("Failed to map IBT source: {error}"),
                ),
            })?;

            Self::from_source(IbtSource::Mapped(mapped))
        }

        /// Parse owned in-memory `.ibt` data.
        pub fn from_bytes<B: Into<Vec<u8>>>(data: B) -> Result<Self> {
            Self::from_source(IbtSource::Owned(data.into()))
        }

        fn from_source(source: IbtSource) -> Result<Self> {
            let bytes = source.read_range(ByteRange::new(0..Self::PREAMBLE_SIZE))?;
            let (header, remainder) = Header::read_from_prefix(bytes.as_ref()).map_err(|_| {
                IRacingSDKError::parse_error(
                    "IbtReader::from_source",
                    "Could not parse header from source",
                )
            })?;
            let (subheader, []) = DiskSubHeader::read_from_prefix(remainder).map_err(|_| {
                IRacingSDKError::parse_error(
                    "IbtReader::from_source",
                    "Could not parse sub-header from source",
                )
            })?
            else {
                return Err(IRacingSDKError::parse_error(
                    "IbtReader::from_source",
                    "Preamble had trailing bytes",
                ));
            };

            let meta = IbtMeta::try_from_headers(header, subheader, source.len())?;

            Ok(Self { source, meta })
        }

        pub fn header(&self) -> &Header {
            &self.meta.header
        }

        pub fn subheader(&self) -> &DiskSubHeader {
            &self.meta.subheader
        }

        pub fn meta(&self) -> &IbtMeta {
            &self.meta
        }

        pub fn session_info_snapshot(&self) -> Result<Option<SessionInfoBuffer>> {
            let Some(range) = &self.meta.session_info else {
                return Ok(None);
            };
            let bytes = self.source.read_range(range.clone())?;
            let buffer = SessionInfoBuffer::from_checked_region(&bytes);

            Ok(Some(buffer))
        }

        pub fn variable_headers_snapshot(&self) -> Result<Option<VariableHeadersBuffer>> {
            let Some(range) = &self.meta.variable_headers else {
                return Ok(None);
            };
            let bytes = self.source.read_range(range.clone())?;
            let buffer = VariableHeadersBuffer::try_from_region_bytes(&bytes)?;

            Ok(Some(buffer))
        }

        pub fn frame_snapshot(&self, index: usize) -> Result<Cow<'_, [u8]>> {
            let range = self.meta.frames.frame(index)?;
            let bytes = self.source.read_range(range)?;

            Ok(bytes)
        }

        pub fn tick_rate(&self) -> usize {
            self.meta.tick_rate
        }

        pub fn frame_size(&self) -> usize {
            self.meta.frames.frame_size
        }

        pub fn frame_count(&self) -> usize {
            self.meta.frames.frame_count
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::test_utils::require_smallest_ibt_fixture;
        use anyhow::Result;
        use zerocopy::IntoBytes;

        fn fixture_bytes() -> Result<Vec<u8>> {
            Ok(std::fs::read(require_smallest_ibt_fixture()?)?)
        }

        fn rewrite_header(bytes: &mut [u8], edit: impl FnOnce(&mut Header)) -> Result<()> {
            let header_bytes = &mut bytes[..size_of::<Header>()];
            let mut header = Header::try_from_bytes(header_bytes)?;
            edit(&mut header);
            header_bytes.copy_from_slice(header.as_bytes());
            Ok(())
        }

        #[test]
        fn file_and_owned_sources_decode_the_same_preamble() -> Result<()> {
            let path = require_smallest_ibt_fixture()?;
            let mapped = IbtReader::open(&path)?;
            let owned = IbtReader::from_bytes(std::fs::read(path)?)?;

            assert!(matches!(mapped.source, IbtSource::Mapped(_)));
            assert!(matches!(owned.source, IbtSource::Owned(_)));
            assert_eq!(mapped.header().as_bytes(), owned.header().as_bytes());
            assert_eq!(mapped.subheader().as_bytes(), owned.subheader().as_bytes());
            assert!(mapped.header().buffer_length > 0);
            assert!(mapped.subheader().record_count > 0);
            Ok(())
        }

        #[test]
        fn incomplete_preamble_is_rejected() -> Result<()> {
            let bytes = fixture_bytes()?;
            for length in [0, size_of::<Header>() - 1, IbtReader::PREAMBLE_SIZE - 1] {
                assert!(IbtReader::from_bytes(bytes[..length].to_vec()).is_err());
            }
            Ok(())
        }

        #[test]
        fn unsupported_header_versions_are_rejected() -> Result<()> {
            let original = fixture_bytes()?;
            for version in [-1, 0, IRSDK_VER + 1] {
                let mut bytes = original.clone();
                rewrite_header(&mut bytes, |header| header.version = version)?;
                let error = IbtReader::from_bytes(bytes).err().unwrap().to_string();
                assert!(error.contains("Unsupported IBT header version"), "{error}");
            }
            Ok(())
        }

        #[test]
        fn zero_length_metadata_is_absent_and_ignores_its_offset() -> Result<()> {
            let mut bytes = fixture_bytes()?;
            rewrite_header(&mut bytes, |header| {
                header.session_info_length = 0;
                header.session_info_offset = i32::MAX;
                header.variable_count = 0;
                header.variable_header_offset = i32::MAX;
            })?;
            bytes.truncate(IbtReader::PREAMBLE_SIZE);

            let reader = IbtReader::from_bytes(bytes)?;
            assert!(reader.session_info_snapshot()?.is_none());
            assert!(reader.variable_headers_snapshot()?.is_none());
            assert_eq!(reader.frame_count(), 0);
            Ok(())
        }

        #[test]
        fn frames_without_variable_headers_are_rejected_by_provider() -> Result<()> {
            let mut bytes = fixture_bytes()?;
            rewrite_header(&mut bytes, |header| {
                header.variable_count = 0;
                header.variable_header_offset = i32::MAX;
            })?;

            let reader = IbtReader::from_bytes(bytes)?;
            assert!(reader.frame_count() > 0);
            assert!(reader.variable_headers_snapshot()?.is_none());
            let error = crate::providers::ibt::IbtProvider::from_reader(reader)
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("Telemetry frames require variable-header metadata"));
            Ok(())
        }

        #[test]
        fn metadata_extending_past_eof_is_rejected() -> Result<()> {
            let original = fixture_bytes()?;

            let mut variables = original.clone();
            let near_eof = i32::try_from(variables.len() - 1)?;
            rewrite_header(&mut variables, |header| {
                header.variable_header_offset = near_eof;
            })?;
            assert!(IbtReader::from_bytes(variables).is_err());

            let mut session = original;
            let near_eof = i32::try_from(session.len() - 1)?;
            rewrite_header(&mut session, |header| {
                header.session_info_offset = near_eof;
            })?;
            assert!(IbtReader::from_bytes(session).is_err());
            Ok(())
        }

        #[test]
        fn malformed_frame_geometry_is_rejected() -> Result<()> {
            let original = fixture_bytes()?;

            for frame_size in [0, -1] {
                let mut invalid_size = original.clone();
                rewrite_header(&mut invalid_size, |header| {
                    header.buffer_length = frame_size
                })?;
                let error = IbtReader::from_bytes(invalid_size)
                    .err()
                    .unwrap()
                    .to_string();
                assert!(error.contains("Frame size must be positive"), "{error}");
            }

            let mut partial_frame = original;
            partial_frame.push(0);
            assert!(IbtReader::from_bytes(partial_frame).is_err());
            Ok(())
        }

        #[test]
        fn metadata_must_start_after_preamble_and_not_overlap() -> Result<()> {
            let original = fixture_bytes()?;

            let mut before_preamble = original.clone();
            rewrite_header(&mut before_preamble, |header| {
                header.variable_header_offset = 0;
            })?;
            let error = IbtReader::from_bytes(before_preamble)
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("overlaps IBT preamble"), "{error}");

            let mut session_before_preamble = original.clone();
            rewrite_header(&mut session_before_preamble, |header| {
                header.session_info_offset = 0;
            })?;
            let error = IbtReader::from_bytes(session_before_preamble)
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("overlaps IBT preamble"), "{error}");

            let mut overlapping = original;
            rewrite_header(&mut overlapping, |header| {
                header.session_info_offset = header.variable_header_offset;
            })?;
            let error = IbtReader::from_bytes(overlapping)
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("Metadata regions overlap"), "{error}");
            Ok(())
        }

        #[test]
        fn record_count_is_advisory_when_physical_frames_are_complete() -> Result<()> {
            let original = fixture_bytes()?;
            let expected = IbtReader::from_bytes(original.clone())?.frame_count();
            for count in [-1, 0, 1, i32::MAX] {
                let mut bytes = original.clone();
                let range = size_of::<Header>() + 28..size_of::<Header>() + 32;
                bytes[range].copy_from_slice(&count.to_le_bytes());
                let reader = IbtReader::from_bytes(bytes)?;
                assert_eq!(reader.frame_count(), expected, "record_count was {count}");
                assert_eq!(
                    reader.frame_snapshot(expected - 1)?.len(),
                    reader.frame_size()
                );
                assert!(reader.frame_snapshot(expected).is_err());
            }
            Ok(())
        }
    }
}

#[cfg(windows)]
pub(crate) mod live {
    use std::borrow::Cow;

    use iracing_irsdk::Header;

    use crate::{
        IRacingSDKError, Result, SessionInfoBuffer, VariableHeadersBuffer,
        telemetry_source::{TelemetrySource, live::WindowsMapping},
        types::ByteRange,
    };

    #[derive(Debug)]
    pub(crate) struct LiveReader {
        mapping: WindowsMapping,
    }

    impl LiveReader {
        pub fn try_connect() -> Result<Self> {
            let mapping = WindowsMapping::try_connect()?;

            Ok(Self { mapping })
        }

        pub fn header_snapshot(&self) -> Result<Header> {
            let bytes = self
                .mapping
                .read_range(ByteRange::new(0..size_of::<Header>()))?;

            Header::try_from_bytes(&bytes).map_err(|e| e.into())
        }

        /// Check if iRacing is connected
        pub fn is_connected(&self) -> bool {
            let Some(header) = self.header_snapshot().ok() else {
                return false;
            };

            header.status.is_connected()
        }

        pub fn session_info_snapshot(&self) -> Result<Option<SessionInfoBuffer>> {
            let header = self.header_snapshot()?;

            let Some(range) = header.session_info_range() else {
                return Ok(None);
            };

            let bytes = self.mapping.read_range(ByteRange::try_from(range)?)?;
            let buffer = SessionInfoBuffer::from_checked_region(&bytes);

            Ok(Some(buffer))
        }

        pub fn variable_headers_snapshot(&self) -> Result<Option<VariableHeadersBuffer>> {
            let header = self.header_snapshot()?;

            let Some(range) = header.variable_headers_range() else {
                return Ok(None);
            };

            let bytes = self.mapping.read_range(ByteRange::try_from(range)?)?;
            let buffer = VariableHeadersBuffer::try_from_region_bytes(&bytes)?;

            Ok(Some(buffer))
        }

        pub fn current_frame_snapshot(&self) -> Result<Cow<'_, [u8]>> {
            let header = self.header_snapshot()?;

            let Some(range) = header.current_variable_buffer_range() else {
                return Err(IRacingSDKError::Buffer {
                    context: "Could not get current variable buffer".to_string(),
                    buffer_index: None,
                    source: None,
                });
            };

            let bytes = self.mapping.read_range(ByteRange::try_from(range)?)?;

            Ok(bytes)
        }
    }
}
