use crate::encoding::{Ascii, GbkEncoding, Unicode};
use crate::{Date, FieldInfo, FieldName, FieldValue, File, Record, TableWriterBuilder};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyTuple};
use std::convert::TryFrom;

const NUMERIC_PRECISION: f64 = 1e-8;

/// DBFFile: Main structure for Python bindings to dbase-rs
/// Provides interface for DBF file operations with Python integration
#[cfg(feature = "python")]
#[pyclass]
pub struct DBFFile {
    // Path to the DBF file
    path: String,
    // Optional encoding specification
    encoding: Option<String>,
}

#[cfg(feature = "python")]
#[pymethods]
impl DBFFile {
    /// Creates a new DBFFile instance
    ///
    /// # Arguments
    /// * `path` - Path to the DBF file
    /// * `encoding` - Optional encoding specification (ascii, utf8, gbk)
    #[new]
    fn new(path: String, encoding: Option<String>) -> PyResult<Self> {
        Ok(Self { path, encoding })
    }

    /// Creates a new DBF file with specified fields
    ///
    /// # Arguments
    /// * `fields` - Vector of field definitions (name, type, length, decimal)
    fn create(&mut self, fields: Vec<(String, String, usize, Option<usize>)>) -> PyResult<()> {
        // Initialize builder with potential capacity hint
        let mut builder = TableWriterBuilder::new();

        // Process each field definition
        for (name, type_str, length, decimal) in fields {
            // Convert field name - involves allocation
            let field_name = FieldName::try_from(name.as_str())
                .map_err(|e| PyValueError::new_err(e.to_string()))?;

            // Convert length - no allocation
            let length_u8 = u8::try_from(length)
                .map_err(|_| PyValueError::new_err("Field length too large"))?;

            // Match field type and build field definition
            let result = match type_str.as_str() {
                "C" => builder.add_character_field(field_name, length_u8),
                "N" => {
                    let decimal_u8 = decimal.unwrap_or(0) as u8;
                    builder.add_numeric_field(field_name, length_u8, decimal_u8)
                }
                "L" => builder.add_logical_field(field_name),
                "D" => builder.add_date_field(field_name),
                "F" => {
                    let decimal_u8 = decimal.unwrap_or(8) as u8;
                    builder.add_float_field(field_name, length_u8, decimal_u8)
                }
                "Y" => builder.add_currency_field(field_name),
                "T" => builder.add_datetime_field(field_name),
                "I" => builder.add_integer_field(field_name),
                "B" => builder.add_double_field(field_name),
                _ => {
                    return Err(PyValueError::new_err(format!(
                        "Unsupported field type: {}",
                        type_str
                    )))
                }
            };
            builder = result;
        }

        // Set encoding - could be optimized with static encoding types
        if let Some(encoding) = &self.encoding {
            match encoding.as_str() {
                "ascii" => builder = builder.set_encoding(Ascii),
                "unicode" | "utf8" | "utf-8" => builder = builder.set_encoding(Unicode),
                "cp936" | "gbk" => builder = builder.set_encoding(GbkEncoding),
                // TODO: Add more encodings
                _ => {
                    return Err(PyValueError::new_err(format!(
                        "Unsupported encoding: {}",
                        encoding
                    )))
                }
            }
        }

        // Create file and handle errors
        match builder.build_with_file_dest(&self.path) {
            Ok(_) => Ok(()),
            Err(e) => Err(PyValueError::new_err(e.to_string())),
        }
    }

    /// Appends multiple records to the DBF file
    fn append_records(&self, records: &PyList) -> PyResult<()> {
        let mut dbf_file = match File::open_read_write(&self.path) {
            Ok(file) => file,
            Err(e) => return Err(PyValueError::new_err(e.to_string())),
        };

        let fields = dbf_file.fields();
        // TODO: Implement streaming records
        let rust_records = self.convert_py_records_to_rust(records, fields)?;

        match dbf_file.append_records(&rust_records) {
            Ok(_) => Ok(()),
            Err(e) => Err(PyValueError::new_err(e.to_string())),
        }
    }

    // Read all records from the DBF file
    // Args:
    //     py: Python - The Python interpreter
    // Returns:
    //     PyObject - A list of records
    fn read_records(&self, py: Python) -> PyResult<PyObject> {
        match crate::read(&self.path) {
            Ok(records) => self.convert_rust_records_to_py(py, records),
            Err(e) => Err(PyValueError::new_err(e.to_string())),
        }
    }

    // Update a record in the DBF file
    // Args:
    //     index: usize - The index of the record to update
    //     values: &PyDict - A dictionary of field values to update
    // Returns:
    //     PyResult<()> - A result indicating success or failure
    fn update_record(&self, index: usize, values: &PyDict) -> PyResult<()> {
        let mut dbf_file =
            File::open_read_write(&self.path).map_err(|e| PyValueError::new_err(e.to_string()))?;

        // Build a record from a PyDict
        let mut record = Record::default();
        for (key, value) in values.iter() {
            let field_name = key.extract::<String>()?;
            let field_value = self.convert_py_value_to_field_value(value)?;
            record.insert(field_name, field_value);
        }

        if index >= dbf_file.num_records() {
            // Append new record
            dbf_file
                .append_record(&record)
                .map_err(|e| PyValueError::new_err(e.to_string()))
        } else {
            // Update existing record
            // First collect all field indices and values
            let updates: Vec<_> = record
                .into_iter()
                .filter_map(|(name, value)| dbf_file.field_index(&name).map(|idx| (idx, value)))
                .collect();

            // Then update the record with collected indices
            let mut existing = dbf_file.record(index).ok_or_else(|| {
                PyValueError::new_err(format!("Could not get record at index {}", index))
            })?;

            for (field_index, value) in updates {
                existing
                    .write_field(field_index, &value)
                    .map_err(|e| PyValueError::new_err(e.to_string()))?;
            }
            Ok(())
        }
    }
}

