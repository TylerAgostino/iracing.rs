//! Provider for sequential IBT replay.

use std::{path::Path, sync::Arc};

use crate::{
    FramePacket, IRacingSDKError, Result, VariableSchema, provider::Provider,
    reader::disk::IbtReader, types::IRacingSessionString,
};

/// A [`Provider`] that streams telemetry frames from an iRacing `.ibt` replay file.
///
/// Owns the validated variable schema and sequential replay cursor. Construction
/// always starts at frame zero, regardless of previous indexed reader operations.
pub struct IbtProvider {
    reader: IbtReader,
    schema: Arc<VariableSchema>,
    current_frame: usize,
    // tick_rate: f64,
}

impl IbtProvider {
    /// Open an `.ibt` file and validate its replay schema.
    ///
    /// The recording must remain unchanged while the provider is alive, as
    /// required by [`IbtReader::open`].
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::from_reader(IbtReader::open(path)?)
    }

    /// Create a replay provider starting at frame zero.
    ///
    /// # Errors
    /// Returns an error if variable headers cannot be read or validated against
    /// the layout's frame size, or if telemetry frames have no variable metadata.
    /// A zero-frame recording may have an empty schema.
    pub fn from_reader(reader: IbtReader) -> Result<Self> {
        let frame_size = reader.frame_size();
        let frame_count = reader.frame_count();

        let schema = match reader.variable_headers_snapshot()? {
            Some(snapshot) => VariableSchema::from_snapshot(snapshot, frame_size)?,
            None if frame_count == 0 => VariableSchema::from_headers(&[], frame_size)?,
            None => {
                return Err(IRacingSDKError::parse_error(
                    "IBT replay schema",
                    "Telemetry frames require variable-header metadata",
                ));
            }
        };

        Ok(Self {
            reader,
            schema: Arc::new(schema),
            current_frame: 0,
        })
    }

    /// Returns an ownable schema.
    pub(crate) fn shared_schema(&self) -> Arc<VariableSchema> {
        Arc::clone(&self.schema)
    }

    /// Returns the total number of telemetry frames in the recording.
    pub fn total_frames(&self) -> usize {
        self.reader.frame_count()
    }

    fn tick_for_frame(index: usize) -> Result<u32> {
        u32::try_from(index).map_err(|_| {
            IRacingSDKError::parse_error(
                "IbtProvider::next_frame",
                format!("Frame index {index} exceeds the u32 tick range"),
            )
        })
    }
}

#[async_trait::async_trait]
impl Provider for IbtProvider {
    /// Emits each frame once in index order, then returns permanent EOF.
    ///
    /// The synthetic tick is the zero-based frame index (checked against `u32`), and
    /// the session version is the recording header's update counter. Failed
    /// reads leave the replay cursor unchanged so the same frame can be retried.
    async fn next_frame(&mut self) -> Result<Option<FramePacket>> {
        if self.current_frame >= self.total_frames() {
            return Ok(None);
        }
        let tick = Self::tick_for_frame(self.current_frame)?;
        let frame_data = self.reader.frame_snapshot(self.current_frame)?;
        let packet = FramePacket::new(frame_data.into_owned(), tick, 0, self.shared_schema());
        self.current_frame += 1;
        Ok(Some(packet))
    }

    async fn session_yaml(&mut self, _version: u32) -> Result<Option<String>> {
        let Some(snapshot) = self.reader.session_info_snapshot()? else {
            return Ok(None);
        };

        Ok(Some(IRacingSessionString::try_from(snapshot)?.into()))
    }

