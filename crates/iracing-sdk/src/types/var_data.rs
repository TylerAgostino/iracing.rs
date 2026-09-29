//! Variable data parsing trait and implementations
use super::VariableInfo;
use crate::{IRacingSDKError, Result, parse_utils::decode_variable_type};
use iracing_irsdk::{
    BitField, BroadcastMessage, CameraState, CameraSwitchFocusMode, CarLeftRight, ChatCommandMode,
    EngineWarnings, ForceFeedbackCommandMode, IncidentFlags, PaceFlags, PaceMode, PitCommandMode,
    PitServiceFlags, PitServiceStatus, ReloadTexturesMode, ReplayPositionMode, ReplaySearchMode,
    ReplayStateMode, SessionFlags, SessionState, TelemetryCommandMode, TrackLocation, TrackSurface,
    TrackWetness, VideoCaptureMode,
};

/// Trait for types that can be parsed from binary telemetry data.
pub trait VarData: Sized {
    /// Parse this type from binary data at the given offset.
    fn from_bytes(data: &[u8], info: &VariableInfo) -> Result<Self>;
}

/// irsdk::VariableType::Float
impl VarData for f32 {
    fn from_bytes(data: &[u8], info: &VariableInfo) -> Result<Self> {
        decode_variable_type!(data, info, Float, f32::from_le_bytes)
    }
}

/// irsdk::VariableType::Integer
impl VarData for i32 {
    fn from_bytes(data: &[u8], info: &VariableInfo) -> Result<Self> {
        decode_variable_type!(data, info, Integer, i32::from_le_bytes)
    }
}

/// irsdk::VariableType::Bool
impl VarData for bool {
    fn from_bytes(data: &[u8], info: &VariableInfo) -> Result<Self> {
        decode_variable_type!(data, info, Boolean, |[byte]| byte != 0)
    }
}

/// irsdk::VariableType::BitField
impl VarData for BitField {
    fn from_bytes(data: &[u8], info: &VariableInfo) -> Result<Self> {
        decode_variable_type!(data, info, BitField, |bytes| {
            BitField(u32::from_le_bytes(bytes))
        })
    }
}

/// irsdk::VariableType::Character
impl VarData for u8 {
    fn from_bytes(data: &[u8], info: &VariableInfo) -> Result<Self> {
        decode_variable_type!(data, info, Character, |[byte]| byte)
    }
}

/// irsdk::VariableType::Double
impl VarData for f64 {
    fn from_bytes(data: &[u8], info: &VariableInfo) -> Result<Self> {
        decode_variable_type!(data, info, Double, f64::from_le_bytes)
    }
}

// Array support for VarData
impl<T: VarData> VarData for Vec<T> {
    fn from_bytes(data: &[u8], info: &VariableInfo) -> Result<Self> {
        let scalar = info.scalar_at_start();
        if let Err(error @ IRacingSDKError::TypeConversion { .. }) = T::from_bytes(&[], &scalar) {
            return Err(error);
        }
        let bytes = data.get(info.region().as_range()).ok_or_else(|| {
            IRacingSDKError::memory_unexpected_eof(
                info.offset(),
                info.region().as_region().end(),
                data.len(),
            )
        })?;
        bytes
            .chunks_exact(info.data_type.byte_size())
            .map(|chunk| T::from_bytes(chunk, &scalar))
            .collect()
    }
}

macro_rules! impl_enum_var_data {
    ($($type:ty),+ $(,)?) => {$ (
        impl VarData for $type {
            fn from_bytes(data: &[u8], info: &VariableInfo) -> crate::Result<Self> {
                let raw = <i32 as VarData>::from_bytes(data, info)?;
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
            fn from_bytes(data: &[u8], info: &VariableInfo) -> crate::Result<Self> {
                if info.data_type != iracing_irsdk::VariableType::BitField {
                    return Err(IRacingSDKError::type_conversion("BitField", info.data_type));
                }

                <BitField as VarData>::from_bytes(data, info).map(Self::from)
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
    fn from_bytes(data: &[u8], info: &VariableInfo) -> crate::Result<Self> {
        match info.data_type {
            iracing_irsdk::VariableType::BitField => {
                <BitField as VarData>::from_bytes(data, info).map(Self::from)
            }
            iracing_irsdk::VariableType::Integer => {
                <i32 as VarData>::from_bytes(data, info).map(|value| Self::from(value as u32))
            }
            actual => Err(IRacingSDKError::type_conversion(
                "BitField or Int32",
                actual,
            )),
        }
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
        assert!(bool::from_bytes(&[0, 2], &variable_info(VariableType::Boolean, 1, 1)).unwrap());
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
        assert_array_pair(VariableType::Boolean, &[0], &[2], [false, true]);
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
        assert!(Vec::<u8>::from_bytes(&[0; 9], &info).is_err());

        let info = variable_info(VariableType::Character, 1, 2);
        assert!(matches!(
            Vec::<u8>::from_bytes(&[0xAA, 42], &info),
            Err(IRacingSDKError::Memory { .. })
        ));

        let info = variable_info(VariableType::Float, 1, 2);
        let mut truncated = vec![0xAA];
        truncated.extend_from_slice(&1.5_f32.to_le_bytes());
        truncated.extend_from_slice(&[0, 0]);
        assert!(matches!(
            Vec::<f32>::from_bytes(&truncated, &info),
            Err(IRacingSDKError::Memory { .. })
        ));
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
    fn nonzero_boolean_byte_is_true() {
        assert!(bool::from_bytes(&[2], &variable_info(VariableType::Boolean, 0, 1)).unwrap());
    }
}
