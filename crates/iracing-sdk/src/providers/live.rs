//! Live telemetry provider for Windows

use std::{sync::Arc, time::Duration};

use crate::{
    FramePacket, IRacingSDKError, Result, SchemaProvider, VariableSchema,
    provider::Provider,
    reader::live::{LiveReader, LiveSessionRead},
    types::IRacingSessionString,
};

const SESSION_SNAPSHOT_ATTEMPTS: usize = 3;

/// A [`Provider`] that streams owned telemetry snapshots from one live session.
///
/// An observed disconnect or invalidation permanently retires this provider.
/// Recreate the higher-level connection and subscriptions to validate the new
/// session's schema and source frequency.
#[derive(Debug)]
pub struct LiveProvider {
    reader: LiveReader,
    schema: Arc<VariableSchema>,
    poll_interval: Duration,
}

impl LiveProvider {
    /// Opens a connection to iRacing live telemetry and constructs a `LiveProvider`.
    ///
    /// Requires an active session and returns errors immediately. Waits up to
    /// 500 milliseconds for telemetry update events between acquisition attempts.
    pub fn new() -> Result<Self> {
        Self::from_parts(LiveReader::try_connect()?, Duration::from_millis(500))
    }

    fn from_parts(mut reader: LiveReader, poll_interval: Duration) -> Result<Self> {
        let frame_size = reader.frame_size();
        let buffer = reader.variable_headers_snapshot()?.ok_or_else(|| {
            IRacingSDKError::parse_error(
                "LiveProvider::from_parts",
                "No variable headers found in live connection",
            )
        })?;
        let schema = VariableSchema::from_snapshot(buffer, frame_size)?;
        Ok(Self {
            reader,
            schema: Arc::new(schema),
            poll_interval,
        })
    }

    /// Returns an ownable schema.
    pub(crate) fn shared_schema(&self) -> Arc<VariableSchema> {
        Arc::clone(&self.schema)
    }

    async fn next_frame_impl(&mut self) -> Result<Option<FramePacket>> {
        loop {
            if let Some(frame) = self.reader.frame_snapshot()? {
                return Ok(Some(FramePacket::new(
                    frame.bytes,
                    frame.tick,
                    frame.session_version,
                    self.shared_schema(),
                )));
            }
            // No new tick and exhausted copy retries both mean wait and retry,
            // never EOF. Signals may also represent a session-only update.
            self.reader
                .wait_for_update_async(self.poll_interval)
                .await?;
        }
    }

    async fn session_yaml_impl(&mut self) -> Result<Option<String>> {
        for _ in 0..SESSION_SNAPSHOT_ATTEMPTS {
            match self.reader.session_info_snapshot()? {
                LiveSessionRead::Snapshot(snapshot) => {
                    tracing::trace!(revision = snapshot.revision, "Captured live session YAML");
                    return Ok(Some(
                        IRacingSessionString::try_from(snapshot.buffer)?.into(),
                    ));
                }
                LiveSessionRead::Absent => return Ok(None),
                LiveSessionRead::Contended => tokio::task::yield_now().await,
            }
        }
        // The session policy fetches once per observed version. Retry contention
        // within that fetch and report exhausted retries as failure, not absence.
        Err(IRacingSDKError::buffer_operation_error(
            "Live session snapshot remained contended",
            None,
        ))
    }
}

impl SchemaProvider for LiveProvider {
    fn schema(&self) -> &VariableSchema {
        self.schema.as_ref()
    }
}

#[async_trait::async_trait]
impl Provider for LiveProvider {
    async fn next_frame(&mut self) -> Result<Option<FramePacket>> {
        self.next_frame_impl().await
    }

    async fn session_yaml(&mut self, _version: u32) -> Result<Option<String>> {
        self.session_yaml_impl().await
    }

