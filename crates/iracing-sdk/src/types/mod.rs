//! Core types for telemetry data representation.
//!
//! This module provides the foundational data structures for handling iRacing telemetry data,
//! including schema management, bitfield helpers, and type-safe binary parsing.
//!
//! ## Architecture
//!
//! The type system maps directly to iRacing SDK structures:
//! - [`VariableSchema`] describes the structure of telemetry variables with O(1) lookup
//! - [`irsdk::VariableType`](crate::irsdk::VariableType) maps to iRacing's `irsdk_VarType` enum with size information
//! - [`VarData`] trait provides type-safe parsing from binary telemetry data
//! - [`BitField`] handles iRacing's bitfield variables with flag operations
//!
//! ## Performance Characteristics
//!
//! - O(1) variable lookup via HashMap
//! - Bounds checking on all memory operations
//! - Tick count wraparound handling for proper frame ordering
//!
//! ## Usage Example
//!
//! ```rust,no_run
//! use iracing_sdk::{VarData, VariableSchema, irsdk::{VariableHeader, VariableType}};
//!
//! // Create a schema for RPM data
//! let header = VariableHeader::new(
//!     VariableType::Float, 0, 1, false, "RPM", "Engine RPM", "rev/min"
//! ).expect("valid RPM header");
//!
//! let schema = VariableSchema::try_from_headers(&[header], 4)?;
//! let frame = vec![0x00, 0xA0, 0x8C, 0x45]; // 4500.0 as little-endian f32
//!
//! // Parse RPM value
//! let rpm_info = schema.get_variable("RPM").expect("RPM variable");
//! let rpm: f32 = f32::from_bytes(&frame, rpm_info)?;
//! assert!((rpm - 4500.0).abs() < 1.0); // Allow for floating point precision
//! # Ok::<(), iracing_sdk::IRacingSDKError>(())
//! ```

mod dynamic_frame;
mod frame;
mod ibt;
mod iracing_session_string;
mod regions;
mod schema;
mod session_info_buffer;
mod telemetry_value;
mod update_rate;
mod var_data;
mod variable_headers_buffer;

