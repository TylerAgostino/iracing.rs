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
    use std::{ptr::NonNull, sync::Arc, time::Duration};
    use windows::{
        Win32::{
            Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::{
                Memory::{FILE_MAP_READ, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
                    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery},
                Threading::{OpenEventW, SYNCHRONIZATION_ACCESS_RIGHTS, WaitForSingleObject},
            },
        },
        core::PCWSTR,
    };
    use super::{ByteRange, TelemetrySource, validate_range};
    use crate::{IRacingSDKError, Result, windows::wide_string};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum WaitResult { Signaled, Timeout }

    #[derive(Debug)]
    struct OwnedHandle(HANDLE);
    // SAFETY: These mapping/event kernel handles have no thread affinity. The
    // unique owner (or an Arc to it) keeps them open for every operation.
    unsafe impl Send for OwnedHandle {}
    unsafe impl Sync for OwnedHandle {}
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: This object uniquely owns a successfully opened handle.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    #[derive(Debug)]
    struct MappedView(NonNull<u8>);
    // SAFETY: The view is read-only external memory, accessed only through
    // checked volatile reads. No Rust references into it are ever constructed.
    // Ownership keeps it mapped until all synchronous reads have finished.
    unsafe impl Send for MappedView {}
    unsafe impl Sync for MappedView {}
    impl Drop for MappedView {
        fn drop(&mut self) {
            // SAFETY: This is the base returned by our successful MapViewOfFile.
            let _ = unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.0.as_ptr().cast(),
            }) };
        }
    }

    #[derive(Debug)]
    pub(crate) struct Mapping {
        view: MappedView,
        _mapping: OwnedHandle,
        event: Arc<OwnedHandle>,
        len: usize,
    }

    /// External mapping access only; interpretation belongs to LiveReader.
    #[derive(Debug)]
    pub(crate) enum LiveSource {
        Windows(Mapping),
        #[cfg(test)]
        Test(test_source::TestSource),
    }

    /// Order volatile protocol loads, including copies, on Windows processors.
    /// The mapping is normal cacheable RAM, outside Rust-managed allocations.
    /// x86/x64 preserve load-load order; the asm memory clobber is the compiler
    /// barrier. ARM64 additionally needs a hardware load barrier. This is not a
    /// Rust atomic synchronization relationship with the external producer.
    pub(crate) fn read_barrier() {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        // SAFETY: Empty asm has no machine side effects; omitting nomem/readonly
        // supplies a compiler memory barrier. Normal x86 loads are ordered.
        unsafe { std::arch::asm!("", options(nostack, preserves_flags)); }
        #[cfg(target_arch = "aarch64")]
        // SAFETY: DMB ISHLD is an unprivileged ARM64 load-ordering instruction.
        unsafe { std::arch::asm!("dmb ishld", options(nostack, preserves_flags)); }
        #[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
        compile_error!("Live shared memory needs a read barrier for this architecture");
    }

    impl LiveSource {
        pub fn try_connect() -> Result<Self> {
            let name = wide_string(IRSDK_MEMMAPFILENAME);
            // SAFETY: name is a live NUL-terminated UTF-16 string.
            let mapping = OwnedHandle(unsafe {
                OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(name.as_ptr()))
            }.map_err(|e| IRacingSDKError::windows_api_error("OpenFileMappingW", e))?);
            // SAFETY: mapping is owned and open; request a read-only full view.
            let raw = unsafe { MapViewOfFile(mapping.0, FILE_MAP_READ, 0, 0, 0) };
            let view = MappedView(NonNull::new(raw.Value.cast()).ok_or_else(||
                IRacingSDKError::windows_api_error("MapViewOfFile", windows::core::Error::from_thread()))?);
            let mut info = MEMORY_BASIC_INFORMATION::default();
            // SAFETY: view is mapped and info is writable for its exact size.
            if unsafe { VirtualQuery(Some(view.0.as_ptr().cast()), &mut info,
                size_of::<MEMORY_BASIC_INFORMATION>()) } == 0 {
                return Err(IRacingSDKError::windows_api_error("VirtualQuery", windows::core::Error::from_thread()));
            }
            // A page-file-backed SDK view is one committed region. Conservatively
            // bound reads to this queried region even if a larger view exists.
            let len = info.RegionSize;
            if info.BaseAddress != view.0.as_ptr().cast() || len > isize::MAX as usize {
                return Err(IRacingSDKError::parse_error("LiveSource", "Invalid mapped view extent"));
            }
            let name = wide_string(IRSDK_DATAVALIDEVENTNAME);
            // SAFETY: name is NUL-terminated; only SYNCHRONIZE access is needed.
            let event = OwnedHandle(unsafe { OpenEventW(SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000),
                false, PCWSTR(name.as_ptr())) }
                .map_err(|e| IRacingSDKError::windows_api_error("OpenEventW", e))?);
            Ok(Self::Windows(Mapping { view, _mapping: mapping, event: Arc::new(event), len }))
        }

        /// One aligned 32-bit load, never four potentially torn byte loads.
        pub fn read_i32(&self, offset: usize) -> Result<i32> {
            let end = offset.checked_add(size_of::<i32>()).ok_or_else(||
                IRacingSDKError::parse_error("LiveSource", "Synchronization offset overflow"))?;
            validate_range(ByteRange::new(offset..end), self.len(), size_of::<i32>())?;
            if !offset.is_multiple_of(align_of::<i32>()) {
                return Err(IRacingSDKError::parse_error("LiveSource", "Unaligned synchronization field"));
            }
            match self {
                Self::Windows(mapping) => {
                    // SAFETY: Mapping bases are page-aligned. Bounds/alignment
                    // were checked above; all i32 bit patterns are valid. This
                    // external memory is never accessed using Rust references.
                    Ok(i32::from_le(unsafe { mapping.view.0.as_ptr().add(offset).cast::<i32>().read_volatile() }))
                }
                #[cfg(test)]
                Self::Test(source) => source.read_i32(offset),
            }
        }

        pub async fn wait_for_update_async(&self, timeout: Duration) -> Result<WaitResult> {
            let Self::Windows(mapping) = self else {
                #[cfg(test)]
                { tokio::task::yield_now().await; return Ok(WaitResult::Timeout); }
            };
            let event = Arc::clone(&mapping.event);
            // INFINITE is u32::MAX; keep even enormous requested waits bounded.
            let timeout_ms = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
            tokio::task::spawn_blocking(move || {
                // SAFETY: The closure owns an Arc, retaining the handle even if
                // its JoinHandle/future and the original source are dropped.
                match unsafe { WaitForSingleObject(event.0, timeout_ms) } {
                    WAIT_OBJECT_0 => Ok(WaitResult::Signaled),
                    WAIT_TIMEOUT => Ok(WaitResult::Timeout),
                    _ => Err(IRacingSDKError::windows_api_error("WaitForSingleObject", windows::core::Error::from_thread())),
                }
            }).await.map_err(|e| IRacingSDKError::buffer_operation_error(format!("Event wait task failed: {e}"), None))?
        }
    }

    impl TelemetrySource for LiveSource {
        fn len(&self) -> usize {
            match self {
                Self::Windows(mapping) => mapping.len,
                #[cfg(test)]
                Self::Test(source) => source.len(),
            }
        }
        fn read_range_into(&self, range: ByteRange, destination: &mut [u8]) -> Result<()> {
            let range = validate_range(range, self.len(), destination.len())?;
            match self {
                Self::Windows(mapping) => {
                    for (index, byte) in destination.iter_mut().enumerate() {
                        // SAFETY: The range was bounded above; u8 has no invalid
                        // representations or alignment requirement. Volatile
                        // reads preserve external accesses throughout the copy.
                        *byte = unsafe { mapping.view.0.as_ptr().add(range.start + index).read_volatile() };
                    }
                    Ok(())
                }
                #[cfg(test)]
                Self::Test(source) => source.read_range_into(range, destination),
            }
        }
    }

    #[cfg(test)]
    pub(crate) mod test_source {
        use super::*;
        use std::{ops::Range, sync::Mutex};
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum Read { Bytes(Range<usize>), I32(usize) }
        type Hook = Box<dyn FnMut(Read, &mut [u8]) + Send>;
        pub struct TestSource { bytes: Mutex<(Vec<u8>, Hook)>, len: usize }
        impl std::fmt::Debug for TestSource {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.debug_struct("TestSource").finish_non_exhaustive() }
        }
        impl TestSource {
            pub fn new(bytes: Vec<u8>, hook: impl FnMut(Read, &mut [u8]) + Send + 'static) -> Self {
                Self { len: bytes.len(), bytes: Mutex::new((bytes, Box::new(hook))) }
            }
            pub fn len(&self) -> usize { self.len }
            pub fn read_i32(&self, offset: usize) -> Result<i32> {
                let mut state = self.bytes.lock().unwrap();
                let (bytes, hook) = &mut *state;
                hook(Read::I32(offset), bytes);
                Ok(i32::from_le_bytes(bytes[offset..offset+4].try_into().unwrap()))
            }
            pub fn read_range_into(&self, range: Range<usize>, destination: &mut [u8]) -> Result<()> {
                let mut state = self.bytes.lock().unwrap();
                let (bytes, hook) = &mut *state;
                hook(Read::Bytes(range.clone()), bytes);
                destination.copy_from_slice(&bytes[range]);
                Ok(())
            }
        }
    }
}