    fn tick_rate(&self) -> f64 {
        self.reader.tick_rate() as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IRacingSDKError;

    use crate::{
        irsdk::{Header, VariableBuffer},
        reader::live::tests::{FRAME_SIZE, FRAME_START, SESSION_END, SESSION_START, source},
        telemetry_source::live::test_source::Read,
    };
    use std::{
        mem::{offset_of, size_of},
        sync::atomic::{AtomicUsize, Ordering},
    };

    fn provider(hook: impl FnMut(Read, &mut [u8]) + Send + 'static) -> LiveProvider {
        LiveProvider::from_parts(
            LiveReader::from_source(source(hook)).unwrap(),
            Duration::from_millis(1),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn frame_packet_uses_the_accepted_snapshot_metadata() {
        let mut provider = provider(|read, bytes| {
            if read == Read::Bytes(FRAME_START..FRAME_START + FRAME_SIZE) {
                bytes[offset_of!(Header, current_buffer)] = 1;
                let tick = offset_of!(Header, buffers) + size_of::<VariableBuffer>();
                bytes[tick..tick + 4].copy_from_slice(&11i32.to_le_bytes());
            }
        });
        let packet = provider.next_frame().await.unwrap().unwrap();
        assert_eq!(packet.tick, 10);
        assert_eq!(packet.session_version, 7);
        assert_eq!(
            f64::from_le_bytes(packet.data[..8].try_into().unwrap()),
            10.0
        );
        assert_eq!(provider.tick_rate(), 60.0);
    }

    #[tokio::test]
    async fn session_fetch_recovers_after_reader_retry_budget() {
        let copies = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&copies);
        let mut provider = provider(move |read, bytes| {
            if read == Read::Bytes(SESSION_START..SESSION_END) {
                let count = observed.fetch_add(1, Ordering::Relaxed);
                if count < 3 {
                    let offset = offset_of!(Header, session_info_update);
                    bytes[offset..offset + 4].copy_from_slice(&(8 + count as i32).to_le_bytes());
                }
            }
        });
        assert!(provider.session_yaml(7).await.unwrap().is_some());
        assert_eq!(copies.load(Ordering::Relaxed), 4);
    }

    #[tokio::test]
    async fn session_contention_is_a_bounded_error_not_absence() {
        let copies = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&copies);
        let mut provider = provider(move |read, bytes| {
            if read == Read::Bytes(SESSION_START..SESSION_END) {
                let count = observed.fetch_add(1, Ordering::Relaxed);
                let offset = offset_of!(Header, session_info_update);
                bytes[offset..offset + 4].copy_from_slice(&(8 + count as i32).to_le_bytes());
            }
        });
        assert!(matches!(
            provider.session_yaml(7).await,
            Err(IRacingSDKError::Buffer { .. })
        ));
        assert_eq!(copies.load(Ordering::Relaxed), 9);
    }

    #[tokio::test]
    async fn disconnected_provider_returns_error_and_never_eof() {
        let mut provider = provider(|read, bytes| {
            if read == Read::Bytes(FRAME_START..FRAME_START + FRAME_SIZE) {
                let offset = offset_of!(Header, status);
                bytes[offset..offset + 4].copy_from_slice(&0i32.to_le_bytes());
            }
        });
        assert!(matches!(
            provider.next_frame().await,
            Err(IRacingSDKError::LiveDisconnected)
        ));
        assert!(matches!(
            provider.session_yaml(7).await,
            Err(IRacingSDKError::LiveDisconnected)
        ));
        assert!(matches!(
            provider.next_frame().await,
            Err(IRacingSDKError::LiveDisconnected)
        ));
    }

    #[tokio::test]
    async fn frame_contention_waits_and_recovers_without_eof() {
        let mut copies = 0;
        let mut provider = provider(move |read, bytes| {
            if read == Read::Bytes(FRAME_START..FRAME_START + FRAME_SIZE) {
                copies += 1;
                let offset =
                    offset_of!(Header, buffers) + offset_of!(VariableBuffer, tick_count_begin);
                let begin: i32 = if copies <= 3 { 11 } else { 10 };
                bytes[offset..offset + 4].copy_from_slice(&begin.to_le_bytes());
            }
        });
        assert_eq!(provider.next_frame().await.unwrap().unwrap().tick, 10);
    }

    #[test]
    fn missing_variable_metadata_is_rejected_at_construction() {
        let reader = LiveReader::from_source(source(|_, bytes| {
            let offset = offset_of!(Header, variable_count);
            bytes[offset..offset + 4].copy_from_slice(&0i32.to_le_bytes());
        }))
        .unwrap();
        assert!(matches!(
            LiveProvider::from_parts(reader, Duration::from_millis(1)),
            Err(IRacingSDKError::Parse { .. })
        ));
    }

    #[tokio::test]
    async fn missing_session_region_returns_absence() {
        let mut provider = provider(|_, bytes| {
            let offset = offset_of!(Header, session_info_length);
            bytes[offset..offset + 4].copy_from_slice(&0i32.to_le_bytes());
        });
        assert!(provider.session_yaml(7).await.unwrap().is_none());
    }
    #[tokio::test]
    async fn unchanged_tick_waits_for_a_new_publication() {
        let mut selections = 0;
        let mut provider = provider(move |read, bytes| {
            if read == Read::U8(offset_of!(Header, current_buffer)) {
                selections += 1;
                if selections == 3 {
                    let offset = offset_of!(Header, buffers);
                    bytes[offset..offset + 4].copy_from_slice(&11i32.to_le_bytes());
                    let begin = offset + offset_of!(VariableBuffer, tick_count_begin);
                    bytes[begin..begin + 4].copy_from_slice(&11i32.to_le_bytes());
                }
            }
        });
        assert_eq!(provider.next_frame().await.unwrap().unwrap().tick, 10);
        assert_eq!(provider.next_frame().await.unwrap().unwrap().tick, 11);
    }
}
