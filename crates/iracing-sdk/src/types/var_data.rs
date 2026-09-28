//! Variable data parsing trait and implementations
use super::VariableInfo;
use crate::{IRacingSDKError, Result, irsdk::VariableType};

use iracing_irsdk::{
    BitField, BroadcastMessage, CameraState, CameraSwitchFocusMode, CarLeftRight, ChatCommandMode,
    EngineWarnings, ForceFeedbackCommandMode, IncidentFlags, PaceFlags, PaceMode, PitCommandMode,
    PitServiceFlags, PitServiceStatus, ReloadTexturesMode, ReplayPositionMode, ReplaySearchMode,
    ReplayStateMode, SessionFlags, SessionState, TelemetryCommandMode, TrackLocation, TrackSurface,
    TrackWetness, VideoCaptureMode,
};
use zerocopy::{FromBytes, TryFromBytes};

/// Trait for types that can be parsed from binary telemetry data.
pub trait VarData: Sized {
    /// Check whether this Rust type can decode a telemetry variable type.
    fn validate_variable_type(_data_type: VariableType) -> Result<()> {
        Ok(())
    }

    /// Decode `count` instaces of `VarData` from `bytes`.
    /// `bytes` is sized to the expected variable type.
    fn decode(bytes: &[u8], count: usize) -> Result<Self>;

    /// Parse this type from binary data at the given offset.
    fn from_bytes(data: &[u8], info: &VariableInfo) -> Result<Self> {
        let region = info.region();

        let bytes = data.get(region.as_range()).ok_or_else(|| {
            IRacingSDKError::parse_error(
                "VarData::from_bytes",
                "Variable region exceeds frame bounds",
            )
        })?;

        Self::decode(bytes, region.count())
    }
}

macro_rules! impl_scalar_from_bytes_var_data {
    ($($type:ty => $variable_type:ident),+ $(,)?) => {
        $(
            impl VarData for $type {
                fn validate_variable_type(data_type: VariableType) -> Result<()> {
                    if data_type != VariableType::$variable_type {
                        return Err(IRacingSDKError::type_conversion(
                            VariableType::$variable_type,
                            data_type,
                        ));
                    }
                    Ok(())
                }

                fn decode(bytes: &[u8], count: usize) -> Result<Self> {
                    if count != 1 {
                        return Err(IRacingSDKError::parse_error("VarData", "Expected a scalar"));
                    }

                    <$type as FromBytes>::read_from_bytes(bytes).map_err(|_| {
                        IRacingSDKError::WireSize {
                            expected: size_of::<$type>(),
                            actual: bytes.len(),
                        }
                    })
                }
            }
        )+
    };
}

impl_scalar_from_bytes_var_data!(
    f32 => Float,
    i32 => Integer,
    u32 => BitField,
    f64 => Double,
    u8 => Character,
);

/// Implementation of VarData over bool is a one-off due to it's usage of `TryFromBytes`.
impl VarData for bool {
    fn validate_variable_type(data_type: VariableType) -> Result<()> {
        if data_type != VariableType::Boolean {
            return Err(IRacingSDKError::type_conversion(
                VariableType::Boolean,
                data_type,
            ));
        }
        Ok(())
    }

    fn decode(bytes: &[u8], count: usize) -> Result<Self> {
        if count != 1 {
            return Err(IRacingSDKError::parse_error("VarData", "Expected a scalar"));
        }

        <bool as TryFromBytes>::try_read_from_bytes(bytes).map_err(|_| IRacingSDKError::WireSize {
            expected: size_of::<bool>(),
            actual: bytes.len(),
        })
    }
}

impl VarData for BitField {
    fn validate_variable_type(data_type: VariableType) -> Result<()> {
        <u32 as VarData>::validate_variable_type(data_type)
    }

    fn decode(bytes: &[u8], count: usize) -> Result<Self> {
        let raw = <u32 as VarData>::decode(bytes, count)?;
        Ok(Self::new(raw))
    }
}

macro_rules! impl_enum_var_data {
    ($($type:ty),+ $(,)?) => {$ (
        impl VarData for $type {
            fn validate_variable_type(data_type: VariableType) -> Result<()> {
                <i32 as VarData>::validate_variable_type(data_type)
            }

            fn decode(bytes: &[u8], count: usize) -> crate::Result<Self> {
                // Decode as an i32
                let raw = <i32 as VarData>::decode(bytes, count)?;

                // Try to instantiate the enum from the raw value
                Self::try_from(raw).map_err(|raw| {
                    IRacingSDKError::parse_error(
                        concat!("unknown ", stringify!($type), " value"),
                        raw.to_string(),
                    )
                })
            }
        }
    )+};
}

