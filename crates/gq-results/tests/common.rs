#![allow(dead_code)]
use std::sync::Arc;

use arrow_array::{Float64Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema, SchemaRef};

pub fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, true),
        Field::new("score", DataType::Float64, true),
    ]))
}

/// Rows `start..start+n`. `name` is ~120 bytes; every 7th score is NULL.
pub fn batch(start: i64, n: usize) -> RecordBatch {
    let ids: Vec<i64> = (start..start + n as i64).collect();
    let names: Vec<Option<String>> =
        ids.iter().map(|i| Some(format!("row {i} {}", "abcdefghij".repeat(11)))).collect();
    let scores: Vec<Option<f64>> = ids.iter().map(|i| (i % 7 != 0).then_some(*i as f64 / 2.0)).collect();
    RecordBatch::try_new(
        schema(),
        vec![
            Arc::new(Int64Array::from(ids)),
            Arc::new(StringArray::from(names)),
            Arc::new(Float64Array::from(scores)),
        ],
    )
    .unwrap()
}

pub fn ids(batches: &[RecordBatch]) -> Vec<i64> {
    batches
        .iter()
        .flat_map(|b| b.column(0).as_any().downcast_ref::<Int64Array>().unwrap().values().to_vec())
        .collect()
}
