//! Telemetry variable schema types

#[cfg(feature = "codegen")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::{
    IRacingSDKError, Result, VariableRegion,
    irsdk::{VariableHeader, VariableType as IRSDKVariableType},
    parse_utils,
};

use super::variable_headers_buffer::VariableHeadersBuffer;

fn schema_validation_error(details: impl Into<String>) -> IRacingSDKError {
    IRacingSDKError::parse_error("Schema validation", details)
}

/// # Variable info
/// Information about a specific telemetry variable.
#[cfg_attr(feature = "codegen", derive(JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariableInfo {
    /// # Name
    /// Variable name as defined by iRacing
    pub name: String,
    /// # Data type
    /// Data type of the variable
    #[cfg_attr(feature = "codegen", schemars(schema_with = "storage_type_schema"))]
    pub data_type: IRSDKVariableType,
    /// # Count as time
    /// Whether the simulator treats the sample count as elapsed time
    pub count_as_time: bool,
    /// # Units
    /// Units of measurement (e.g., "m/s", "C", "N*m")
    pub units: String,
    /// # Description
    /// Human-readable description
    pub description: String,

    region: VariableRegion,
}

impl VariableInfo {
    /// Builds variable metadata from an SDK header, validating its frame bounds.
    pub fn try_from_header(value: &VariableHeader, frame_size: usize) -> Result<Self> {
        let offset = usize::try_from(value.offset).map_err(|_| {
            IRacingSDKError::parse_error(
                "VariableInfo::try_from",
                format!("Could not convert {} to usize", value.offset),
            )
        })?;

        let count = usize::try_from(value.count).map_err(|_| {
            IRacingSDKError::parse_error(
                "VariableInfo::try_from",
                format!("Could not convert {} to usize", value.count,),
            )
        })?;

        Ok(VariableInfo {
            name: parse_utils::c_string_to_string(&value.name),
            description: parse_utils::c_string_to_string(&value.description),
            units: parse_utils::c_string_to_string(&value.unit),
            data_type: value.variable_type,
            count_as_time: value.count_as_time != 0,
            region: VariableRegion::try_new(
                offset,
                value.variable_type.byte_size(),
                count,
                frame_size,
            )?,
        })
    }

    /// Returns the validated byte region occupied by this variable.
    pub fn region(&self) -> VariableRegion {
        self.region
    }

    /// Returns the variable's byte offset within a frame.
    pub fn offset(&self) -> usize {
        self.region.offset()
    }

    /// Returns the number of elements in the variable.
    pub fn count(&self) -> usize {
        self.region.count()
    }
}

/// # Variable schema
/// Schema describing the structure and metadata of telemetry variables.
#[cfg_attr(feature = "codegen", derive(JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariableSchema {
    /// # Variables map
    /// Map of variable names to their metadata (provides O(1) lookup)
    pub variables: HashMap<String, VariableInfo>,
    /// # Frame size
    /// Total size of a telemetry frame in bytes
    pub frame_size: usize,
}

impl VariableSchema {
    /// Creates a schema from previously validated variable metadata.
    pub fn new(variables: HashMap<String, VariableInfo>, frame_size: usize) -> Self {
        Self {
            variables,
            frame_size,
        }
    }

    /// Constructs a schema from an exact snapshot of SDK variable headers.
    pub fn try_from_snapshot(snapshot: VariableHeadersBuffer, frame_size: usize) -> Result<Self> {
        Self::try_from_headers(snapshot.as_slice(), frame_size)
    }

    /// Constructs and validates a schema from decoded SDK variable headers.
    pub fn try_from_headers(headers: &[VariableHeader], frame_size: usize) -> Result<Self> {
        let mut variables = HashMap::with_capacity(headers.len());

        for header in headers.iter() {
            let variable = VariableInfo::try_from_header(header, frame_size)?;

            if variable.name.is_empty() {
                return Err(schema_validation_error("Variable header has empty name"));
            }

            if variables.contains_key(&variable.name) {
                return Err(schema_validation_error(format!(
                    "Duplicate variable name '{}' in header region",
                    variable.name
                )));
            }

            variables.insert(variable.name.clone(), variable);
        }

        Ok(Self::new(variables, frame_size))
    }

    /// Get variable info by name (O(1) lookup).
    pub fn get_variable(&self, name: &str) -> Option<&VariableInfo> {
        self.variables.get(name)
    }

    /// Check if a variable exists.
    pub fn has_variable(&self, name: &str) -> bool {
        self.variables.contains_key(name)
    }

    /// Get the number of variables.
    pub fn variable_count(&self) -> usize {
        self.variables.len()
    }

    /// Get the names of all available variables.
    pub fn variable_names(&self) -> Vec<String> {
        self.variables.keys().cloned().collect()
    }

    /// Get all available variables in the schema.
    pub fn variables(&self) -> Vec<VariableInfo> {
        self.variables.values().cloned().collect()
    }
}

/// Provider abstraction for schema discovery across telemetry sources.
///
/// This trait enables consumers to work with any telemetry source (live iRacing,
/// IBT files, or test data) by abstracting schema access.
pub trait SchemaProvider {
    /// Get the variable schema for this telemetry source.
    fn schema(&self) -> &VariableSchema;

    /// Get variable information for a field name.
    fn variable(&self, name: &str) -> Option<&VariableInfo> {
        self.schema().get_variable(name)
    }

    /// Check if a field exists in the schema.
    fn has_variable(&self, name: &str) -> bool {
        self.schema().has_variable(name)
    }

    /// Get all available field names in this schema.
    fn variable_names(&self) -> Vec<String> {
        self.schema().variable_names()
    }

    /// Get all available variable values.
    fn variables(&self) -> Vec<VariableInfo> {
        self.schema().variables()
    }

    /// The number of variables in the schema.
    fn variable_count(&self) -> usize {
        self.schema().variable_count()
    }
}

#[cfg(feature = "codegen")]
fn storage_type_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "enum": ["Character", "Boolean", "Integer", "BitField", "Float", "Double"]
    })
}

#[cfg(test)]
mod tests {
    use zerocopy::IntoBytes;

    use super::*;
    use crate::irsdk::VariableType as IRSDKVariableType;

    #[cfg(feature = "codegen")]
    #[test]
    fn metadata_schema_only_advertises_storage_types() {
        let schema = schemars::schema_for!(VariableInfo);
        let value = serde_json::to_value(schema).unwrap();
        assert_eq!(
            value["properties"]["data_type"]["enum"],
            serde_json::json!([
                "Character",
                "Boolean",
                "Integer",
                "BitField",
                "Float",
                "Double"
            ])
        );
    }

    #[test]
    fn constructs_schema_from_variable_headers_buffer() {
        let header = VariableHeader::new(
            IRSDKVariableType::Float,
            4,
            1,
            false,
            "Speed",
            "Vehicle speed",
            "m/s",
        )
        .unwrap();

        let bytes = header.as_bytes();
        let headers = VariableHeadersBuffer::try_from_region_bytes(bytes, 1).unwrap();

        let schema = VariableSchema::try_from_snapshot(headers, 8).unwrap();

        let speed = schema.get_variable("Speed").unwrap();
        assert_eq!(speed.offset(), 4);
        assert_eq!(speed.count(), 1);
        assert_eq!(schema.frame_size, 8);
    }
}
