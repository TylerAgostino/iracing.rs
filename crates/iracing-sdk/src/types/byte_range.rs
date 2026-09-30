use crate::IRacingSDKError;
use std::ops::Range;

#[derive(Debug, Clone)]
pub(crate) struct ByteRange {
    range: Range<usize>,
}

impl ByteRange {
    pub fn new(range: Range<usize>) -> Self {
        Self { range }
    }

    pub fn as_range(&self) -> Range<usize> {
        self.range.clone()
    }

    pub fn len(&self) -> usize {
        self.range.end - self.range.start
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

                Ok(Self::new(start..end))
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
        assert!(ByteRange::try_from(0..-1).is_err());
    }
}
