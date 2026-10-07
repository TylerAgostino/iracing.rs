use crate::{IRacingSDKError, Result};
use std::ops::Range;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Unchecked;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Checked {
    source_len: usize,
}

/// Offset and length for a byte span within an SDK data source.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ByteRegion<S = Unchecked> {
    /// Start offset of the region, measured in bytes from the source origin.
    offset: usize,
    /// Length of the region in bytes.
    length: usize,

    state: S,
}

impl ByteRegion<Unchecked> {
    pub fn new(offset: usize, length: usize) -> Result<Self> {
        offset.checked_add(length).ok_or_else(|| {
            IRacingSDKError::parse_error(
                "ByteRegion",
                format!("Region offset {offset} + length {length} overflows usize"),
            )
        })?;

        Ok(Self {
            offset,
            length,
            state: Unchecked,
        })
    }

    pub fn checked(offset: usize, length: usize, source_len: usize) -> Result<ByteRegion<Checked>> {
        let end = offset.checked_add(length).ok_or_else(|| {
            IRacingSDKError::parse_error(
                "ByteRegion",
                format!("Region offset {offset} + length {length} overflows usize"),
            )
        })?;

        if end > source_len {
            return Err(IRacingSDKError::parse_error(
                "ByteRegion",
                format!("Region end is out of bounds: {} ({})", end, source_len),
            ));
        }

        Ok(ByteRegion {
            offset,
            length,
            state: Checked { source_len },
        })
    }

    pub fn check(self, source_len: usize) -> Result<ByteRegion<Checked>> {
        if self.end() > source_len {
            return Err(IRacingSDKError::parse_error(
                "ByteRegion",
                format!(
                    "Region end is out of bounds: {} ({})",
                    self.end(),
                    source_len
                ),
            ));
        }

        Ok(ByteRegion {
            offset: self.offset,
            length: self.length,
            state: Checked { source_len },
        })
    }
}

impl ByteRegion<Checked> {
    /// Creates a region validated against a source length.
    pub fn new(offset: usize, length: usize, source_len: usize) -> Result<Self> {
        ByteRegion::<Unchecked>::new(offset, length)?.check(source_len)
    }

    /// Returns the source length against which this region was validated.
    pub const fn source_len(&self) -> usize {
        self.state.source_len
    }
}

impl<S> ByteRegion<S> {
    /// Returns the source-relative starting byte offset.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Returns the length of the region in bytes.
    pub fn len(&self) -> usize {
        self.length
    }

    /// Returns whether the region contains no bytes.
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Returns the exclusive end offset of the region.
    pub fn end(&self) -> usize {
        self.offset + self.length
    }

    /// Returns the region as a half-open byte range.
    ///
    /// Construction guarantees that calculating the range end cannot overflow.
    pub fn as_range(&self) -> Range<usize> {
        self.offset..self.end()
    }

    /// Returns whether each region begins before the other region ends.
    pub fn overlaps<T>(&self, other: &ByteRegion<T>) -> bool {
        // Ensure self is not empty...
        !self.is_empty()
            // Other is not empty...
            && !other.is_empty()
            // Overlap
            && self.offset < other.end()
            && other.offset < self.end()
    }
}

impl TryFrom<(usize, usize)> for ByteRegion<Unchecked> {
    type Error = IRacingSDKError;

    /// Creates a region from an `(offset, length)` pair.
    ///
    /// # Errors
    ///
    /// Returns [`IRacingSDKError::Parse`] if `offset + length` overflows
    /// `usize`.
    fn try_from((offset, length): (usize, usize)) -> Result<Self> {
        Self::new(offset, length)
    }
}

impl TryFrom<Range<usize>> for ByteRegion<Unchecked> {
    type Error = IRacingSDKError;

    /// Creates a region from a half-open byte range.
    ///
    /// # Errors
    ///
    /// Returns [`IRacingSDKError::Parse`] if the range ends before it starts.
    fn try_from(range: Range<usize>) -> Result<Self> {
        let length = range.end.checked_sub(range.start).ok_or_else(|| {
            IRacingSDKError::parse_error("ByteRegion::try_from", "Range end precedes range start")
        })?;

        Self::new(range.start, length)
    }
}

impl From<ByteRegion<Unchecked>> for Range<usize> {
    fn from(value: ByteRegion<Unchecked>) -> Self {
        value.as_range()
    }
}

impl From<ByteRegion<Checked>> for Range<usize> {
    fn from(value: ByteRegion<Checked>) -> Self {
        value.as_range()
    }
}

pub type CheckedByteRegion = ByteRegion<Checked>;
pub type UncheckedByteRegion = ByteRegion<Unchecked>;

#[cfg(test)]
mod tests {
    use std::assert_eq;

    use crate::{ByteRegion, types::regions::bytes::Unchecked};

    #[test]
    fn empty_range_succeeds() {
        assert!(ByteRegion::<Unchecked>::new(0, 0).is_ok());
    }

    #[test]
    fn end_calculation_succeeds() {
        let region = ByteRegion::<Unchecked>::new(0, 2).unwrap();

        assert_eq!(region.end(), 2);
    }

    #[test]
    fn usize_overflow_rejected() {
        assert!(ByteRegion::<Unchecked>::new(usize::MAX, 1).is_err());
    }

    #[test]
    fn try_from_range() {
        assert!(
            ByteRegion::try_from(std::ops::Range {
                start: 200,
                end: 100,
            })
            .is_err()
        );

        let valid_region_result = ByteRegion::try_from(1..100);
        assert!(valid_region_result.is_ok());
        let valid_region = valid_region_result.unwrap();
        assert_eq!(valid_region.offset, 1);
        assert_eq!(valid_region.length, 99);
    }

    #[allow(clippy::reversed_empty_ranges)]
    #[test]
    fn try_from_range_rejects_end_preceding_start() {
        assert!(ByteRegion::try_from(12..11).is_err());
    }

    #[test]
    fn try_from_tuple() {
        // `usize` overflow
        assert!(ByteRegion::try_from((usize::MAX, 1)).is_err());
        assert!(ByteRegion::try_from((0, 100)).is_ok());
    }

    #[test]
    fn overlap_half_open() {
        // 0..4
        let region = ByteRegion::<Unchecked>::new(0, 4).unwrap();
        // 2..5
        let overlap = ByteRegion::<Unchecked>::new(2, 3).unwrap();

        assert!(region.overlaps(&overlap));

        // 4..5
        let adjacent = ByteRegion::<Unchecked>::new(4, 1).unwrap();
        assert!(!region.overlaps(&adjacent));
    }

    #[test]
    fn empty_region_never_overlaps_even_inside_another_region() {
        let occupied = ByteRegion::<Unchecked>::new(2, 5).unwrap();
        for offset in [2, 4, 7] {
            let empty = ByteRegion::<Unchecked>::new(offset, 0).unwrap();
            assert!(!empty.overlaps(&occupied));
            assert!(!occupied.overlaps(&empty));
        }
    }

    #[test]
    fn valid_regions_can_end_at_usize_max() {
        for region in [
            ByteRegion::<Unchecked>::new(usize::MAX, 0).unwrap(),
            ByteRegion::<Unchecked>::new(usize::MAX - 1, 1).unwrap(),
        ] {
            assert_eq!(region.end(), usize::MAX);
            assert_eq!(region.as_range().end, usize::MAX);
        }
        assert!(ByteRegion::<Unchecked>::new(usize::MAX, 1).is_err());
    }
}
