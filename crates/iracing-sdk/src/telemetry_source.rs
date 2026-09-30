use crate::{IRacingSDKError, Result, types::ByteRange};
use std::{borrow::Cow, ops::Range};

pub(crate) trait TelemetrySource {
    fn len(&self) -> usize;

    fn read_range_into(&self, range: ByteRange, destination: &mut [u8]) -> Result<()>;

    fn read_range(&self, range: ByteRange) -> Result<Cow<'_, [u8]>> {
        let mut bytes = vec![0; range.len()];

        self.read_range_into(range, &mut bytes)?;

        Ok(Cow::Owned(bytes))
    }
}

fn validate_range(
    range: ByteRange,
    source_len: usize,
    destination_len: usize,
) -> Result<Range<usize>> {
    let range = range.as_range();

    let Some(len) = range.end.checked_sub(range.start) else {
        return Err(IRacingSDKError::parse_error(
            "TelemetrySource",
            "invalid byte range",
        ));
    };

    if range.end > source_len {
        return Err(IRacingSDKError::parse_error(
            "TelemetrySource",
            "range exceeds source bounds",
        ));
    }

    if len != destination_len {
        return Err(IRacingSDKError::parse_error(
            "TelemetrySource",
            "destination length must match range length",
        ));
    }

    Ok(range)
}

pub(crate) mod disk {
    use std::borrow::Cow;

    use super::{TelemetrySource, validate_range};
    use crate::{IRacingSDKError, Result, types::ByteRange};

    use memmap2::Mmap;
    use zerocopy::IntoBytes;

    pub(crate) enum IbtSource {
        Mapped(Mmap),
        Owned(Vec<u8>),
    }

    impl IbtSource {
        fn as_bytes(&self) -> &[u8] {
            match self {
                Self::Mapped(source) => source.as_bytes(),
                Self::Owned(source) => source.as_bytes(),
            }
        }

        fn get(&self, range: ByteRange) -> Option<&[u8]> {
            self.as_bytes().get(range.as_range())
        }
    }

    impl TelemetrySource for IbtSource {
        fn len(&self) -> usize {
            match self {
                Self::Mapped(source) => source.len(),
                Self::Owned(source) => source.len(),
            }
        }

        fn read_range(&self, range: ByteRange) -> Result<Cow<'_, [u8]>> {
            let bytes = self.get(range).ok_or_else(|| {
                IRacingSDKError::parse_error("IbtSource::read_range", "range exceeds source bounds")
            })?;

            Ok(Cow::Borrowed(bytes))
        }

        fn read_range_into(&self, range: ByteRange, destination: &mut [u8]) -> Result<()> {
            let range = validate_range(range, self.len(), destination.len())?;

            destination.copy_from_slice(&self.as_bytes()[range]);

            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use anyhow::Result;

        #[test]
        fn reads_borrowed_bytes_from_the_requested_range() {
            let source = IbtSource::Owned(vec![10, 20, 30, 40]);
            assert_eq!(source.len(), 4);

            let bytes = source.read_range(ByteRange::new(1..3)).unwrap();
            assert!(matches!(bytes, Cow::Borrowed(_)));
            assert_eq!(bytes.as_ref(), &[20, 30]);
            assert_eq!(bytes.as_ptr(), source.as_bytes()[1..3].as_ptr());
            assert!(source.read_range(ByteRange::new(3..5)).is_err());
        }

        #[test]
        fn mapped_source_reads_match_file_bytes() -> Result<()> {
            let path = crate::test_utils::require_smallest_ibt_fixture()?;
            let file = std::fs::File::open(&path)?;
            let expected = std::fs::read(&path)?;
            // SAFETY: The generated fixture remains unchanged while this read-only
            // mapping and its borrowed range are alive.
            let source = IbtSource::Mapped(unsafe { Mmap::map(&file)? });
            let range = ByteRange::new(1..9);

            assert_eq!(source.len(), expected.len());
            assert_eq!(source.read_range(range)?.as_ref(), &expected[1..9]);
            Ok(())
        }

        #[test]
        fn copies_exact_range_without_touching_other_source_bytes() {
            let source = IbtSource::Owned(vec![10, 20, 30, 40]);
            let mut destination = [0; 2];
            source
                .read_range_into(ByteRange::new(1..3), &mut destination)
                .unwrap();
            assert_eq!(destination, [20, 30]);
            assert_eq!(source.as_bytes(), &[10, 20, 30, 40]);
        }

        #[test]
        fn copy_rejects_wrong_destination_length_and_out_of_bounds_range() {
            let source = IbtSource::Owned(vec![10, 20, 30, 40]);
            assert!(
                source
                    .read_range_into(ByteRange::new(1..3), &mut [0; 1])
                    .is_err()
            );
            assert!(
                source
                    .read_range_into(ByteRange::new(3..5), &mut [0; 2])
                    .is_err()
            );
        }
    }
}

#[cfg(windows)]
pub(crate) mod live {
    use iracing_irsdk::constants::{IRSDK_DATAVALIDEVENTNAME, IRSDK_MEMMAPFILENAME};
    use std::{ptr::NonNull, time::Duration};
    use windows::{
        Win32::{
            Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::{
                Memory::{
                    FILE_MAP_READ, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
                    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery,
                },
                Threading::{OpenEventW, SYNCHRONIZATION_ACCESS_RIGHTS, WaitForSingleObject},
            },
        },
        core::PCWSTR,
    };

