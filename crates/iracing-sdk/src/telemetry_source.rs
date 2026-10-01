use crate::{IRacingSDKError, Result, types::ByteRange};
use std::ops::Range;

fn validate_range(
    range: ByteRange,
    source_len: usize,
    destination_len: usize,
) -> Result<Range<usize>> {
    let len = range.len();
    let range = range.as_range();

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

pub mod disk {
    use std::borrow::Cow;
    use std::ops::Range;

    use super::validate_range;
    use crate::{IRacingSDKError, Result, types::ByteRange};

    use memmap2::Mmap;
    use zerocopy::IntoBytes;

    pub(crate) enum IbtSource {
        Mapped(Mmap),
        Owned(Vec<u8>),
    }

    impl IbtSource {
        pub(crate) fn len(&self) -> usize {
            match self {
                Self::Mapped(source) => source.len(),
                Self::Owned(source) => source.len(),
            }
        }

        fn as_bytes(&self) -> &[u8] {
            match self {
                Self::Mapped(source) => source.as_bytes(),
                Self::Owned(source) => source.as_bytes(),
            }
        }

        fn get(&self, range: ByteRange) -> Option<&[u8]> {
            self.as_bytes().get(range.as_range())
        }

        /// # Safety
        ///
        /// The caller must guarantee that `range` is entirely
        /// contained within the source.
        pub(crate) unsafe fn slice_unchecked(&self, range: Range<usize>) -> &[u8] {
            unsafe { self.as_bytes().get_unchecked(range) }
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

            let bytes = source.read_range(ByteRange::new(1..3).unwrap()).unwrap();
            assert!(matches!(bytes, Cow::Borrowed(_)));
            assert_eq!(bytes.as_ref(), &[20, 30]);
            assert_eq!(bytes.as_ptr(), source.as_bytes()[1..3].as_ptr());
            assert!(source.read_range(ByteRange::new(3..5).unwrap()).is_err());
        }

        #[test]
        fn mapped_source_reads_match_file_bytes() -> Result<()> {
            let path = crate::test_utils::require_smallest_ibt_fixture()?;
            let file = std::fs::File::open(&path)?;
            let expected = std::fs::read(&path)?;
            // SAFETY: The generated fixture remains unchanged while this read-only
            // mapping and its borrowed range are alive.
            let source = IbtSource::Mapped(unsafe { Mmap::map(&file)? });
            let range = ByteRange::new(1..9).unwrap();

            assert_eq!(source.len(), expected.len());
            assert_eq!(source.read_range(range)?.as_ref(), &expected[1..9]);
            Ok(())
        }

        #[test]
        fn copies_exact_range_without_touching_other_source_bytes() {
            let source = IbtSource::Owned(vec![10, 20, 30, 40]);
            let mut destination = [0; 2];
            source
                .read_range_into(ByteRange::new(1..3).unwrap(), &mut destination)
                .unwrap();
            assert_eq!(destination, [20, 30]);
            assert_eq!(source.as_bytes(), &[10, 20, 30, 40]);
        }

        #[test]
        fn copy_rejects_wrong_destination_length_and_out_of_bounds_range() {
            let source = IbtSource::Owned(vec![10, 20, 30, 40]);
            assert!(
                source
                    .read_range_into(ByteRange::new(1..3).unwrap(), &mut [0; 1])
                    .is_err()
            );
            assert!(
                source
                    .read_range_into(ByteRange::new(3..5).unwrap(), &mut [0; 2])
                    .is_err()
            );
        }
    }
}

#[cfg(windows)]
pub mod live {
    use super::{ByteRange, validate_range};
    use crate::{IRacingSDKError, Result, windows::wide_string};
    use iracing_irsdk::constants::{IRSDK_DATAVALIDEVENTNAME, IRSDK_MEMMAPFILENAME};
    use std::{ptr::NonNull, sync::Arc, time::Duration};
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

    // Import the Windows implementation as an opaque external call. Unlike a
    // Rust memcpy intrinsic, the compiler cannot fold this into ordinary Rust
    // loads from memory which the simulator changes independently. The native
    // routine performs the bulk copy; publication checks belong to LiveReader.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn RtlMoveMemory(
            destination: *mut std::ffi::c_void,
            source: *const std::ffi::c_void,
            length: usize,
        );
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum WaitResult {
        Signaled,
        Timeout,
    }

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
    // SAFETY: The view is external memory accessed only through bounded volatile
    // scalar reads and native copies. No Rust references into it are constructed.
    // Ownership keeps it mapped until all synchronous reads have finished.
    unsafe impl Send for MappedView {}
    unsafe impl Sync for MappedView {}
    impl Drop for MappedView {
        fn drop(&mut self) {
            // SAFETY: This is the base returned by our successful MapViewOfFile.
            let _ = unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.0.as_ptr().cast(),
                })
            };
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
        unsafe {
            std::arch::asm!("", options(nostack, preserves_flags));
        }
        #[cfg(target_arch = "aarch64")]
        // SAFETY: DMB ISHLD is an unprivileged ARM64 load-ordering instruction.
        unsafe {
            std::arch::asm!("dmb ishld", options(nostack, preserves_flags));
        }
        #[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
        compile_error!("Live shared memory needs a read barrier for this architecture");
    }

    impl LiveSource {
        pub fn try_connect() -> Result<Self> {
            let name = wide_string(IRSDK_MEMMAPFILENAME);
            // SAFETY: name is a live NUL-terminated UTF-16 string.
            let mapping = OwnedHandle(
                unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(name.as_ptr())) }
                    .map_err(|e| IRacingSDKError::windows_api_error("OpenFileMappingW", e))?,
            );
            // SAFETY: mapping is owned and open; request a read-only full view.
            let raw = unsafe { MapViewOfFile(mapping.0, FILE_MAP_READ, 0, 0, 0) };
            let view = MappedView(NonNull::new(raw.Value.cast()).ok_or_else(|| {
                IRacingSDKError::windows_api_error(
                    "MapViewOfFile",
                    windows::core::Error::from_thread(),
                )
            })?);
            let mut info = MEMORY_BASIC_INFORMATION::default();
            // SAFETY: view is mapped and info is writable for its exact size.
            if unsafe {
                VirtualQuery(
                    Some(view.0.as_ptr().cast()),
                    &mut info,
                    size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            } == 0
            {
                return Err(IRacingSDKError::windows_api_error(
                    "VirtualQuery",
                    windows::core::Error::from_thread(),
                ));
            }
            // A page-file-backed SDK view is one committed region. Conservatively
            // bound reads to this queried region even if a larger view exists.
            let len = info.RegionSize;
            if info.BaseAddress != view.0.as_ptr().cast() || len > isize::MAX as usize {
                return Err(IRacingSDKError::parse_error(
                    "LiveSource",
                    "Invalid mapped view extent",
                ));
            }
            let name = wide_string(IRSDK_DATAVALIDEVENTNAME);
            // SAFETY: name is NUL-terminated; only SYNCHRONIZE access is needed.
            let event = OwnedHandle(
                unsafe {
                    OpenEventW(
                        SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000),
                        false,
                        PCWSTR(name.as_ptr()),
                    )
                }
                .map_err(|e| IRacingSDKError::windows_api_error("OpenEventW", e))?,
            );
            Ok(Self::Windows(Mapping {
                view,
                _mapping: mapping,
                event: Arc::new(event),
                len,
            }))
        }

        /// One aligned 32-bit load, never four potentially torn byte loads.
        pub fn read_i32(&self, offset: usize) -> Result<i32> {
            let end = offset.checked_add(size_of::<i32>()).ok_or_else(|| {
                IRacingSDKError::parse_error("LiveSource", "Synchronization offset overflow")
            })?;
            validate_range(ByteRange::new(offset..end)?, self.len(), size_of::<i32>())?;
            if !offset.is_multiple_of(align_of::<i32>()) {
                return Err(IRacingSDKError::parse_error(
                    "LiveSource",
                    "Unaligned synchronization field",
                ));
            }
            // SAFETY: Bounds and alignment were validated above.
            unsafe { self.read_i32_unchecked(offset) }
        }

        /// Read a synchronization word using geometry validated at activation.
        ///
        /// # Safety
        /// `offset` must be aligned for i32 and its four bytes must fit in this
        /// source, which must retain its mapping throughout the read.
        pub(crate) unsafe fn read_i32_unchecked(&self, offset: usize) -> Result<i32> {
            match self {
                Self::Windows(mapping) => {
                    // SAFETY: The caller establishes bounds and alignment. The
                    // external mapping is initialized and all i32 bits are valid.
                    Ok(i32::from_le(unsafe {
                        mapping
                            .view
                            .0
                            .as_ptr()
                            .add(offset)
                            .cast::<i32>()
                            .read_volatile()
                    }))
                }
                #[cfg(test)]
                Self::Test(source) => source.read_i32(offset),
            }
        }

        /// # Safety
        /// `offset` must identify a byte within this source's retained mapping.
        pub(crate) unsafe fn read_u8_unchecked(&self, offset: usize) -> Result<u8> {
            match self {
                // SAFETY: The caller establishes bounds. u8 has no alignment or
                // representation restrictions; the mapping remains owned.
                Self::Windows(mapping) => {
                    Ok(unsafe { mapping.view.0.as_ptr().add(offset).read_volatile() })
                }
                #[cfg(test)]
                Self::Test(source) => source.read_u8(offset),
            }
        }

        /// Bulk-copy a range whose source bounds were checked at activation.
        ///
        /// # Safety
        /// The range must fit in this source's retained mapping and its length
        /// must equal `destination.len()`. Destination storage must be disjoint
        /// from the external mapping. The copy can tear; readers must validate
        /// the producer's publication fields before accepting its bytes.
        pub(crate) unsafe fn read_range_into_unchecked(
            &self,
            range: &ByteRange,
            destination: &mut [u8],
        ) -> Result<()> {
            // SAFETY: The caller guarantees a bounded range with exactly the
            // destination length, establishing the offset copy's full extent.
            unsafe { self.copy_unchecked(range.start(), destination) }
        }

        /// Copy bytes from an offset validated at activation, without building
        /// a range or repeating geometry checks.
        ///
        /// # Safety
        /// `offset + destination.len()` must not overflow and must fit within
        /// this retained source. The destination must be disjoint from it.
        /// Publication checks are required before accepting a possibly torn copy.
        pub(crate) unsafe fn copy_unchecked(
            &self,
            offset: usize,
            destination: &mut [u8],
        ) -> Result<()> {
            match self {
                Self::Windows(mapping) => {
                    if !destination.is_empty() {
                        // SAFETY: Caller establishes both extents and disjoint
                        // ownership. RtlMoveMemory accepts unaligned byte ranges;
                        // the opaque Windows call copies into owned Rust storage
                        // without constructing a reference into shared memory.
                        unsafe {
                            RtlMoveMemory(
                                destination.as_mut_ptr().cast(),
                                mapping.view.0.as_ptr().add(offset).cast(),
                                destination.len(),
                            );
                        }
                    }
                    Ok(())
                }
                #[cfg(test)]
                Self::Test(source) => {
                    source.read_range_into(offset..offset + destination.len(), destination)
                }
            }
        }

        pub async fn wait_for_update_async(&self, timeout: Duration) -> Result<WaitResult> {
            let mapping = match self {
                Self::Windows(mapping) => mapping,
                #[cfg(test)]
                Self::Test(_) => {
                    tokio::task::yield_now().await;
                    return Ok(WaitResult::Timeout);
                }
            };
            wait_for_event_async(Arc::clone(&mapping.event), timeout).await
        }

        pub(crate) fn len(&self) -> usize {
            match self {
                Self::Windows(mapping) => mapping.len,
                #[cfg(test)]
                Self::Test(source) => source.len(),
            }
        }
    }

    #[cfg(test)]
    impl LiveSource {
        fn read_range_into(&self, range: ByteRange, destination: &mut [u8]) -> Result<()> {
            validate_range(range.clone(), self.len(), destination.len())?;
            // SAFETY: The safe entry point validates every caller-supplied range
            // and destination. The source owns a mapping disjoint from Rust data.
            unsafe { self.read_range_into_unchecked(&range, destination) }
        }

        fn read_range(&self, range: ByteRange) -> Result<std::borrow::Cow<'_, [u8]>> {
            // Check before allocating, including deliberately enormous test ranges.
            validate_range(range.clone(), self.len(), range.len())?;
            let mut bytes = vec![0; range.len()];
            self.read_range_into(range, &mut bytes)?;
            Ok(std::borrow::Cow::Owned(bytes))
        }
    }

    async fn wait_for_event_async(
        event: Arc<OwnedHandle>,
        timeout: Duration,
    ) -> Result<WaitResult> {
        // INFINITE is u32::MAX; keep even enormous requested waits bounded.
        let timeout_ms = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
        tokio::task::spawn_blocking(move || {
            // SAFETY: The closure owns an Arc, retaining the handle even if
            // its JoinHandle/future and the original source are dropped.
            match unsafe { WaitForSingleObject(event.0, timeout_ms) } {
                WAIT_OBJECT_0 => Ok(WaitResult::Signaled),
                WAIT_TIMEOUT => Ok(WaitResult::Timeout),
                _ => Err(IRacingSDKError::windows_api_error(
                    "WaitForSingleObject",
                    windows::core::Error::from_thread(),
                )),
            }
        })
        .await
        .map_err(|e| {
            IRacingSDKError::buffer_operation_error(format!("Event wait task failed: {e}"), None)
        })?
    }

    #[cfg(test)]
    pub(crate) mod test_source {
        use super::*;
        use std::{
            ops::Range,
            sync::{
                Mutex,
                atomic::{AtomicUsize, Ordering},
            },
        };
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum Read {
            Bytes(Range<usize>),
            I32(usize),
            U8(usize),
        }
        type Hook = Box<dyn FnMut(Read, &mut [u8]) + Send>;
        pub struct TestSource {
            bytes: Mutex<(Vec<u8>, Hook)>,
            len: usize,
            len_queries: AtomicUsize,
        }
        impl std::fmt::Debug for TestSource {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("TestSource").finish_non_exhaustive()
            }
        }
        impl TestSource {
            pub fn new(bytes: Vec<u8>, hook: impl FnMut(Read, &mut [u8]) + Send + 'static) -> Self {
                Self {
                    len: bytes.len(),
                    len_queries: AtomicUsize::new(0),
                    bytes: Mutex::new((bytes, Box::new(hook))),
                }
            }
            pub fn len(&self) -> usize {
                self.len_queries.fetch_add(1, Ordering::Relaxed);
                self.len
            }
            pub fn len_queries(&self) -> usize {
                self.len_queries.load(Ordering::Relaxed)
            }
            pub fn read_i32(&self, offset: usize) -> Result<i32> {
                let mut state = self.bytes.lock().unwrap();
                let (bytes, hook) = &mut *state;
                hook(Read::I32(offset), bytes);
                Ok(i32::from_le_bytes(
                    bytes[offset..offset + 4].try_into().unwrap(),
                ))
            }
            pub fn read_u8(&self, offset: usize) -> Result<u8> {
                let mut state = self.bytes.lock().unwrap();
                let (bytes, hook) = &mut *state;
                hook(Read::U8(offset), bytes);
                Ok(bytes[offset])
            }

            pub fn read_range_into(
                &self,
                range: Range<usize>,
                destination: &mut [u8],
            ) -> Result<()> {
                let mut state = self.bytes.lock().unwrap();
                let (bytes, hook) = &mut *state;
                hook(Read::Bytes(range.clone()), bytes);
                destination.copy_from_slice(&bytes[range]);
                Ok(())
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::test_source::TestSource;
        use super::*;
        use std::sync::atomic::{AtomicBool, Ordering};

        #[test]
        fn owned_copies_and_sync_reads_check_bounds_alignment_and_lengths() {
            let source = LiveSource::Test(TestSource::new(vec![1, 2, 3, 4, 5, 6, 7, 8], |_, _| {}));
            assert_eq!(
                source.read_i32(0).unwrap(),
                i32::from_le_bytes([1, 2, 3, 4])
            );
            assert!(source.read_i32(1).is_err());
            assert!(source.read_i32(8).is_err());
            assert!(source.read_i32(usize::MAX).is_err());
            assert!(matches!(
                source.read_range(ByteRange::new(0..4).unwrap()).unwrap(),
                std::borrow::Cow::Owned(_)
            ));
            assert!(
                source
                    .read_range(ByteRange::new(0..usize::MAX).unwrap())
                    .is_err()
            );
            assert!(
                source
                    .read_range_into(ByteRange::new(0..4).unwrap(), &mut [0; 3])
                    .is_err()
            );
            assert!(
                source
                    .read_range_into(ByteRange::new(7..9).unwrap(), &mut [0; 2])
                    .is_err()
            );
        }

        #[test]
        fn retained_snapshot_cannot_change_with_the_source() {
            let changed = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&changed);
            let source = LiveSource::Test(TestSource::new(vec![1; 4], move |_, bytes| {
                if flag.load(Ordering::Relaxed) {
                    bytes.fill(2);
                }
            }));
            let first = source.read_range(ByteRange::new(0..4).unwrap()).unwrap();
            changed.store(true, Ordering::Relaxed);
            let second = source.read_range(ByteRange::new(0..4).unwrap()).unwrap();
            assert_eq!(&*first, &[1; 4]);
            assert_eq!(&*second, &[2; 4]);
        }

        // These generated bindings require the Security feature only for an
        // optional SECURITY_ATTRIBUTES pointer. Tests use null and declare that
        // ABI without adding a production dependency.
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CreateEventW(
                attributes: *const std::ffi::c_void,
                manual_reset: i32,
                initial_state: i32,
                name: *const u16,
            ) -> HANDLE;
            fn CreateFileMappingW(
                file: HANDLE,
                attributes: *const std::ffi::c_void,
                protection: u32,
                maximum_size_high: u32,
                maximum_size_low: u32,
                name: *const u16,
            ) -> HANDLE;
        }

        fn mapped_source(bytes: &[u8]) -> LiveSource {
            use windows::Win32::{
                Foundation::INVALID_HANDLE_VALUE,
                System::Memory::{FILE_MAP_ALL_ACCESS, PAGE_READWRITE},
            };
            // SAFETY: An unnamed page-file mapping uses default security. The
            // checked size is nonzero and fits the low 32-bit size parameter.
            assert!(!bytes.is_empty());
            let size = u32::try_from(bytes.len()).unwrap();
            let handle = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    std::ptr::null(),
                    PAGE_READWRITE.0,
                    0,
                    size,
                    std::ptr::null(),
                )
            };
            assert!(!handle.is_invalid());
            let mapping = OwnedHandle(handle);
            // SAFETY: The mapping is owned; request its complete writable view
            // only to initialize test data before any reader exists.
            let raw = unsafe { MapViewOfFile(mapping.0, FILE_MAP_ALL_ACCESS, 0, 0, 0) };
            let view = MappedView(NonNull::new(raw.Value.cast()).unwrap());
            // SAFETY: Both extents contain bytes.len() valid bytes, are disjoint,
            // and the destination view is writable. No reads occur concurrently.
            unsafe {
                RtlMoveMemory(view.0.as_ptr().cast(), bytes.as_ptr().cast(), bytes.len());
            }
            // SAFETY: Default security, unnamed event, valid BOOL arguments.
            let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
            assert!(!event.is_invalid());
            LiveSource::Windows(Mapping {
                view,
                _mapping: mapping,
                event: Arc::new(OwnedHandle(event)),
                len: bytes.len(),
            })
        }

        #[test]
        fn native_bulk_copy_preserves_odd_lengths_offsets_and_destination_guards() {
            let bytes: Vec<_> = (0..32768).map(|index| (index % 251) as u8).collect();
            let source = mapped_source(&bytes);
            for offset in [0, bytes.len() - 4] {
                assert_eq!(
                    source.read_i32(offset).unwrap(),
                    i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
                );
            }
            // SAFETY: Byte 1 is within the retained mapping initialized above.
            assert_eq!(unsafe { source.read_u8_unchecked(1).unwrap() }, bytes[1]);
            for length in [0, 1, 7, 16, 31, 8587, 32767] {
                let mut destination = vec![0xfe; length + 2];
                source
                    .read_range_into(
                        ByteRange::new(1..1 + length).unwrap(),
                        &mut destination[1..1 + length],
                    )
                    .unwrap();
                assert_eq!(destination[0], 0xfe);
                assert_eq!(destination[length + 1], 0xfe);
                assert_eq!(&destination[1..1 + length], &bytes[1..1 + length]);
            }
            // A zero-length range at the end must not dereference its pointer.
            source
                .read_range_into(ByteRange::new(bytes.len()..bytes.len()).unwrap(), &mut [])
                .unwrap();
        }

        #[tokio::test]
        async fn cancelled_wait_retains_event_until_blocking_work_finishes() {
            // SAFETY: Null security/name pointers request default attributes and
            // an unnamed event; BOOL parameters have their documented i32 ABI.
            let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
            assert!(!handle.is_invalid());
            let owner = Arc::new(OwnedHandle(handle));
            let weak = Arc::downgrade(&owner);
            let mut wait = Box::pin(wait_for_event_async(
                Arc::clone(&owner),
                Duration::from_secs(5),
            ));
            // Poll once to deterministically dispatch the blocking closure, then
            // cancel the waiting future and release the original owner.
            std::future::poll_fn(|cx| {
                use std::future::Future;
                assert!(wait.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            drop(wait);
            drop(owner);
            let retained = weak
                .upgrade()
                .expect("blocking wait must retain event ownership");
            // SAFETY: retained owns the same valid event being waited on.
            unsafe {
                windows::Win32::System::Threading::SetEvent(retained.0).unwrap();
            }
            drop(retained);
            tokio::time::timeout(Duration::from_secs(2), async {
                while weak.upgrade().is_some() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("blocking wait did not release its event");
        }
    }
}
