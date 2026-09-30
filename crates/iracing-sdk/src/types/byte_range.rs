use crate::IRacingSDKError;
use std::ops::Range;

/// A nonnegative, ordered half-open byte range. Construction validates the
/// endpoints; source bounds remain the responsibility of the reader/source.
#[derive(Debug, Clone)]
pub(crate) struct ByteRange {
    range: Range<usize>,
}

impl ByteRange {
    pub fn new(range: Range<usize>) -> crate::Result<Self> {
        if range.end < range.start {
            return Err(IRacingSDKError::parse_error(
                "ByteRange::new",
                "Byte range end precedes its start",
            ));
        }
        Ok(Self { range })
    }

    pub fn as_range(&self) -> Range<usize> {
        self.range.clone()
    }

    pub fn len(&self) -> usize {
        self.range.end - self.range.start
    }

    pub fn start(&self) -> usize {
        self.range.start
    }

    pub fn end(&self) -> usize {
        self.range.end
    }

    pub fn overlaps(&self, other: &ByteRange) -> bool {
        self.start() < other.end() && other.start() < self.end()
    }
}

macro_rules! impl_try_from_byte_range {
    ($($from:ty),+ $(,)?) => {
        $(impl TryFrom<Range<$from>> for ByteRange {
            type Error = IRacingSDKError;

            fn try_from(value: Range<$from>) -> crate::Result<Self> {
                let start = usize::try_from(value.start).map_err(|_| {
                    IRacingSDKError::parse_error(
                        "ByteRange::try_from",
                        format!("{} overflowed usize", value.start),
                    )
                })?;

                let end = usize::try_from(value.end).map_err(|_| {
                    IRacingSDKError::parse_error(
                        "ByteRange::try_from",
                        format!("{} overflowed usize", value.end),
                    )
                })?;

                Self::new(start..end)
            }
        })+
    };
}

impl_try_from_byte_range!(i32, u32);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_ranges_preserve_boundaries_and_reject_negative_offsets() {
        let range = ByteRange::try_from(3..8).unwrap();
        assert_eq!(range.as_range(), 3..8);
        assert_eq!(range.len(), 5);

        assert!(ByteRange::try_from(-1..8).is_err());
        assert!(ByteRange::try_from(Range { start: 0, end: -1 }).is_err());
    }

    #[test]
    fn every_constructor_rejects_inverted_ranges() {
        assert!(ByteRange::new(Range { start: 8, end: 3 }).is_err());
        assert!(
            ByteRange::try_from(Range {
                start: 8_i32,
                end: 3
            })
            .is_err()
        );
        assert!(
            ByteRange::try_from(Range {
                start: 8_u32,
                end: 3
            })
            .is_err()
        );
        assert!(
            ByteRange::try_from(Range {
                start: -8_i32,
                end: -3
            })
            .is_err()
        );
    }

    #[test]
    fn empty_and_maximum_endpoints_have_safe_lengths() {
        for endpoint in [0, usize::MAX] {
            let range = ByteRange::new(endpoint..endpoint).unwrap();
            assert_eq!(range.len(), 0);
            assert_eq!(range.as_range(), endpoint..endpoint);
        }
        assert_eq!(ByteRange::new(0..usize::MAX).unwrap().len(), usize::MAX);
        assert_eq!(ByteRange::try_from(3_u32..8).unwrap().len(), 5);
    }
}