    use super::{ByteRange, TelemetrySource, validate_range};

    use crate::{IRacingSDKError, Result, windows::wide_string};

    /// Result of waiting for data updates
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum WaitResult {
        /// Wait resolved with data.
        Signaled,
        /// Wait time elapsed.
        Timeout,
    }

    #[derive(Debug)]
    pub(crate) struct LiveSource {
        mapping: HANDLE,
        base: NonNull<u8>,
        event: HANDLE,
        len: usize,
    }

    impl LiveSource {
        fn wait_for_event(event: HANDLE, timeout_ms: u32) -> Result<WaitResult> {
            tracing::trace!(timeout_ms = timeout_ms, "Waiting for telemetry update");

            let result = unsafe { WaitForSingleObject(event, timeout_ms) };

            match result {
                WAIT_OBJECT_0 => {
                    tracing::trace!("Telemetry update signaled");
                    Ok(WaitResult::Signaled)
                }
                WAIT_TIMEOUT => {
                    tracing::trace!("Wait timed out");
                    Ok(WaitResult::Timeout)
                }
                _ => {
                    let win_err = windows::core::Error::from_thread();
                    Err(IRacingSDKError::windows_api_error(
                        "WaitForSingleObject",
                        win_err,
                    ))
                }
            }
        }

        pub fn try_connect() -> Result<Self> {
            tracing::trace!("Attempting to connect to iRacing shared memory");

            // Open the memory mapping
            let mapping = unsafe {
                let wide_name = wide_string(IRSDK_MEMMAPFILENAME);
                OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR::from_raw(wide_name.as_ptr()))
                    .map_err(|e| IRacingSDKError::windows_api_error("OpenFileMappingW", e))?
            };

            // Map the view
            let base = unsafe {
                let ptr = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
                NonNull::new(ptr.Value as *mut u8).ok_or_else(|| {
                    let win_err = windows::core::Error::from_thread();
                    IRacingSDKError::windows_api_error("MapViewOfFile", win_err)
                })?
            };

            // Query the mapped memory region.
            let mut info = MEMORY_BASIC_INFORMATION::default();

            let result = unsafe {
                VirtualQuery(
                    Some(base.as_ptr().cast()),
                    &mut info,
                    size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };

            if result == 0 {
                let win_err = windows::core::Error::from_thread();
                return Err(IRacingSDKError::windows_api_error("VirtualQuery", win_err));
            }

            let len = info.RegionSize;

            // Open the data valid event
            let event = unsafe {
                let wide_name = wide_string(IRSDK_DATAVALIDEVENTNAME);
                OpenEventW(
                    SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000),
                    false,
                    PCWSTR::from_raw(wide_name.as_ptr()),
                ) // SYNCHRONIZE
                .map_err(|e| IRacingSDKError::windows_api_error("OpenEventW", e))?
            };

            Ok(Self {
                mapping,
                base,
                event,
                len,
            })
        }

        /// Wait for new telemetry data (synchronous - blocks thread)
        pub fn wait_for_update(&self, timeout: Duration) -> Result<WaitResult> {
            let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
            Self::wait_for_event(self.event, ms)
        }

        /// Wait for new telemetry data (async - cooperative, non-blocking)
        ///
        /// This method uses `spawn_blocking` to isolate the synchronous Windows event wait
        /// on a dedicated blocking thread pool, preventing starvation of other async tasks.
        /// The async worker thread yields cooperatively via `.await` while the blocking
        /// thread waits for the Windows event signal.
        ///
        /// At 60Hz (16.67ms frames), the hot path (data already available) never reaches
        /// this method, so spawn_blocking overhead is only paid during startup, pauses,
        /// or frame drops - exactly when we want cooperative yielding anyway.
        pub async fn wait_for_update_async(&self, timeout: Duration) -> Result<WaitResult> {
            // Convert HANDLE to raw pointer value (usize) to make it Send
            // SAFETY: Windows event handles are thread-safe kernel objects
            let event_raw = self.event.0 as usize;
            let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;

            tokio::task::spawn_blocking(move || {
                tracing::trace!(timeout_ms, "Async waiting for Windows event");

                // Reconstruct HANDLE from raw pointer value
                // SAFETY: event_raw came from a valid HANDLE, kernel object is still alive
                let event = HANDLE(event_raw as *mut std::ffi::c_void);
                Self::wait_for_event(event, timeout_ms)
            })
            .await
            .map_err(|e| {
                IRacingSDKError::buffer_operation_error(
                    format!("Event wait task panicked: {}", e),
                    None,
                )
            })?
        }
    }

    impl TelemetrySource for LiveSource {
        fn len(&self) -> usize {
            self.len
        }

        fn read_range_into(&self, range: ByteRange, destination: &mut [u8]) -> Result<()> {
            let range = validate_range(range, self.len, destination.len())?;

            for (index, byte) in destination.iter_mut().enumerate() {
                *byte = unsafe { self.base.as_ptr().add(range.start + index).read_volatile() };
            }

            Ok(())
        }
    }

    unsafe impl Send for LiveSource {}
    unsafe impl Sync for LiveSource {}

    impl Drop for LiveSource {
        fn drop(&mut self) {
            unsafe {
                let addr = MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.base.as_ptr() as *mut _,
                };
                let _ = UnmapViewOfFile(addr);
                let _ = CloseHandle(self.mapping);
                let _ = CloseHandle(self.event);
            }
        }
    }
}
