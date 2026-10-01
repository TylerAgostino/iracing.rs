macro_rules! parse_header_range {
    ($length:expr, $range:expr) => {{
        let range: Option<std::ops::Range<i32>> = $range;

        if $length == 0 {
            None
        } else {
            Some(ByteRange::try_from(range.ok_or_else(|| {
                IRacingSDKError::parse_error(
                    "parse_header_range",
                    "Could not find variable headers byte range",
                )
            })?)?)
        }
    }};
}

/// Disk telemetry readers
pub mod disk {
    use crate::{
        Result, SessionInfoBuffer, VariableHeadersBuffer, error::IRacingSDKError,
        telemetry_source::disk::IbtSource, types::ByteRange,
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

            ByteRange::new(start..end)
        }
    }

    struct IbtMeta {
        header: Header,
        subheader: DiskSubHeader,

        session_info: Option<ByteRange>,
        variable_headers: Option<ByteRange>,

        frames: IbtFrameRegion,

        tick_rate: usize,
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

            let session_info =
                parse_header_range!(header.session_info_length, header.session_info_range());

            let variable_headers =
                parse_header_range!(header.variable_count, header.variable_headers_range());

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

            // Ensure the regions don't overlap
            if let (Some(session_range), Some(variable_range)) = (&session_info, &variable_headers)
            {
                if session_range.overlaps(variable_range) {
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

            let frames = IbtFrameRegion::new(ByteRange::new(metadata_end..source_len)?, &header)?;

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
            // Read the first 144 bytes to parse the header and sub-header
            let bytes = unsafe { source.slice_unchecked(0..Self::PREAMBLE_SIZE) };

            // The first 132 bytes should be the header
            let (header, remainder) = Header::read_from_prefix(bytes).map_err(|_| {
                IRacingSDKError::parse_error(
                    "IbtReader::from_source",
                    "Could not parse header from source",
                )
            })?;

            // The remaining bytes should be the disk-header
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

        /// The borrowed header
        pub fn header(&self) -> &Header {
            &self.meta.header
        }

        /// The borrowed disk header
        pub fn subheader(&self) -> &DiskSubHeader {
            &self.meta.subheader
        }

        /// The borrowed metadata section
        pub fn meta(&self) -> &IbtMeta {
            &self.meta
        }

        /// The tick rate at which the file was recorded at
        pub fn tick_rate(&self) -> usize {
            self.meta.tick_rate
        }

        /// The size of each frame
        pub fn frame_size(&self) -> usize {
            self.meta.frames.frame_size
        }

        /// The number of frames in the file
        pub fn frame_count(&self) -> usize {
            self.meta.frames.frame_count
        }

        /// Returns the session info string bytes from the file.
        pub fn session_info_snapshot(&self) -> Result<Option<SessionInfoBuffer>> {
            let Some(range) = &self.meta.session_info else {
                return Ok(None);
            };
            let bytes = unsafe { self.source.slice_unchecked(range.as_range()) };
            let buffer = SessionInfoBuffer::from_checked_region(bytes);

            Ok(Some(buffer))
        }

        /// Returns a snapshot of the variable headers region within the IBT file.
        pub fn variable_headers_snapshot(&self) -> Result<Option<VariableHeadersBuffer>> {
            let Some(range) = &self.meta.variable_headers else {
                return Ok(None);
            };
            let bytes = unsafe { self.source.slice_unchecked(range.as_range()) };
            let buffer = VariableHeadersBuffer::try_from_region_bytes(&bytes)?;

            Ok(Some(buffer))
        }

        /// Returns a borrowed slice of bytes for the given frame.
        pub fn frame_snapshot(&self, index: usize) -> Result<Cow<'_, [u8]>> {
            let range = self.meta.frames.frame(index)?;
            let bytes = unsafe { self.source.slice_unchecked(range.as_range()) };

            Ok(Cow::Borrowed(bytes))
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

/// Live telemetry readers
#[cfg(windows)]
pub mod live {
    use std::{
        mem::{align_of, offset_of},
        ops::Range,
        time::Duration,
    };

    use iracing_irsdk::{Header, VariableBuffer, constants::IRSDK_VER};

    use crate::{
        IRacingSDKError, Result, SessionInfoBuffer, VariableHeadersBuffer,
        telemetry_source::live::{LiveSource, WaitResult, read_barrier},
        types::ByteRange,
    };

    const SNAPSHOT_ATTEMPTS: usize = 3;

    /// Validate once; acquisition only uses the returned scalar offset.
    fn checked_sync_offset(offset: usize, source_len: usize) -> Result<usize> {
        let end = offset.checked_add(size_of::<i32>()).ok_or_else(|| {
            IRacingSDKError::parse_error("LiveMeta", "Synchronization offset overflow")
        })?;
        if !offset.is_multiple_of(align_of::<i32>()) || end > source_len {
            return Err(IRacingSDKError::parse_error(
                "LiveMeta",
                "Unaligned or out-of-bounds synchronization field",
            ));
        }
        Ok(offset)
    }

    fn checked_live_range(range: Range<i32>, source_len: usize) -> Result<ByteRange> {
        let range = ByteRange::try_from(range)?;
        if range.end() > source_len {
            return Err(IRacingSDKError::parse_error(
                "LiveMeta",
                "Range exceeds mapping bounds",
            ));
        }
        Ok(range)
    }

    fn validate_nonoverlapping(
        session: Option<&ByteRange>,
        variables: Option<&ByteRange>,
        buffers: &[LiveBufferMeta],
    ) -> Result<()> {
        let header = ByteRange::new(0..size_of::<Header>())?;
        let ranges: Vec<_> = std::iter::once(&header)
            .chain(session)
            .chain(variables)
            .chain(buffers.iter().map(|buffer| &buffer.range))
            .collect();
        for (index, range) in ranges.iter().enumerate() {
            if ranges[index + 1..]
                .iter()
                .any(|other| range.overlaps(other))
            {
                return Err(IRacingSDKError::parse_error(
                    "LiveMeta",
                    "Live regions overlap",
                ));
            }
        }
        Ok(())
    }

    fn read_header(source: &LiveSource) -> Result<Header> {
        let mut bytes = [0; size_of::<Header>()];
        // Read aligned words so individual geometry/status values cannot tear.
        // This is still only a layout observation, not an atomic whole header.
        for (index, word) in bytes.chunks_exact_mut(size_of::<i32>()).enumerate() {
            word.copy_from_slice(&source.read_i32(index * size_of::<i32>())?.to_le_bytes());
        }
        read_barrier();
        let status = source.read_i32(offset_of!(Header, status))?;
        if status & 1 == 0 {
            // Keep any disconnect observed at either end of the layout read.
            bytes[offset_of!(Header, status)..offset_of!(Header, status) + 4]
                .copy_from_slice(&status.to_le_bytes());
        }
        read_barrier();
        Header::try_from_bytes(&bytes).map_err(Into::into)
    }

    #[derive(Debug, Clone)]
    struct LiveBufferMeta {
        range: ByteRange,
        tick_count_offset: usize,
        tick_count_begin_offset: usize,
    }

    impl LiveBufferMeta {
        fn try_from_header(header: &Header, index: usize, source_len: usize) -> Result<Self> {
            let range = header.variable_buffer_range(index).ok_or_else(|| {
                IRacingSDKError::parse_error("LiveBufferMeta", "Invalid variable buffer range")
            })?;

            let range = checked_live_range(range, source_len)?;

            let descriptor_offset = index
                .checked_mul(size_of::<VariableBuffer>())
                .and_then(|offset| offset.checked_add(offset_of!(Header, buffers)))
                .ok_or_else(|| {
                    IRacingSDKError::parse_error("LiveBufferMeta", "Descriptor offset overflow")
                })?;
            let field_offset = |field| {
                let offset = descriptor_offset.checked_add(field).ok_or_else(|| {
                    IRacingSDKError::parse_error(
                        "LiveBufferMeta",
                        "Synchronization offset overflow",
                    )
                })?;
                checked_sync_offset(offset, source_len)
            };

            Ok(Self {
                range,
                tick_count_offset: field_offset(offset_of!(VariableBuffer, tick_count))?,
                tick_count_begin_offset: field_offset(offset_of!(
                    VariableBuffer,
                    tick_count_begin
                ))?,
            })
        }
    }

    #[derive(Debug, Clone)]
    struct LiveMeta {
        layout_key: LiveLayoutKey,

        session_info: Option<ByteRange>,
        variable_headers: Option<ByteRange>,

        buffers: Vec<LiveBufferMeta>,
        // Immutable geometry words, excluding publication counters and curBuf.
        // Both their read locations and expected values are fixed at activation.
        structural_words: Vec<(usize, i32)>,

        tick_rate: usize,
        frame_size: usize,
    }

    impl LiveMeta {
        fn try_from_header(header: &Header, source_len: usize) -> Result<Self> {
            if source_len < size_of::<Header>() {
                return Err(IRacingSDKError::parse_error(
                    "LiveMeta",
                    "Mapping is shorter than header",
                ));
            }
            if !header.is_connected() {
                return Err(IRacingSDKError::connection_failed(
                    "iRacing is not connected",
                ));
            }

            if header.version != IRSDK_VER {
                return Err(IRacingSDKError::parse_error(
                    "LiveMeta",
                    "Unsupported SDK version",
                ));
            }

            let tick_rate = usize::try_from(header.tick_rate)
                .ok()
                .filter(|rate| *rate > 0)
                .ok_or_else(|| IRacingSDKError::parse_error("LiveMeta", "Invalid tick rate"))?;

            let frame_size = usize::try_from(header.buffer_length)
                .ok()
                .filter(|size| *size > 0)
                .ok_or_else(|| IRacingSDKError::parse_error("LiveMeta", "Invalid frame size"))?;

            let buffer_count = usize::try_from(header.buffer_count)
                .map_err(|_| IRacingSDKError::parse_error("LiveMeta", "Invalid buffer count"))?;

            if !(3..=Header::MAX_BUFFERS).contains(&buffer_count) {
                return Err(IRacingSDKError::parse_error(
                    "LiveMeta",
                    "Expected 3 or 4 telemetry buffers",
                ));
            }

            if usize::from(header.current_buffer) >= buffer_count {
                return Err(IRacingSDKError::parse_error(
                    "LiveMeta",
                    "Invalid current buffer index",
                ));
            }

            if header.session_info_length < 0
                || header.session_info_offset < 0
                || header.variable_count < 0
                || header.variable_header_offset < 0
            {
                return Err(IRacingSDKError::parse_error(
                    "LiveMeta",
                    "Invalid metadata geometry",
                ));
            }

            let session_info = if header.session_info_length > 0 {
                Some(checked_live_range(
                    header.session_info_range().ok_or_else(|| {
                        IRacingSDKError::parse_error(
                            "LiveMeta",
                            "Invalid session information range",
                        )
                    })?,
                    source_len,
                )?)
            } else {
                None
            };

            let variable_headers = if header.variable_count > 0 {
                Some(checked_live_range(
                    header.variable_headers_range().ok_or_else(|| {
                        IRacingSDKError::parse_error("LiveMeta", "Invalid variable headers range")
                    })?,
                    source_len,
                )?)
            } else {
                None
            };

            let buffers = (0..buffer_count)
                .map(|index| LiveBufferMeta::try_from_header(header, index, source_len))
                .collect::<Result<Vec<_>>>()?;

            // Include the fixed header, both present metadata regions, and
            // every active telemetry buffer in the overlap check.
            validate_nonoverlapping(session_info.as_ref(), variable_headers.as_ref(), &buffers)?;

            checked_sync_offset(offset_of!(Header, status), source_len)?;
            checked_sync_offset(offset_of!(Header, session_info_update), source_len)?;
            let current_buffer_end = offset_of!(Header, current_buffer)
                .checked_add(size_of::<u8>())
                .ok_or_else(|| {
                    IRacingSDKError::parse_error("LiveMeta", "Current buffer offset overflow")
                })?;
            if current_buffer_end > source_len {
                return Err(IRacingSDKError::parse_error(
                    "LiveMeta",
                    "Current buffer exceeds mapping bounds",
                ));
            }
            let mut structural_words = vec![
                (offset_of!(Header, version), header.version),
                (offset_of!(Header, tick_rate), header.tick_rate),
                (
                    offset_of!(Header, session_info_offset),
                    header.session_info_offset,
                ),
                (
                    offset_of!(Header, session_info_length),
                    header.session_info_length,
                ),
                (
                    offset_of!(Header, variable_header_offset),
                    header.variable_header_offset,
                ),
                (offset_of!(Header, variable_count), header.variable_count),
                (offset_of!(Header, buffer_count), header.buffer_count),
                (offset_of!(Header, buffer_length), header.buffer_length),
            ];
            for (index, buffer) in header.buffers.iter().enumerate().take(buffer_count) {
                let offset = index
                    .checked_mul(size_of::<VariableBuffer>())
                    .and_then(|offset| offset.checked_add(offset_of!(Header, buffers)))
                    .and_then(|offset| {
                        offset.checked_add(offset_of!(VariableBuffer, buffer_offset))
                    })
                    .ok_or_else(|| {
                        IRacingSDKError::parse_error("LiveMeta", "Buffer offset overflow")
                    })?;
                structural_words.push((offset, buffer.buffer_offset));
            }
            for &(offset, _) in &structural_words {
                checked_sync_offset(offset, source_len)?;
            }

            Ok(Self {
                layout_key: LiveLayoutKey::from_header(header),
                session_info,
                variable_headers,
                buffers,
                structural_words,
                tick_rate,
                frame_size,
            })
        }

        fn matches_header(&self, header: &Header) -> bool {
            header.is_connected() && self.layout_key == LiveLayoutKey::from_header(header)
        }

        fn frame_size(&self) -> usize {
            self.frame_size
        }

        fn tick_rate(&self) -> usize {
            self.tick_rate
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct LiveLayoutKey {
        version: i32,
        tick_rate: i32,

        session_info_offset: i32,
        session_info_length: i32,

        variable_header_offset: i32,
        variable_count: i32,

        buffer_count: i32,
        buffer_length: i32,

        buffer_offsets: [i32; Header::MAX_BUFFERS],
    }

    impl LiveLayoutKey {
        fn from_header(header: &Header) -> Self {
            let mut buffer_offsets = [0; Header::MAX_BUFFERS];

            // Invalid counts will be rejected by LiveMeta.
            let count = usize::try_from(header.buffer_count)
                .unwrap_or(0)
                .min(Header::MAX_BUFFERS);

            for (index, offset) in buffer_offsets.iter_mut().enumerate().take(count) {
                *offset = header.buffers[index].buffer_offset;
            }

            Self {
                version: header.version,
                tick_rate: header.tick_rate,

                session_info_offset: header.session_info_offset,
                session_info_length: header.session_info_length,

                variable_header_offset: header.variable_header_offset,
                variable_count: header.variable_count,

                buffer_count: header.buffer_count,
                buffer_length: header.buffer_length,

                buffer_offsets,
            }
        }
    }

    /// Session absence and contention require different provider retry policies.
    #[derive(Debug)]
    pub enum LiveSessionRead {
        Snapshot(LiveSessionSnapshot),
        Absent,
        Contended,
    }

    #[derive(Debug)]
    pub struct LiveFrameSnapshot {
        pub bytes: Vec<u8>,
        pub tick: u32,
        /// Separately observed session revision; not atomic with frame data.
        pub session_version: u32,
    }

    #[derive(Debug)]
    pub struct LiveSessionSnapshot {
        pub buffer: SessionInfoBuffer,
        pub revision: u32,
    }

    #[derive(Debug, Clone)]
    enum Lifecycle {
        Active,
        Disconnected,
        Invalidated {
            cause: std::sync::Arc<IRacingSDKError>,
        },
    }

    /// Acquires owned snapshots using geometry validated at activation. Like
    /// the SDK, acquisition relies on finalized geometry staying fixed while
    /// connected. Observed structural changes or disconnects retire this reader.
    /// A disconnect and reconnect with identical geometry missed between
    /// observations cannot be detected. Schema ownership belongs to the provider.
    #[derive(Debug)]
    pub struct LiveReader {
        source: LiveSource,
        meta: LiveMeta,
        last_tick: Option<i32>,
        lifecycle: Lifecycle,
    }

    impl LiveReader {
        /// Attempt to connect to the source
        pub fn try_connect() -> Result<Self> {
            Self::from_source(LiveSource::try_connect()?)
        }

        /// Indicates if the reader is connected
        pub fn is_connected(&self) -> Result<bool> {
            let bit = unsafe { self.source.read_i32_unchecked(offset_of!(Header, status))? };

            Ok(bit != 0)
        }

        // Opening a source does not require a connected session. Reader
        // activation does; the provider must choose its startup policy.
        pub(crate) fn from_source(source: LiveSource) -> Result<Self> {
            for _ in 0..SNAPSHOT_ATTEMPTS {
                let header = read_header(&source)?;
                let meta = LiveMeta::try_from_header(&header, source.len())?;
                let after = read_header(&source)?;
                if !after.is_connected() {
                    return Err(IRacingSDKError::connection_failed(
                        "iRacing disconnected during activation",
                    ));
                }
                // Revalidate even a changing candidate before accepting it.
                LiveMeta::try_from_header(&after, source.len())?;
                if meta.matches_header(&after) {
                    return Ok(Self {
                        source,
                        meta,
                        last_tick: None,
                        lifecycle: Lifecycle::Active,
                    });
                }
            }
            Err(IRacingSDKError::connection_failed(
                "Live layout remained unstable during activation",
            ))
        }

        fn ensure_active(&self) -> Result<()> {
            match &self.lifecycle {
                Lifecycle::Active => Ok(()),
                Lifecycle::Disconnected => Err(IRacingSDKError::LiveDisconnected),
                Lifecycle::Invalidated { cause } => Err(IRacingSDKError::LiveInvalidated {
                    cause: std::sync::Arc::clone(cause),
                }),
            }
        }

        fn invalidate(&mut self, error: IRacingSDKError) -> IRacingSDKError {
            let cause = std::sync::Arc::new(error);
            self.lifecycle = Lifecycle::Invalidated {
                cause: std::sync::Arc::clone(&cause),
            };
            IRacingSDKError::LiveInvalidated { cause }
        }

        fn check_connected(&mut self) -> Result<()> {
            self.ensure_active()?;
            // SAFETY: Activation validated the complete fixed header in this
            // retained source. status is an aligned i32 within that header.
            let status = match unsafe { self.source.read_i32_unchecked(offset_of!(Header, status)) }
            {
                Ok(status) => status,
                Err(error) => return Err(self.invalidate(error)),
            };
            read_barrier();
            if status & 1 == 0 {
                self.lifecycle = Lifecycle::Disconnected;
                return Err(IRacingSDKError::LiveDisconnected);
            }
            for &(offset, expected) in &self.meta.structural_words {
                // SAFETY: Activation checked each immutable geometry word's
                // bounds and alignment in this retained source. Comparing its
                // value detects invalidation without revalidating byte geometry.
                let actual = match unsafe { self.source.read_i32_unchecked(offset) } {
                    Ok(value) => value,
                    Err(error) => return Err(self.invalidate(error)),
                };
                if actual != expected {
                    return Err(self.invalidate(IRacingSDKError::parse_error(
                        "LiveReader",
                        "Live layout changed after activation",
                    )));
                }
            }
            read_barrier();
            Ok(())
        }

        fn session_revision(&self) -> Result<i32> {
            // SAFETY: Activation validated the fixed header. This aligned word
            // remains within our retained mapping for the reader's lifetime.
            unsafe {
                self.source
                    .read_i32_unchecked(offset_of!(Header, session_info_update))
            }
        }

        /// Copies fixed variable metadata; `None` means no advertised headers.
        pub fn variable_headers_snapshot(&mut self) -> Result<Option<VariableHeadersBuffer>> {
            self.check_connected()?;
            let Some(range) = &self.meta.variable_headers else {
                return Ok(None);
            };
            let mut bytes = vec![0; range.len()];
            // SAFETY: Activation checked this range in this source. Owned
            // storage has exactly its length and is disjoint from the mapping.
            unsafe {
                self.source.read_range_into_unchecked(range, &mut bytes)?;
            }
            read_barrier();
            self.check_connected()?;
            // The SDK fixes variable definitions after header finalization. The
            // exact count comes from the validated range; decoding uses zerocopy.
            Ok(Some(VariableHeadersBuffer::try_from_region_bytes(&bytes)?))
        }

        /// Copies current YAML, distinguishing missing geometry from exhausted
        /// revision retries. The observed revision is not a historical lookup key.
        pub fn session_info_snapshot(&mut self) -> Result<LiveSessionRead> {
            self.check_connected()?;
            let Some(range) = self.meta.session_info.clone() else {
                return Ok(LiveSessionRead::Absent);
            };
            let mut bytes = vec![0; range.len()];
            for attempt in 0..SNAPSHOT_ATTEMPTS {
                if attempt > 0 {
                    self.check_connected()?;
                }
                let revision = self.session_revision()?;
                read_barrier();
                // SAFETY: The fixed-capacity session range was validated at
                // activation. bytes has its exact length and private ownership.
                unsafe {
                    self.source.read_range_into_unchecked(&range, &mut bytes)?;
                }
                read_barrier();
                let after = self.session_revision()?;
                self.check_connected()?;
                if revision == after {
                    // This after-write counter is observational, not a seqlock:
                    // a writer which has started but not incremented is invisible.
                    return Ok(LiveSessionRead::Snapshot(LiveSessionSnapshot {
                        buffer: SessionInfoBuffer::from_owned_checked_region(bytes),
                        revision: revision as u32,
                    }));
                }
            }
            Ok(LiveSessionRead::Contended)
        }

        /// Copies a new published frame. `None` means unchanged/unpublished tick
        /// or exhausted copy retries, never EOF. Failed attempts retain last_tick.
        pub fn frame_snapshot(&mut self) -> Result<Option<LiveFrameSnapshot>> {
            // Allocate only for a new tick and reuse storage on torn-copy retries.
            // The accepted snapshot owns raw bytes; interpretation happens later.
            let mut bytes = Vec::new();
            for _ in 0..SNAPSHOT_ATTEMPTS {
                self.check_connected()?;
                // SAFETY: curBuf is a byte in the fixed header validated at
                // activation; the source retains the same mapped view.
                let index = usize::from(unsafe {
                    self.source
                        .read_u8_unchecked(offset_of!(Header, current_buffer))?
                });
                let Some(buffer) = self.meta.buffers.get(index) else {
                    return Err(self.invalidate(IRacingSDKError::parse_error(
                        "LiveReader",
                        "Invalid current buffer index",
                    )));
                };
                // SAFETY: Active descriptor words were validated with the fixed
                // header at activation. This cached offset is aligned and bounded.
                let tick = unsafe { self.source.read_i32_unchecked(buffer.tick_count_offset)? };
                if tick == -1 || self.last_tick == Some(tick) {
                    self.check_connected()?;
                    return Ok(None);
                }
                read_barrier();
                if bytes.is_empty() {
                    bytes.resize(self.meta.frame_size, 0);
                }
                // SAFETY: Activation checked this frame range in this source.
                // bytes is private storage of exactly the validated frame size.
                unsafe {
                    self.source
                        .copy_unchecked(buffer.range.start(), &mut bytes)?;
                }
                read_barrier();
                // SAFETY: Like tick_count, this word is aligned and within the
                // fixed descriptor array validated during activation.
                let begin = unsafe {
                    self.source
                        .read_i32_unchecked(buffer.tick_count_begin_offset)?
                };
                self.check_connected()?;
                if tick == begin {
                    let session_version = self.session_revision()? as u32;
                    self.last_tick = Some(tick);
                    return Ok(Some(LiveFrameSnapshot {
                        bytes,
                        tick: tick as u32,
                        session_version,
                    }));
                }
            }
            Ok(None)
        }

        pub async fn wait_for_update_async(&mut self, timeout: Duration) -> Result<WaitResult> {
            self.check_connected()?;
            self.source.wait_for_update_async(timeout).await
        }

        pub fn tick_rate(&self) -> usize {
            self.meta.tick_rate()
        }
        pub fn frame_size(&self) -> usize {
            self.meta.frame_size()
        }
    }

    #[cfg(test)]
    pub(crate) mod tests {
        use super::*;
        use crate::telemetry_source::live::test_source::{Read, TestSource};
        use iracing_irsdk::{StatusField, VariableHeader, VariableType};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering},
        };
        use zerocopy::IntoBytes;

        // SessionTime and the frame size come from live-variable-schema.yml.
        // Keep that capture's layout; other variables are unnecessary here.
        pub(crate) const FRAME_SIZE: usize = 8587;
        pub(crate) const FRAME_START: usize = 512;
        pub(crate) const SESSION_START: usize = 112;
        pub(crate) const SESSION_END: usize = 240;
        const VARIABLES_START: usize = 256;
        const SOURCE_LEN: usize = FRAME_START + 4 * FRAME_SIZE;

        fn header() -> Header {
            Header::new(
                IRSDK_VER,
                StatusField::CONNECTED,
                60,
                7,
                128,
                112,
                1,
                VARIABLES_START as i32,
                3,
                FRAME_SIZE as i32,
                10,
                0,
                std::array::from_fn(|i| {
                    VariableBuffer::new(10, (FRAME_START + i * FRAME_SIZE) as i32, 10)
                }),
            )
        }

        fn bytes(header: Header) -> Vec<u8> {
            let mut bytes = vec![0; SOURCE_LEN];
            bytes[..size_of::<Header>()].copy_from_slice(header.as_bytes());
            let variable = VariableHeader::new(
                VariableType::Double,
                0,
                1,
                false,
                "SessionTime",
                "Seconds since session start",
                "s",
            )
            .unwrap();
            bytes[VARIABLES_START..VARIABLES_START + size_of::<VariableHeader>()]
                .copy_from_slice(variable.as_bytes());
            bytes[SESSION_START..SESSION_START + 12].copy_from_slice(b"WeekendInfo:");
            for i in 0..4 {
                let start = FRAME_START + i * FRAME_SIZE;
                bytes[start..start + 8].copy_from_slice(&(10.0 + i as f64).to_le_bytes());
            }
            bytes
        }

        pub(crate) fn source(hook: impl FnMut(Read, &mut [u8]) + Send + 'static) -> LiveSource {
            LiveSource::Test(TestSource::new(bytes(header()), hook))
        }

        fn put_i32(bytes: &mut [u8], offset: usize, value: i32) {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }

        fn tick_offset(index: usize) -> usize {
            offset_of!(Header, buffers) + index * size_of::<VariableBuffer>()
        }

        fn snapshot(reader: &mut LiveReader) -> LiveFrameSnapshot {
            match reader.frame_snapshot().unwrap() {
                Some(frame) => frame,
                other => panic!("Expected frame, got {other:?}"),
            }
        }

        #[test]
        fn validates_connected_geometry_bounds_counts_and_overflow() {
            let valid = header();
            assert!(LiveMeta::try_from_header(&valid, SOURCE_LEN).is_ok());
            let mut four = valid;
            four.buffer_count = 4;
            assert!(LiveMeta::try_from_header(&four, SOURCE_LEN).is_ok());
            for edit in [
                |h: &mut Header| h.version = IRSDK_VER + 1,
                |h: &mut Header| h.tick_rate = 0,
                |h: &mut Header| h.buffer_length = -1,
                |h: &mut Header| h.buffer_length = 0,
                |h: &mut Header| h.buffer_length = i32::MAX,
                |h: &mut Header| h.buffer_count = -1,
                |h: &mut Header| h.buffer_count = 0,
                |h: &mut Header| h.buffer_count = 2,
                |h: &mut Header| h.buffer_count = 5,
                |h: &mut Header| h.current_buffer = 3,
                |h: &mut Header| h.session_info_offset = -1,
                |h: &mut Header| h.session_info_length = -1,
                |h: &mut Header| h.variable_count = -1,
                |h: &mut Header| h.variable_header_offset = -1,
                |h: &mut Header| h.variable_count = i32::MAX,
                |h: &mut Header| h.session_info_offset = i32::MAX,
                |h: &mut Header| h.buffers[0].buffer_offset = i32::MAX,
                |h: &mut Header| h.buffers[0].buffer_offset = -1,
                |h: &mut Header| h.buffers[0].buffer_offset = SOURCE_LEN as i32,
                |h: &mut Header| h.status = StatusField::empty(),
            ] {
                let mut invalid = valid;
                edit(&mut invalid);
                assert!(
                    LiveMeta::try_from_header(&invalid, SOURCE_LEN).is_err(),
                    "{invalid:?}"
                );
                assert!(
                    LiveReader::from_source(LiveSource::Test(TestSource::new(
                        bytes(invalid),
                        |_, _| {},
                    )))
                    .is_err(),
                    "activation accepted {invalid:?}"
                );
            }
            assert!(LiveMeta::try_from_header(&valid, size_of::<Header>() - 1).is_err());
            assert!(checked_live_range(Range { start: 9, end: 3 }, SOURCE_LEN).is_err());
            assert!(checked_live_range(-1..3, SOURCE_LEN).is_err());
            assert!(checked_live_range(0..i32::MAX, SOURCE_LEN).is_err());
        }

        #[test]
        fn synchronization_locations_are_validated_before_use() {
            assert_eq!(checked_sync_offset(4, 8).unwrap(), 4);
            for (offset, length) in [
                (1, 8),
                (4, 7),
                (8, 8),
                (usize::MAX, usize::MAX),
                (usize::MAX - 3, usize::MAX),
            ] {
                assert!(checked_sync_offset(offset, length).is_err());
            }
            let initial = header();
            for index in 0..initial.buffer_count as usize {
                let meta = LiveBufferMeta::try_from_header(&initial, index, SOURCE_LEN).unwrap();
                assert_eq!(meta.tick_count_offset, tick_offset(index));
                assert_eq!(
                    meta.tick_count_begin_offset,
                    tick_offset(index) + offset_of!(VariableBuffer, tick_count_begin)
                );
            }
            for index in [3, Header::MAX_BUFFERS, usize::MAX] {
                assert!(LiveBufferMeta::try_from_header(&initial, index, SOURCE_LEN).is_err());
            }
            // A short source cannot establish the fixed header or any of its
            // synchronization fields. No unchecked operation is reached.
            let mut short = bytes(initial);
            short.truncate(size_of::<Header>() - 1);
            assert!(
                LiveReader::from_source(LiveSource::Test(TestSource::new(short, |_, _| {})))
                    .is_err()
            );
        }

        #[test]
        fn rejects_all_kinds_of_region_overlap() {
            for edit in [
                |h: &mut Header| h.session_info_offset = 0,
                |h: &mut Header| h.variable_header_offset = 0,
                |h: &mut Header| h.variable_header_offset = h.session_info_offset,
                |h: &mut Header| h.buffers[0].buffer_offset = 0,
                |h: &mut Header| h.buffers[0].buffer_offset = h.session_info_offset,
                |h: &mut Header| h.buffers[0].buffer_offset = h.variable_header_offset,
                |h: &mut Header| h.buffers[1].buffer_offset = h.buffers[0].buffer_offset,
            ] {
                let mut invalid = header();
                edit(&mut invalid);
                assert!(LiveMeta::try_from_header(&invalid, SOURCE_LEN).is_err());
            }
        }

        #[test]
        fn key_ignores_publication_fields_and_inactive_descriptors() {
            let original = header();
            let meta = LiveMeta::try_from_header(&original, SOURCE_LEN).unwrap();
            let mut changed = original;
            changed.status = StatusField::from_bits_retain(3);
            changed.current_buffer = 2;
            changed.current_buffer_tick_count = i32::MIN;
            changed.session_info_update = -99;
            for buffer in &mut changed.buffers {
                buffer.tick_count = 25;
                buffer.tick_count_begin = 26;
            }
            changed.buffers[3].buffer_offset = -1;
            assert!(meta.matches_header(&changed));
            changed.buffers[0].buffer_offset += 1;
            assert!(!meta.matches_header(&changed));
            // Unlike publication fields, fixed session capacity/offset are layout.
            let mut changed = original;
            changed.session_info_length -= 1;
            assert!(!meta.matches_header(&changed));
        }

        #[test]
        fn stable_frame_uses_current_buffer_and_pairs_bytes_with_its_tick() {
            let mut initial = header();
            initial.current_buffer = 1;
            initial.buffers[0].tick_count = 1000; // Must not select highest tick.
            initial.buffers[1].tick_count = 11;
            initial.buffers[1].tick_count_begin = 11;
            let mut reader = LiveReader::from_source(LiveSource::Test(TestSource::new(
                bytes(initial),
                |_, _| {},
            )))
            .unwrap();
            assert_eq!(reader.frame_size(), FRAME_SIZE);
            assert_eq!(reader.tick_rate(), 60);
            let frame = snapshot(&mut reader);
            assert_eq!(frame.tick, 11);
            assert_eq!(frame.session_version, 7);
            assert_eq!(
                f64::from_le_bytes(frame.bytes[..8].try_into().unwrap()),
                11.0
            );
            assert!(reader.frame_snapshot().unwrap().is_none());
            assert_eq!(frame.bytes.len(), FRAME_SIZE);
        }

        #[test]
        fn publication_changes_and_inactive_geometry_do_not_invalidate_layout() {
            let publish = Arc::new(AtomicBool::new(false));
            let state = Arc::clone(&publish);
            let mut reader = LiveReader::from_source(source(move |read, bytes| {
                if state.load(Ordering::Relaxed) && read == Read::I32(offset_of!(Header, status)) {
                    put_i32(bytes, offset_of!(Header, status), 3);
                    put_i32(bytes, offset_of!(Header, session_info_update), 8);
                    put_i32(bytes, offset_of!(Header, current_buffer_tick_count), 11);
                    bytes[offset_of!(Header, current_buffer)] = 2;
                    put_i32(bytes, tick_offset(2), 11);
                    put_i32(
                        bytes,
                        tick_offset(2) + offset_of!(VariableBuffer, tick_count_begin),
                        11,
                    );
                    // The fourth descriptor is not active in this layout.
                    put_i32(
                        bytes,
                        tick_offset(3) + offset_of!(VariableBuffer, buffer_offset),
                        -1,
                    );
                }
            }))
            .unwrap();
            assert_eq!(snapshot(&mut reader).tick, 10);
            publish.store(true, Ordering::Relaxed);
            let frame = snapshot(&mut reader);
            assert_eq!(frame.tick, 11);
            assert_eq!(frame.session_version, 8);
            assert_eq!(
                f64::from_le_bytes(frame.bytes[..8].try_into().unwrap()),
                12.0
            );
            assert!(reader.frame_snapshot().unwrap().is_none());
        }

        #[test]
        fn fourth_active_buffer_uses_its_prevalidated_offsets() {
            let mut initial = header();
            initial.buffer_count = 4;
            initial.current_buffer = 3;
            let mut reader = LiveReader::from_source(LiveSource::Test(TestSource::new(
                bytes(initial),
                |_, _| {},
            )))
            .unwrap();
            let frame = snapshot(&mut reader);
            assert_eq!(frame.bytes.len(), FRAME_SIZE);
            assert_eq!(
                f64::from_le_bytes(frame.bytes[..8].try_into().unwrap()),
                13.0
            );
        }

        #[test]
        fn duplicate_sentinel_reset_and_wrap_ticks() {
            let tick = Arc::new(AtomicI32::new(-1));
            let update = Arc::clone(&tick);
            let mut reader = LiveReader::from_source(source(move |read, bytes| {
                if read == Read::U8(offset_of!(Header, current_buffer)) {
                    let tick = update.load(Ordering::Relaxed);
                    put_i32(bytes, tick_offset(0), tick);
                    put_i32(
                        bytes,
                        tick_offset(0) + offset_of!(VariableBuffer, tick_count_begin),
                        tick,
                    );
                    bytes[FRAME_START..FRAME_START + 8]
                        .copy_from_slice(&f64::from(tick).to_le_bytes());
                }
            }))
            .unwrap();
            assert!(reader.frame_snapshot().unwrap().is_none());
            assert_eq!(reader.last_tick, None);
            for value in [i32::MAX, i32::MIN, -2, 0, 20, 1] {
                tick.store(value, Ordering::Relaxed);
                let frame = snapshot(&mut reader);
                assert_eq!(frame.tick, value as u32);
                assert_eq!(
                    f64::from_le_bytes(frame.bytes[..8].try_into().unwrap()),
                    f64::from(value)
                );
                assert!(reader.frame_snapshot().unwrap().is_none());
            }
        }

        #[test]
        fn torn_frame_retries_with_fresh_selection_and_does_not_consume_tick() {
            let copies = Arc::new(AtomicUsize::new(0));
            let count = Arc::clone(&copies);
            let mut reader = LiveReader::from_source(source(move |read, bytes| {
                if read == Read::Bytes(FRAME_START..FRAME_START + FRAME_SIZE)
                    && count.fetch_add(1, Ordering::Relaxed) == 0
                {
                    put_i32(
                        bytes,
                        tick_offset(0) + offset_of!(VariableBuffer, tick_count_begin),
                        99,
                    );
                    bytes[offset_of!(Header, current_buffer)] = 1;
                    put_i32(bytes, tick_offset(1), 11);
                    put_i32(
                        bytes,
                        tick_offset(1) + offset_of!(VariableBuffer, tick_count_begin),
                        11,
                    );
                }
            }))
            .unwrap();
            let frame = snapshot(&mut reader);
            assert_eq!(frame.tick, 11);
            assert_eq!(
                f64::from_le_bytes(frame.bytes[..8].try_into().unwrap()),
                11.0
            );
            assert_eq!(reader.last_tick, Some(11));
        }

        #[test]
        fn exhausted_contention_preserves_last_tick_and_can_recover() {
            let tearing = Arc::new(AtomicBool::new(true));
            let enabled = Arc::clone(&tearing);
            let copies = Arc::new(AtomicUsize::new(0));
            let count = Arc::clone(&copies);
            let mut reader = LiveReader::from_source(source(move |read, bytes| {
                if read == Read::Bytes(FRAME_START..FRAME_START + FRAME_SIZE) {
                    count.fetch_add(1, Ordering::Relaxed);
                    let begin = if enabled.load(Ordering::Relaxed) {
                        11
                    } else {
                        10
                    };
                    put_i32(
                        bytes,
                        tick_offset(0) + offset_of!(VariableBuffer, tick_count_begin),
                        begin,
                    );
                }
            }))
            .unwrap();
            assert!(reader.frame_snapshot().unwrap().is_none());
            assert_eq!(copies.load(Ordering::Relaxed), SNAPSHOT_ATTEMPTS);
            assert_eq!(reader.last_tick, None);
            tearing.store(false, Ordering::Relaxed);
            assert_eq!(snapshot(&mut reader).tick, 10);
        }

        #[test]
        fn session_snapshots_retry_and_pair_the_observed_revision() {
            let mut first = true;
            let mut reader = LiveReader::from_source(source(move |read, bytes| {
                if read == Read::Bytes(SESSION_START..SESSION_END) && first {
                    first = false;
                    put_i32(bytes, offset_of!(Header, session_info_update), 8);
                    bytes[SESSION_START..SESSION_START + 12].copy_from_slice(b"SessionInfo:");
                }
            }))
            .unwrap();
            let LiveSessionRead::Snapshot(session) = reader.session_info_snapshot().unwrap() else {
                panic!("missing session");
            };
            assert_eq!(session.revision, 8);
            assert_eq!(session.buffer.payload().decode(), "SessionInfo:");
        }

        #[test]
        fn session_contention_is_bounded() {
            let mut revision = 7;
            let mut reader = LiveReader::from_source(source(move |read, bytes| {
                if read == Read::Bytes(SESSION_START..SESSION_END) {
                    revision += 1;
                    put_i32(bytes, offset_of!(Header, session_info_update), revision);
                }
            }))
            .unwrap();
            assert!(matches!(
                reader.session_info_snapshot().unwrap(),
                LiveSessionRead::Contended
            ));
        }

        #[test]
        fn variable_snapshot_uses_exact_advertised_count_and_absence_is_explicit() {
            let mut reader = LiveReader::from_source(source(|_, _| {})).unwrap();
            let Some(variables) = reader.variable_headers_snapshot().unwrap() else {
                panic!("missing variables");
            };
            assert_eq!(variables.len(), 1);
            let mut initial = header();
            initial.variable_count = 0;
            initial.session_info_length = 0;
            let mut reader = LiveReader::from_source(LiveSource::Test(TestSource::new(
                bytes(initial),
                |_, _| {},
            )))
            .unwrap();
            assert!(reader.variable_headers_snapshot().unwrap().is_none());
            assert!(matches!(
                reader.session_info_snapshot().unwrap(),
                LiveSessionRead::Absent
            ));
        }

        #[test]
        fn post_copy_disconnect_is_terminal() {
            let mut reader = LiveReader::from_source(source(|read, bytes| {
                if read == Read::Bytes(FRAME_START..FRAME_START + FRAME_SIZE) {
                    put_i32(bytes, offset_of!(Header, status), 0);
                }
            }))
            .unwrap();
            assert!(matches!(
                reader.frame_snapshot(),
                Err(IRacingSDKError::LiveDisconnected)
            ));
            assert_eq!(reader.last_tick, None);
            assert!(matches!(
                reader.frame_snapshot(),
                Err(IRacingSDKError::LiveDisconnected)
            ));
            assert!(matches!(
                reader.session_info_snapshot(),
                Err(IRacingSDKError::LiveDisconnected)
            ));
            assert!(matches!(
                reader.variable_headers_snapshot(),
                Err(IRacingSDKError::LiveDisconnected)
            ));
        }

        #[test]
        fn metadata_copy_checks_disconnect() {
            for region in [
                SESSION_START..SESSION_END,
                VARIABLES_START..VARIABLES_START + size_of::<VariableHeader>(),
            ] {
                let session = region.start == SESSION_START;
                let mut reader = LiveReader::from_source(source(move |read, bytes| {
                    if read == Read::Bytes(region.clone()) {
                        put_i32(bytes, offset_of!(Header, status), 0);
                    }
                }))
                .unwrap();
                if session {
                    assert!(matches!(
                        reader.session_info_snapshot(),
                        Err(IRacingSDKError::LiveDisconnected)
                    ));
                } else {
                    assert!(matches!(
                        reader.variable_headers_snapshot(),
                        Err(IRacingSDKError::LiveDisconnected)
                    ));
                }
            }
        }

        #[test]
        fn frame_acquisition_uses_validated_offsets_without_source_bounds_queries() {
            use std::sync::Mutex;
            let reads = Arc::new(Mutex::new(Vec::new()));
            let observed = Arc::clone(&reads);
            let mut reader =
                LiveReader::from_source(source(move |read, _| observed.lock().unwrap().push(read)))
                    .unwrap();
            reads.lock().unwrap().clear();
            let LiveSource::Test(test_source) = &reader.source else {
                panic!("expected test source")
            };
            let len_queries = test_source.len_queries();
            let structural_reads: Vec<_> = reader
                .meta
                .structural_words
                .iter()
                .map(|&(offset, _)| Read::I32(offset))
                .collect();
            let checked_status: Vec<_> = std::iter::once(Read::I32(offset_of!(Header, status)))
                .chain(structural_reads)
                .collect();
            assert_eq!(snapshot(&mut reader).tick, 10);
            assert_eq!(
                *reads.lock().unwrap(),
                checked_status
                    .iter()
                    .cloned()
                    .chain([
                        Read::U8(offset_of!(Header, current_buffer)),
                        Read::I32(tick_offset(0)),
                        Read::Bytes(FRAME_START..FRAME_START + FRAME_SIZE),
                        Read::I32(tick_offset(0) + offset_of!(VariableBuffer, tick_count_begin)),
                    ])
                    .chain(checked_status.iter().cloned())
                    .chain([Read::I32(offset_of!(Header, session_info_update)),])
                    .collect::<Vec<_>>()
            );
            reads.lock().unwrap().clear();
            assert!(reader.frame_snapshot().unwrap().is_none());
            assert_eq!(
                *reads.lock().unwrap(),
                checked_status
                    .iter()
                    .cloned()
                    .chain([
                        Read::U8(offset_of!(Header, current_buffer)),
                        Read::I32(tick_offset(0)),
                    ])
                    .chain(checked_status.iter().cloned())
                    .collect::<Vec<_>>()
            );
            let LiveSource::Test(test_source) = &reader.source else {
                panic!("expected test source")
            };
            assert_eq!(test_source.len_queries(), len_queries);
        }

        #[test]
        fn structural_changes_retire_reader_before_reusing_validated_locations() {
            for edit in [
                |h: &mut Header| h.version += 1,
                |h: &mut Header| h.tick_rate += 1,
                |h: &mut Header| h.buffer_count = 4,
                |h: &mut Header| h.buffer_length += 1,
                |h: &mut Header| h.buffers[0].buffer_offset += 1,
                |h: &mut Header| h.buffers[2].buffer_offset = -1,
                |h: &mut Header| h.variable_header_offset += 1,
                |h: &mut Header| h.variable_count += 1,
                |h: &mut Header| h.session_info_offset += 1,
                |h: &mut Header| h.session_info_length += 1,
            ] {
                let changed = Arc::new(AtomicBool::new(false));
                let state = Arc::clone(&changed);
                let copies = Arc::new(AtomicUsize::new(0));
                let count = Arc::clone(&copies);
                let mut reader = LiveReader::from_source(source(move |read, bytes| {
                    if state.load(Ordering::Relaxed)
                        && read == Read::I32(offset_of!(Header, status))
                    {
                        let mut invalid = header();
                        edit(&mut invalid);
                        bytes[..size_of::<Header>()].copy_from_slice(invalid.as_bytes());
                    }
                    if matches!(read, Read::Bytes(_)) {
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                }))
                .unwrap();
                changed.store(true, Ordering::Relaxed);
                let IRacingSDKError::LiveInvalidated { cause } =
                    reader.frame_snapshot().unwrap_err()
                else {
                    panic!("expected structural invalidation")
                };
                assert_eq!(copies.load(Ordering::Relaxed), 0);
                assert_eq!(reader.last_tick, None);
                changed.store(false, Ordering::Relaxed);
                let IRacingSDKError::LiveInvalidated { cause: repeated } =
                    reader.frame_snapshot().unwrap_err()
                else {
                    panic!("expected terminal invalidation")
                };
                assert!(Arc::ptr_eq(&cause, &repeated));
                assert!(reader.session_info_snapshot().is_err());
                assert!(reader.variable_headers_snapshot().is_err());
            }
        }

        #[test]
        fn structural_change_during_copy_is_terminal_and_does_not_consume_tick() {
            let mut reader = LiveReader::from_source(source(|read, bytes| {
                if read == Read::Bytes(FRAME_START..FRAME_START + FRAME_SIZE) {
                    put_i32(
                        bytes,
                        offset_of!(Header, buffer_length),
                        FRAME_SIZE as i32 + 1,
                    );
                }
            }))
            .unwrap();
            assert!(matches!(
                reader.frame_snapshot(),
                Err(IRacingSDKError::LiveInvalidated { .. })
            ));
            assert_eq!(reader.last_tick, None);
            assert!(matches!(
                reader.frame_snapshot(),
                Err(IRacingSDKError::LiveInvalidated { .. })
            ));
        }

        #[test]
        fn activation_retries_but_never_accepts_continuously_changing_layout() {
            for always in [false, true] {
                let mut reads = 0;
                let result = LiveReader::from_source(source(move |read, bytes| {
                    if read == Read::I32(0) {
                        reads += 1;
                        if reads == 2 || always {
                            put_i32(bytes, offset_of!(Header, tick_rate), 60 + reads);
                        }
                    }
                }));
                assert_eq!(result.is_err(), always);
            }
        }

        #[test]
        fn a_newer_publication_after_copy_does_not_relabel_accepted_bytes() {
            let mut reader = LiveReader::from_source(source(|read, bytes| {
                if read == Read::Bytes(FRAME_START..FRAME_START + FRAME_SIZE) {
                    bytes[offset_of!(Header, current_buffer)] = 1;
                    put_i32(bytes, tick_offset(1), 11);
                    put_i32(
                        bytes,
                        tick_offset(1) + offset_of!(VariableBuffer, tick_count_begin),
                        11,
                    );
                }
            }))
            .unwrap();
            let first = snapshot(&mut reader);
            assert_eq!(first.tick, 10);
            assert_eq!(
                f64::from_le_bytes(first.bytes[..8].try_into().unwrap()),
                10.0
            );
            assert_eq!(snapshot(&mut reader).tick, 11);
        }

        #[test]
        fn observed_disconnect_stays_terminal_after_identical_reconnect() {
            let connected = Arc::new(AtomicBool::new(true));
            let state = Arc::clone(&connected);
            let mut reader = LiveReader::from_source(source(move |read, bytes| {
                if read == Read::I32(offset_of!(Header, status)) {
                    put_i32(
                        bytes,
                        offset_of!(Header, status),
                        i32::from(state.load(Ordering::Relaxed)),
                    );
                }
            }))
            .unwrap();
            connected.store(false, Ordering::Relaxed);
            assert!(matches!(
                reader.frame_snapshot(),
                Err(IRacingSDKError::LiveDisconnected)
            ));
            connected.store(true, Ordering::Relaxed);
            assert!(matches!(
                reader.frame_snapshot(),
                Err(IRacingSDKError::LiveDisconnected)
            ));
        }

        #[tokio::test]
        async fn waiting_on_a_disconnected_reader_returns_terminal_error() {
            let mut reader = LiveReader::from_source(source(|read, bytes| {
                if let Read::Bytes(_) = read {
                    put_i32(bytes, offset_of!(Header, status), 0);
                }
            }))
            .unwrap();
            assert!(matches!(
                reader.frame_snapshot(),
                Err(IRacingSDKError::LiveDisconnected)
            ));
            assert!(matches!(
                reader.wait_for_update_async(Duration::from_secs(60)).await,
                Err(IRacingSDKError::LiveDisconnected)
            ));
        }

        #[test]
        fn invalid_current_buffer_is_never_dereferenced_and_invalidates_reader() {
            let corrupt = Arc::new(AtomicBool::new(false));
            let state = Arc::clone(&corrupt);
            let mut reader = LiveReader::from_source(source(move |read, bytes| {
                if read == Read::U8(offset_of!(Header, current_buffer))
                    && state.load(Ordering::Relaxed)
                {
                    bytes[offset_of!(Header, current_buffer)] = u8::MAX;
                }
            }))
            .unwrap();
            corrupt.store(true, Ordering::Relaxed);
            let IRacingSDKError::LiveInvalidated { cause } = reader.frame_snapshot().unwrap_err()
            else {
                panic!("expected invalidation");
            };
            assert!(matches!(cause.as_ref(), IRacingSDKError::Parse { .. }));
            corrupt.store(false, Ordering::Relaxed);
            let IRacingSDKError::LiveInvalidated { cause: repeated } =
                reader.frame_snapshot().unwrap_err()
            else {
                panic!("expected retained invalidation");
            };
            assert!(Arc::ptr_eq(&cause, &repeated));
            assert!(matches!(
                reader.session_info_snapshot(),
                Err(IRacingSDKError::LiveInvalidated { .. })
            ));
            assert!(matches!(
                reader.variable_headers_snapshot(),
                Err(IRacingSDKError::LiveInvalidated { .. })
            ));
        }
    }
}