impl_enum_var_data!(
    BroadcastMessage,
    CameraSwitchFocusMode,
    CarLeftRight,
    ChatCommandMode,
    ForceFeedbackCommandMode,
    PaceMode,
    PitCommandMode,
    PitServiceStatus,
    ReloadTexturesMode,
    ReplayPositionMode,
    ReplaySearchMode,
    ReplayStateMode,
    SessionState,
    TelemetryCommandMode,
    TrackLocation,
    TrackSurface,
    TrackWetness,
    VideoCaptureMode,
);

macro_rules! impl_bitmask_var_data {
    ($($type:ty),+ $(,)?) => {$ (
        impl VarData for $type {
            fn validate_variable_type(data_type: VariableType) -> Result<()> {
                <u32 as VarData>::validate_variable_type(data_type)
            }

            fn decode(bytes: &[u8], count: usize) -> Result<Self> {
                let raw = <u32 as VarData>::decode(bytes, count)?;
                Ok(Self::from(raw))
            }
        }
    )+};
}

impl_bitmask_var_data!(
    CameraState,
    EngineWarnings,
    PaceFlags,
    PitServiceFlags,
    SessionFlags,
);

impl VarData for IncidentFlags {
    fn validate_variable_type(data_type: VariableType) -> Result<()> {
        if matches!(data_type, VariableType::BitField | VariableType::Integer) {
            Ok(())
        } else {
            Err(IRacingSDKError::type_conversion(
                VariableType::BitField,
                data_type,
            ))
        }
    }

    fn decode(bytes: &[u8], count: usize) -> Result<Self> {
        let raw = <u32 as VarData>::decode(bytes, count)?;
        Ok(Self::from(raw))
    }
}

impl<T: VarData> VarData for Vec<T> {
    fn validate_variable_type(data_type: VariableType) -> Result<()> {
        T::validate_variable_type(data_type)
    }

