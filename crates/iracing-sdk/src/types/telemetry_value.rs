use serde::{Deserialize, Serialize};

use crate::{BitField, Result, VarData, VariableInfo, irsdk::VariableType};

/// Runtime value type that can hold any telemetry data.
///
/// SDK decoding produces scalar values or arrays of those values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TelemetryValue {
    /// An 8-bit character value (`irsdk_char`).
    Char(u8),
    /// A 32-bit signed integer (`irsdk_int`).
    Int32(i32),
    /// A 32-bit IEEE 754 floating-point value (`irsdk_float`).
    Float32(f32),
    /// A 64-bit IEEE 754 floating-point value (`irsdk_double`).
    Float64(f64),
    /// A boolean value (`irsdk_bool`).
    Bool(bool),
    /// A 32-bit bitfield (`irsdk_bitField`).
    BitField(super::BitField),
    /// An array of homogeneous telemetry values (multi-element variables).
    Array(Vec<TelemetryValue>),
}

impl TelemetryValue {
    /// Decodes the variable described by `info` from a complete telemetry frame.
    ///
    /// `info.offset()` is relative to the start of `data`. A count of one
    /// produces a scalar; a larger count produces an [`Self::Array`].
    ///
    /// # Errors
    ///
    /// Returns an error if the requested bytes are outside `data` or cannot be
    /// decoded as the storage type described by `info`.
    pub fn decode(data: &[u8], info: &VariableInfo) -> Result<Self> {
        match info.data_type {
            VariableType::Character => Self::decode_typed::<u8>(data, info, Self::Char),
            VariableType::BitField => Self::decode_typed::<BitField>(data, info, Self::BitField),
            VariableType::Boolean => Self::decode_typed::<bool>(data, info, Self::Bool),
            VariableType::Integer => Self::decode_typed::<i32>(data, info, Self::Int32),
            VariableType::Float => Self::decode_typed::<f32>(data, info, Self::Float32),
            VariableType::Double => Self::decode_typed::<f64>(data, info, Self::Float64),
        }
    }

    fn decode_typed<T: VarData>(
        data: &[u8],
        info: &VariableInfo,
        wrap: fn(T) -> Self,
    ) -> Result<Self> {
        if info.count() == 1 {
            T::from_bytes(data, info).map(wrap)
        } else {
            Vec::<T>::from_bytes(data, info)
                .map(|values| Self::Array(values.into_iter().map(wrap).collect()))
        }
    }
}

/// Decodes telemetry values using their variable metadata.
///
/// Implementors provide access to the raw data for a telemetry frame while
/// callers supply the corresponding [`VariableInfo`].
pub trait TelemetryValueProvider {
    /// Decodes the telemetry value described by `info`.
    fn telemetry_value(&self, info: &VariableInfo) -> Result<TelemetryValue>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irsdk::VariableHeader;

    fn info(data_type: VariableType, offset: usize, count: usize) -> VariableInfo {
        let header = VariableHeader::new(
            data_type,
            i32::try_from(offset).unwrap(),
            i32::try_from(count).unwrap(),
            false,
            "test",
            "",
            "",
        )
        .unwrap();
        VariableInfo::try_from_header(&header, offset + data_type.byte_size() * count).unwrap()
    }

    #[test]
    fn dispatches_every_storage_type_for_scalars_and_arrays() {
        let cases = [
            (
                VariableType::Character,
                vec![0, 0xFF],
                [TelemetryValue::Char(0), TelemetryValue::Char(0xFF)],
            ),
            (
                VariableType::Boolean,
                vec![0, 1],
                [TelemetryValue::Bool(false), TelemetryValue::Bool(true)],
            ),
            (
                VariableType::Integer,
                [(-2_i32).to_le_bytes(), 123_i32.to_le_bytes()].concat(),
                [TelemetryValue::Int32(-2), TelemetryValue::Int32(123)],
            ),
            (
                VariableType::BitField,
                [1_u32.to_le_bytes(), 0x8000_0000_u32.to_le_bytes()].concat(),
                [
                    TelemetryValue::BitField(BitField::new(1)),
                    TelemetryValue::BitField(BitField::new(0x8000_0000)),
                ],
            ),
            (
                VariableType::Float,
                [(-1.5_f32).to_le_bytes(), 10.25_f32.to_le_bytes()].concat(),
                [
                    TelemetryValue::Float32(-1.5),
                    TelemetryValue::Float32(10.25),
                ],
            ),
            (
                VariableType::Double,
                [(-1.5_f64).to_le_bytes(), 10.25_f64.to_le_bytes()].concat(),
                [
                    TelemetryValue::Float64(-1.5),
                    TelemetryValue::Float64(10.25),
                ],
            ),
        ];

        for (data_type, bytes, expected) in cases {
            let mut frame = vec![0xAA];
            frame.extend_from_slice(&bytes);

            let scalar = info(data_type, 1, 1);
            assert_eq!(
                TelemetryValue::decode(&frame, &scalar).unwrap(),
                expected[0],
                "scalar {data_type:?}"
            );

            let array = info(data_type, 1, 2);
            assert_eq!(
                TelemetryValue::decode(&frame, &array).unwrap(),
                TelemetryValue::Array(expected.to_vec()),
                "array {data_type:?}"
            );
        }
    }

    #[test]
    fn propagates_var_data_errors() {
        let info = info(VariableType::Boolean, 1, 2);
        assert!(TelemetryValue::decode(&[0xAA, 0, 2], &info).is_err());
        assert!(TelemetryValue::decode(&[0xAA, 0], &info).is_err());
    }
}
