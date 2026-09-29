use serde::{Deserialize, Serialize};

use crate::{BitField, Result, VarData, VariableInfo, irsdk::VariableType};

/// Runtime value type that can hold any telemetry data.
///
/// SDK decoding produces only `Char`, `Bool`, `Int32`, `BitField`, `Float32`,
/// `Float64`, and arrays. Other integer variants remain available for callers
/// constructing values directly or reading previously serialized values.
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