    fn decode(bytes: &[u8], count: usize) -> Result<Self> {
        if count == 0 || !bytes.len().is_multiple_of(count) {
            return Err(IRacingSDKError::parse_error(
                "Vec<T>",
                "Invalid array dimensions",
            ));
        }

        let element_size = bytes.len() / count;

        if element_size == 0 {
            return Err(IRacingSDKError::parse_error(
                "Vec<T>",
                "Element size cannot be zero",
            ));
        }

        bytes
            .chunks_exact(element_size)
            .map(|chunk| T::decode(chunk, 1))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irsdk::{VariableHeader, VariableType};
    use std::fmt::Debug;

    fn variable_info(data_type: VariableType, offset: i32, count: i32) -> VariableInfo {
        let header = VariableHeader::new(data_type, offset, count, false, "test", "", "").unwrap();
        let frame_size = offset as usize + data_type.byte_size() * count as usize;
        VariableInfo::try_from_header(&header, frame_size).unwrap()
    }

    fn assert_array_pair<T: VarData + Debug + PartialEq>(
        data_type: VariableType,
        first: &[u8],
        second: &[u8],
        expected: [T; 2],
    ) {
        let mut data = vec![0xAA];
        data.extend_from_slice(first);
        data.extend_from_slice(second);

        let info = variable_info(data_type, 1, 2);
        assert_eq!(Vec::<T>::from_bytes(&data, &info).unwrap(), expected);
    }

    #[test]
    fn fixed_width_scalars_decode_little_endian_at_an_offset() {
        assert_eq!(
            f32::from_bytes(
                &[0, 0, 0, 0x20, 0x41],
                &variable_info(VariableType::Float, 1, 1)
            )
            .unwrap(),
            10.0
        );
        assert_eq!(
            i32::from_bytes(
                &[0, 0x78, 0x56, 0x34, 0x12],
                &variable_info(VariableType::Integer, 1, 1)
            )
            .unwrap(),
            0x1234_5678
        );
        assert!(bool::from_bytes(&[0, 1], &variable_info(VariableType::Boolean, 1, 1)).unwrap());
        assert_eq!(
            BitField::from_bytes(
                &[0, 0x78, 0x56, 0x34, 0x12],
                &variable_info(VariableType::BitField, 1, 1),
            )
            .unwrap()
            .value(),
            0x1234_5678
        );
        assert_eq!(
            f64::from_bytes(
                &[0, 0, 0, 0, 0, 0, 0, 0x24, 0x40],
                &variable_info(VariableType::Double, 1, 1),
            )
            .unwrap(),
            10.0
        );
    }

    #[test]
    fn enum_and_bitmask_wire_types_still_decode_through_var_data() {
        assert_eq!(
            SessionState::from_bytes(
                &i32::from(SessionState::Racing).to_le_bytes(),
                &variable_info(VariableType::Integer, 0, 1),
            )
            .unwrap(),
            SessionState::Racing
        );
        assert_eq!(
            SessionFlags::from_bytes(
                &SessionFlags::GREEN.bits().to_le_bytes(),
                &variable_info(VariableType::BitField, 0, 1),
            )
            .unwrap(),
            SessionFlags::GREEN
        );
    }

    #[test]
    fn incident_flags_accept_bitfield_and_integer_storage() {
        const RAW: u32 = 0x8000_0408;
        for data_type in [VariableType::BitField, VariableType::Integer] {
            let decoded =
                IncidentFlags::from_bytes(&RAW.to_le_bytes(), &variable_info(data_type, 0, 1))
                    .unwrap();
            assert_eq!(decoded.bits(), RAW);
        }
    }

    #[test]
    fn decoding_uses_requested_type_and_region_width() {
        let info = variable_info(VariableType::Integer, 1, 1);
        assert_eq!(u32::from_bytes(&[0, 1, 0, 0, 0], &info).unwrap(), 1);
        assert!(matches!(
            u8::from_bytes(&[0, 1, 0, 0, 0], &info),
            Err(IRacingSDKError::WireSize {
                expected: 1,
                actual: 4
            })
        ));
    }

    #[test]
    fn truncated_frame_reports_region_bounds_error() {
        for info in [
            variable_info(VariableType::Float, 3, 1),
            variable_info(VariableType::Double, 3, 1),
            variable_info(VariableType::Character, 3, 2),
        ] {
            assert!(matches!(
                u8::from_bytes(&[], &info),
                Err(IRacingSDKError::Parse { context, .. }) if context == "VarData::from_bytes"
            ));
        }
    }

    #[test]
    fn character_decoding_preserves_raw_byte_values() {
        assert_eq!(
            u8::from_bytes(&[0xAA, 0], &variable_info(VariableType::Character, 1, 1)).unwrap(),
            0
        );
        assert_eq!(
            u8::from_bytes(&[0xAA, 0xFF], &variable_info(VariableType::Character, 1, 1)).unwrap(),
            0xFF
        );
    }

    #[test]
    fn arrays_decode_every_storage_type_at_a_nonzero_offset() {
        assert_array_pair(VariableType::Character, &[0], &[0xFF], [0_u8, 0xFF]);
        assert_array_pair(VariableType::Boolean, &[0], &[1], [false, true]);
        assert_array_pair(
            VariableType::Integer,
            &(-2_i32).to_le_bytes(),
            &0x1234_5678_i32.to_le_bytes(),
            [-2_i32, 0x1234_5678],
        );
        assert_array_pair(
            VariableType::Float,
            &(-1.5_f32).to_le_bytes(),
            &10.25_f32.to_le_bytes(),
            [-1.5_f32, 10.25_f32],
        );
        assert_array_pair(
            VariableType::Double,
            &(-1.5_f64).to_le_bytes(),
            &10.25_f64.to_le_bytes(),
            [-1.5_f64, 10.25_f64],
        );
        assert_array_pair(
            VariableType::BitField,
            &0x8000_0000_u32.to_le_bytes(),
            &0x1234_5678_u32.to_le_bytes(),
            [BitField::new(0x8000_0000), BitField::new(0x1234_5678)],
        );
    }

    #[test]
    fn arrays_report_element_width_and_frame_bounds_errors() {
        let info = variable_info(VariableType::Integer, 1, 2);
        assert!(matches!(
            Vec::<u8>::from_bytes(&[0; 9], &info),
            Err(IRacingSDKError::WireSize {
                expected: 1,
                actual: 4
            })
        ));

        let info = variable_info(VariableType::Character, 1, 2);
        assert!(matches!(
            Vec::<u8>::from_bytes(&[0xAA, 42], &info),
            Err(IRacingSDKError::Parse { context, .. }) if context == "VarData::from_bytes"
        ));

        let info = variable_info(VariableType::Float, 1, 2);
        let mut truncated = vec![0xAA];
        truncated.extend_from_slice(&1.5_f32.to_le_bytes());
        truncated.extend_from_slice(&[0, 0]);
        assert!(matches!(
            Vec::<f32>::from_bytes(&truncated, &info),
            Err(IRacingSDKError::Parse { context, .. }) if context == "VarData::from_bytes"
        ));
    }

    #[test]
    fn zero_count_array_is_rejected() {
        assert!(matches!(
            Vec::<u8>::decode(&[], 0),
            Err(IRacingSDKError::Parse { context, .. }) if context == "Vec<T>"
        ));
        assert!(VariableHeader::new(VariableType::Character, 0, 0, false, "test", "", "").is_err());
    }

    #[test]
    fn invalid_boolean_byte_is_rejected() {
        assert!(bool::from_bytes(&[2], &variable_info(VariableType::Boolean, 0, 1)).is_err());
    }
}