    fn tick_rate(&self) -> f64 {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irsdk::{DiskSubHeader, Header};
    use crate::test_utils::{load_fixture_manifest, require_smallest_ibt_fixture};
    use futures::executor::block_on;
    use std::fs;
    use std::mem::{offset_of, size_of};

    #[test]
    fn file_and_owned_constructors_replay_equivalent_frames_and_schema() -> anyhow::Result<()> {
        for fixture in load_fixture_manifest()?.fixtures {
            let path = fixture.fixture_path()?;
            let bytes = fs::read(&path)?;
            let reference = IbtReader::from_bytes(bytes.clone())?;
            for mut provider in [
                IbtProvider::open(&path)?,
                IbtProvider::from_reader(IbtReader::from_bytes(bytes)?)?,
            ] {
                assert_eq!(provider.total_frames(), fixture.num_frames);
                assert_eq!(provider.schema.frame_size, fixture.frame_size);
                assert_eq!(provider.schema.variable_count(), fixture.num_vars as usize);
                assert_eq!(provider.tick_rate(), f64::from(fixture.tick_rate));
                for expected in &fixture.required_variables {
                    let actual = provider.schema.get_variable(&expected.name).unwrap();
                    assert_eq!(actual.offset, expected.offset);
                    assert_eq!(actual.count, expected.count);
                    assert_eq!(actual.units, expected.units);
                }
                for index in 0..reference.frame_count() {
                    // Session snapshots remain available between frame reads.
                    if index == 1 {
                        let yaml = block_on(provider.session_yaml(0))?.unwrap();
                        let session = crate::schema::SessionInfo::parse(&yaml)?;
                        assert!(!session.weekend_info.track_name.is_empty());
                    }
                    let packet = block_on(provider.next_frame())?.unwrap();
                    assert_eq!(packet.tick as usize, index);
                    assert_eq!(packet.session_version, fixture.session_info_update as u32);
                    assert_eq!(
                        packet.data.as_ref(),
                        reference.frame_snapshot(index)?.as_ref()
                    );
                    assert!(Arc::ptr_eq(&packet.schema, &provider.schema));
                }
                assert!(block_on(provider.next_frame())?.is_none());
                assert!(block_on(provider.next_frame())?.is_none());
            }
        }
        Ok(())
    }

    #[test]
    fn schema_validation_uses_layout_frame_size() -> anyhow::Result<()> {
        use crate::irsdk::VariableHeader;
        let mut bytes = fs::read(require_smallest_ibt_fixture()?)?;
        let reader = IbtReader::from_bytes(bytes.clone())?;
        let offset = usize::try_from(reader.header().variable_header_offset)?
            + std::mem::offset_of!(VariableHeader, offset);
        bytes[offset..offset + 4]
            .copy_from_slice(&i32::try_from(reader.frame_size())?.to_le_bytes());
        // Byte geometry remains valid; the provider owns semantic validation.
        let reader = IbtReader::from_bytes(bytes)?;
        let error = IbtProvider::from_reader(reader).err().unwrap().to_string();
        assert!(error.contains("beyond frame size"), "{error}");
        Ok(())
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn frame_index_beyond_tick_range_is_rejected() {
        assert_eq!(
            IbtProvider::tick_for_frame(u32::MAX as usize).unwrap(),
            u32::MAX
        );
        let error = IbtProvider::tick_for_frame(u32::MAX as usize + 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeds the u32 tick range"), "{error}");
    }

    #[test]
    fn invalid_session_snapshot_returns_an_error() -> anyhow::Result<()> {
        let mut bytes = fs::read(require_smallest_ibt_fixture()?)?;
        let offset = usize::try_from(
            IbtReader::from_bytes(bytes.clone())?
                .header()
                .session_info_offset,
        )?;
        bytes[offset] = 0;
        let mut provider = IbtProvider::from_reader(IbtReader::from_bytes(bytes)?)?;

        let error = block_on(provider.session_yaml(0)).unwrap_err().to_string();
        assert!(
            error.contains("YAML is empty after preprocessing"),
            "{error}"
        );
        Ok(())
    }

    #[test]
    fn zero_frame_recording_without_variable_headers_has_empty_schema() -> anyhow::Result<()> {
        let mut bytes = fs::read(require_smallest_ibt_fixture()?)?;
        let reader = IbtReader::from_bytes(bytes.clone())?;
        let metadata_end = usize::try_from(reader.header().session_info_range().unwrap().end)?;
        let frame_size = reader.frame_size();

        bytes[offset_of!(Header, variable_count)..offset_of!(Header, variable_count) + 4]
            .copy_from_slice(&0_i32.to_le_bytes());
        let record_count_offset = size_of::<Header>() + offset_of!(DiskSubHeader, record_count);
        bytes[record_count_offset..record_count_offset + 4].copy_from_slice(&0_i32.to_le_bytes());
        bytes.truncate(metadata_end);

        let mut provider = IbtProvider::from_reader(IbtReader::from_bytes(bytes)?)?;
        assert_eq!(provider.reader.frame_count(), 0);
        assert!(provider.reader.variable_headers_snapshot()?.is_none());
        assert_eq!(provider.schema.variable_count(), 0);
        assert_eq!(provider.schema.frame_size, frame_size);
        assert!(block_on(provider.next_frame())?.is_none());
        Ok(())
    }
}