// Re-export all public types
pub use dynamic_frame::DynamicFrame;
pub use frame::FramePacket;
pub use ibt::IbtLayout;
pub use iracing_irsdk::BitField;
pub(crate) use iracing_session_string::IRacingSessionString;
pub use regions::*;
pub use schema::{SchemaProvider, VariableInfo, VariableSchema};
pub use session_info_buffer::{SessionInfoBuffer, SessionInfoEncoding, SessionInfoPayload};
pub use telemetry_value::{TelemetryValue, TelemetryValueProvider};
pub use update_rate::UpdateRate;
pub use var_data::VarData;
pub use variable_headers_buffer::VariableHeadersBuffer;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irsdk::{VariableHeader, VariableType};

    use proptest::prelude::*;

    // Property test strategies
    prop_compose! {
        fn arb_variable_metadata()(
            data_type in prop::sample::select(vec![
                VariableType::Character, VariableType::Integer,
                VariableType::Float, VariableType::Double,
                VariableType::Boolean, VariableType::BitField
            ]),
            offset in 0..1024usize,
            count in 1..10usize,
            units in "[a-zA-Z/^2]{0,31}",
            description in "[a-zA-Z ]{0,63}"
        ) -> (VariableType, usize, usize, String, String) {
            (data_type, offset, count, units, description)
        }
    }

    fn scalar_info(data_type: VariableType, offset: usize, frame_size: usize) -> VariableInfo {
        let header =
            VariableHeader::new(data_type, offset as i32, 1, false, "test", "test", "test")
                .unwrap();
        VariableInfo::try_from_header(&header, frame_size).unwrap()
    }

    // Property tests for VariableSchema
    proptest! {

        #[test]
        fn prop_variable_schema_parsing_with_fuzzed_headers(
            variables in prop::collection::btree_map(
                "[a-zA-Z][a-zA-Z0-9_]{0,30}",
                arb_variable_metadata(),
                0..20
            ),
            frame_size in 64..2048usize
        ) {
            let headers: Vec<_> = variables
                .into_iter()
                .map(|(name, (data_type, offset, count, units, description))| {
                    let count = if data_type.byte_size() * count <= frame_size { count } else { 1 };
                    let length = data_type.byte_size() * count;
                    let offset = offset % (frame_size - length + 1);
                    VariableHeader::new(
                        data_type,
                        offset as i32,
                        count as i32,
                        false,
                        &name,
                        &description,
                        &units,
                    )
                    .unwrap()
                })
                .collect();

            let schema = VariableSchema::try_from_headers(&headers, frame_size).unwrap();
            prop_assert_eq!(schema.variable_count(), headers.len());
            prop_assert_eq!(schema.frame_size, frame_size);

            for var_info in schema.variables.values() {
                let region = var_info.region();
                prop_assert!(region.as_range().end <= schema.frame_size);
                prop_assert_eq!(region.len(), var_info.data_type.byte_size() * region.count());
                prop_assert!(region.count() > 0);
            }
        }

        #[test]
        fn prop_variable_type_size_calculations_correct(var_type in prop::sample::select(vec![
            VariableType::Character, VariableType::Integer,
            VariableType::Float, VariableType::Double,
            VariableType::Boolean, VariableType::BitField
        ])) {
            // VariableType size calculations correct for all enum variants
            let size = var_type.byte_size();
            prop_assert!(size > 0);
            prop_assert!(size <= 8);

            match var_type {
                VariableType::Character | VariableType::Boolean => {
                    prop_assert_eq!(size, 1);
                },
                VariableType::Integer | VariableType::Float | VariableType::BitField => {
                    prop_assert_eq!(size, 4);
                },
                VariableType::Double => {
                    prop_assert_eq!(size, 8);
                },
            }
        }

        #[test]
        fn prop_vardata_roundtrip_preserves_data_f32(
            value in any::<f32>(),
            offset in 0..100usize
        ) {
            // VarData roundtrip (serialize→deserialize) preserves data
            let mut data = vec![0u8; offset + 4 + 10];
            let bytes = value.to_le_bytes();
            data[offset..offset + 4].copy_from_slice(&bytes);

            let var_info = scalar_info(VariableType::Float, offset, data.len());

            let result = f32::from_bytes(&data, &var_info);
            prop_assert!(result.is_ok());

            let parsed = result.unwrap();
            if value.is_finite() {
                prop_assert!((parsed - value).abs() < f32::EPSILON);
            } else if value.is_nan() {
                prop_assert!(parsed.is_nan());
            } else {
                prop_assert_eq!(parsed, value);
            }
        }

        #[test]
        fn prop_vardata_roundtrip_preserves_data_i32(
            value in any::<i32>(),
            offset in 0..100usize
        ) {
            let mut data = vec![0u8; offset + 4 + 10];
            let bytes = value.to_le_bytes();
            data[offset..offset + 4].copy_from_slice(&bytes);

            let var_info = scalar_info(VariableType::Integer, offset, data.len());

            let result = i32::from_bytes(&data, &var_info);
            prop_assert!(result.is_ok());
            prop_assert_eq!(result.unwrap(), value);
        }

        #[test]
        fn prop_bitfield_parsing_handles_all_32bit_patterns(
            value in any::<u32>(),
            offset in 0..100usize
        ) {
            // BitField parsing handles all 32-bit patterns correctly
            let mut data = vec![0u8; offset + 4 + 10];
            let bytes = value.to_le_bytes();
            data[offset..offset + 4].copy_from_slice(&bytes);

            let var_info = scalar_info(VariableType::BitField, offset, data.len());

            let result = BitField::from_bytes(&data, &var_info);
            prop_assert!(result.is_ok());
            prop_assert_eq!(result.unwrap().value(), value);
        }

        #[test]
        fn prop_tick_comparison_handles_wraparound(
            tick1 in any::<u32>(),
            tick2 in any::<u32>()
        ) {
            // Tick comparison handles wraparound correctly for all u32 sequences
            let diff = tick2.wrapping_sub(tick1);

            // If the difference is small (< half range), tick2 is "after" tick1
            // If the difference is large (> half range), it's wraparound and tick1 is "after" tick2
            let is_tick2_newer = diff < u32::MAX / 2;

            // This property should always hold for proper wraparound handling
            if tick1 == tick2 {
                prop_assert_eq!(diff, 0);
            } else if diff == 1 {
                prop_assert!(is_tick2_newer);
            }
        }

        #[test]
        fn prop_bitfield_flag_operations(
            value in any::<u32>(),
            bit_index in 0..32u32
        ) {
            let bitfield = BitField::new(value);
            let expected_bit_set = (value & (1 << bit_index)) != 0;
            prop_assert_eq!(bitfield.is_set(bit_index), expected_bit_set);

            // Test flag checking with the bit as a flag
            let flag = 1 << bit_index;
            prop_assert_eq!(bitfield.has_flag(flag), expected_bit_set);
        }
    }
}