// Private implementation
#[cfg(feature = "python")]
impl DBFFile {
    fn convert_py_records_to_rust(
        &self,
        records: &PyList,
        fields: &[FieldInfo],
    ) -> PyResult<Vec<Record>> {
        let mut rust_records = Vec::with_capacity(records.len());
        let first_record = records.get_item(0)?;

        match first_record.is_instance_of::<PyDict>() {
            true => {
                for record in records {
                    let py_dict = record.downcast::<PyDict>()?;
                    let mut dbf_record = Record::default();

                    for (key, value) in py_dict {
                        let field_name = key.extract::<String>()?;
                        let field_value = self.convert_py_value_to_field_value(value)?;
                        dbf_record.insert(field_name, field_value);
                    }

                    rust_records.push(dbf_record);
                }
            }
            false => {
                if !first_record.is_instance_of::<PyTuple>() {
                    return Err(PyValueError::new_err(
                        "Records must be either list of dicts or list of tuples",
                    ));
                }

                for record in records {
                    let py_tuple = record.downcast::<PyTuple>()?;
                    let mut dbf_record = Record::default();

                    if py_tuple.len() != fields.len() {
                        return Err(PyValueError::new_err(format!(
                            "Tuple length ({}) does not match number of fields ({})",
                            py_tuple.len(),
                            fields.len()
                        )));
                    }

                    for (i, field) in fields.iter().enumerate() {
                        let value = py_tuple.get_item(i)?;
                        let field_value = self.convert_py_value_to_field_value(value)?;
                        dbf_record.insert(field.name.to_string(), field_value);
                    }

                    rust_records.push(dbf_record);
                }
            }
        }

        Ok(rust_records)
    }

    fn convert_py_value_to_field_value(&self, value: &PyAny) -> PyResult<FieldValue> {
        if value.is_none() {
            return Ok(FieldValue::Character(None));
        }

        if let Ok(s) = value.extract::<String>() {
            // Try to parse date
            if s.len() == 8 && s.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(year) = s[0..4].parse::<u32>() {
                    if let Ok(month) = s[4..6].parse::<u32>() {
                        if let Ok(day) = s[6..8].parse::<u32>() {
                            if month > 0 && month <= 12 && day > 0 && day <= 31 {
                                return Ok(FieldValue::Date(Some(Date::new(day, month, year))));
                            }
                        }
                    }
                }
            }
            return Ok(FieldValue::Character(Some(s)));
        }

        if let Ok(n) = value.extract::<f64>() {
            let rounded = (n / NUMERIC_PRECISION).round() * NUMERIC_PRECISION;
            return Ok(FieldValue::Numeric(Some(rounded)));
        }

        if let Ok(i) = value.extract::<i32>() {
            return Ok(FieldValue::Integer(i));
        }

        if let Ok(b) = value.extract::<bool>() {
            return Ok(FieldValue::Logical(Some(b)));
        }

        Err(PyValueError::new_err(format!(
            "Unsupported value type: {:?}",
            value
        )))
    }

    fn convert_rust_records_to_py(&self, py: Python, records: Vec<Record>) -> PyResult<PyObject> {
        let py_list = PyList::empty(py);

        for record in records {
            let dict = PyDict::new(py);
            for (field_name, field_value) in record {
                let py_value = match field_value {
                    FieldValue::Character(opt) => match opt {
                        Some(s) => s.into_py(py),
                        None => py.None(),
                    },
                    FieldValue::Numeric(opt) => match opt {
                        Some(n) => n.into_py(py),
                        None => py.None(),
                    },
                    FieldValue::Logical(opt) => match opt {
                        Some(b) => b.into_py(py),
                        None => py.None(),
                    },
                    FieldValue::Date(opt) => match opt {
                        Some(d) => {
                            format!("{:04}{:02}{:02}", d.year(), d.month(), d.day()).into_py(py)
                        }
                        None => py.None(),
                    },
                    FieldValue::Float(opt) => match opt {
                        Some(f) => f.into_py(py),
                        None => py.None(),
                    },
                    FieldValue::Currency(c) => c.into_py(py),
                    FieldValue::DateTime(dt) => {
                        let date = dt.date();
                        let time = dt.time();
                        format!(
                            "{:04}{:02}{:02}{:02}{:02}{:02}",
                            date.year(),
                            date.month(),
                            date.day(),
                            time.hours(),
                            time.minutes(),
                            time.seconds()
                        )
                        .into_py(py)
                    }
                    FieldValue::Integer(i) => i.into_py(py),
                    FieldValue::Double(d) => d.into_py(py),
                    FieldValue::Memo(s) => s.into_py(py),
                };
                dict.set_item(field_name, py_value)?;
            }
            py_list.append(dict)?;
        }

        Ok(py_list.into())
    }
}
