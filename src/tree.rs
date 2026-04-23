use std::collections::HashMap;

use bitter::{BitReader, LittleEndianReader};
use evalexpr::{ContextWithMutableVariables, eval_with_context_mut, HashMapContext};
#[cfg(test)]
use evalexpr::eval_int_with_context;
use serde::Deserialize;
use serde_json::{Map, Number, Value};
#[cfg(test)]
use serde_json::json;
#[cfg(test)]
use crate::utils::hex_to_bin;

#[derive(Deserialize, Debug, Clone)]
pub struct Branch {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: Option<String>,
    #[serde(rename = "match")]
    pub match_cases: Option<Map<String, Value>>,
    pub loop_count: Option<Value>,
    pub branches: Option<Vec<Branch>>,
    pub optional: Option<bool>,
    pub eval: Option<String>
}

#[derive(Deserialize, Debug, Clone)]
pub struct Tree {
    pub branches: Vec<Branch>,
}

const TYPE_FLAG_USE_PREV_FIELD:&str = "u0";

fn read_bits(data: &[u8], bit_offset:usize, num_bits: u32) -> Result<(u64, usize), String> {
    let n_bytes_to_skip = bit_offset / 8;
    let n_bits_to_skip = (bit_offset % 8) as u32;
    let mut reader = if n_bytes_to_skip > 0 {
        LittleEndianReader::new(&data[n_bytes_to_skip..])
    } else {
        LittleEndianReader::new(data)
    };
    if n_bits_to_skip > 0 {
        _ = reader.read_bits(n_bits_to_skip);
    }
    let value = reader.read_bits(num_bits).ok_or(format!("Read failed: data len bits {} bit_offset {} attempt read num_bits: {}", data.len()*8, bit_offset, num_bits))?;
    let new_offset = bit_offset + num_bits as usize;
    Ok((value, new_offset))
}

fn evaluate_expression(value: Number, expression: &str) -> bool {
    if expression.starts_with("gt_") {
        if let Ok(threshold) = expression[3..].parse::<i64>() {
            return value.as_i64().unwrap() > threshold;
        }
    } else if expression.starts_with("ge_") {
        if let Ok(threshold) = expression[3..].parse::<i64>() {
            return value.as_i64().unwrap() >= threshold;
        }
    } else if expression.starts_with("lt_") {
        if let Ok(threshold) = expression[3..].parse::<i64>() {
            return value.as_i64().unwrap() < threshold;
        }
    } else if expression.starts_with("le_") {
        if let Ok(threshold) = expression[3..].parse::<i64>() {
            return value.as_i64().unwrap() <= threshold;
        }
    }
    false
}


pub fn get_field_size(field: &Branch, val_cache: &mut HashMap<String, Value>) -> (u32, Option<u64>)
{
    if field.loop_count.is_some() {
        match field.loop_count.clone().unwrap() {
            Value::String(loop_field_name) => {
                if let Some(loop_count) = val_cache.get(&loop_field_name) {
                    println!("loop_count from loop_field_name {} val_cache {}", loop_field_name, loop_count);
                    (0, Some(loop_count.as_u64().unwrap()))
                } else {
                    panic!("Previous field {} not found for loop_count matching", loop_field_name);
                }
            }
            Value::Number(loop_count_static) => {
                (0, Some(loop_count_static.as_u64().unwrap()))
            }
            _ => {
                panic!("loop_count must be either a previous field name or a number");
            }
        }
    } else {
        let bits = match field.field_type.clone().unwrap().as_str() {
            "u8" | "u8_le" | "u8_be" => 8,
            "u16" | "u16_le" | "u16_be" => 16,
            "u32" | "u32_le" | "u32_be" => 32,
            "u64" | "u64_le" | "u64_be" => 64,
            "f32" | "f32_le" | "f32_be" => 32,
            "f64" | "f64_le" | "f64_be" => 64,
            "i16" | "i16_le" | "i16_be" => 16,
            t if t.starts_with('u') && t.len() > 1 => t[1..].parse().unwrap_or(0),
            _ => 0,
        };
        (bits, None)
    }
}

pub fn process_field(data: &[u8], field: &Branch, dry_run: bool, value: Number, bit_offset:usize, prev_val_cache: &mut HashMap<String, Value>, parsed_data: &mut Map<String, Value>) -> Result<usize, (Value, String)>
{
    let mut new_bit_offset = bit_offset;
    if let Some(match_cases) = &field.match_cases {
        println!("match_cases: {:?}", match_cases);
        let mut matched = false;
        let matched_field_name = field.name.clone()+"_matched"; //use diff field name so wont override ori val in json dict
        parsed_data.insert(field.name.clone(), Value::from(value.clone()));
        for (key, description) in match_cases {
            if key.starts_with("gt_") || key.starts_with("ge_") || key.starts_with("lt_") || key.starts_with("le_") {
                if evaluate_expression(value.clone(), key) {
                    parsed_data.insert(matched_field_name.clone(), description.clone());
                    matched = true;
                    break;
                }
            } else if key == &value.to_string() {
                if let Some(obj) = description.as_object() {
                    println!("key matched: {} obj0", key);
                    if let Some(fields) = obj.get("branches").and_then(|f| f.as_array()) {
                        let nested_fields: Vec<Branch> = serde_json::from_value(Value::Array(fields.clone())).unwrap();
                        let (mut nested_data, nested_bits) = parse_schema(&data[(bit_offset / 8)..], &nested_fields, dry_run, prev_val_cache)?;
                        new_bit_offset += nested_bits;
                        if nested_data.is_object() && obj.contains_key("name") {
                            let no = nested_data.as_object_mut().unwrap();
                            no.insert(
                                "name".to_string(),
                                obj.get("name").unwrap().clone()
                            );
                        }
                        parsed_data.insert(matched_field_name.clone(), nested_data);
                    }

                } else {
                    println!("key matched: {} not object", key);
                    parsed_data.insert(matched_field_name.clone(), description.clone());
                }
                matched = true;
                break;
            }
        }
        if !matched {
            parsed_data.insert(matched_field_name, Value::String("unknown".to_string()));
        }
    } else {
        //non match case
        //make eval context
        let mut eval_context = HashMapContext::new();
        if value.is_f64() {
            eval_context.set_value("value_float".to_string(), value.as_f64().unwrap().into()).unwrap();
        } else if value.is_i64() {
            eval_context.set_value("value_int".to_string(), value.as_i64().unwrap().into()).unwrap();
        }
        eval_context.set_value("value_string".to_string(), value.to_string().into()).unwrap();

        match &field.eval {
            Some(evals) => {
                let eval_ret = eval_with_context_mut(evals.as_str(), &mut eval_context);
                match eval_ret {
                    Ok(ret) => {
                        println!("eval ret: {:?}", ret);
                        let ret_serde_value = if ret.is_float() {
                            Value::from(ret.as_float().unwrap())
                        } else if ret.is_int() {
                            Value::from(ret.as_int().unwrap())
                        } else if ret.is_boolean() {
                            Value::from(ret.as_boolean().unwrap())
                        } else {
                            Value::from(ret.to_string())
                        };
                        parsed_data.insert(field.name.clone(), ret_serde_value.clone());
                        prev_val_cache.insert(field.name.clone(), ret_serde_value.clone());
                    }
                    Err(emsg) => {
                        parsed_data.insert(field.name.clone()+"_eval_error", Value::from(format!("eval error: {}", emsg)));
                    }
                }
            }
            None => {
                parsed_data.insert(field.name.clone(), Value::from(value.clone()));
                prev_val_cache.insert(field.name.clone(), Value::from(value.clone()));
            }
        }
    }
    Ok(new_bit_offset)
}

pub fn parse_schema(
    data: &[u8],
    schema_fields: &[Branch],
    dry_run: bool,
    prev_val_cache: &mut HashMap<String, Value>,
) -> Result<(Value, usize), (Value, String)> {
    let mut bit_offset = 0;
    let mut parsed_data = Map::new();
    for field in schema_fields {
        println!("proc field {:?}", field);
        let mut prev_field_refer_val:Option<Number> = None;
        if field.field_type.is_some() || field.loop_count.is_some() { //normal fields or loop
            let (field_bits, loop_count_option) = get_field_size(&field, prev_val_cache);
            if loop_count_option.is_none() && field_bits == 0 {
                if field.field_type.clone().unwrap() == TYPE_FLAG_USE_PREV_FIELD && prev_val_cache.contains_key(&field.name) {
                    println!("got TYPE_FLAG_USE_PREV_FIELD and prev_val_cache has mathing field name");
                    prev_field_refer_val = Some(prev_val_cache.get(&field.name).unwrap().clone().as_number().unwrap().clone());
                } else {
                    println!("Invalid field - not a loop and no bits to parse: {:?}", field);
                    continue;
                }
            }
            if let Some(loop_count) = loop_count_option {  //loop around files

                let mut output_array:Vec<Value> = vec![];
                for li in 0..loop_count {
                    println!("loop {li} / loop_count: {loop_count} start");
                    let loop_ret = parse_schema(&data[(bit_offset / 8)..], field.branches.as_ref().unwrap(), dry_run, prev_val_cache);
                    match loop_ret {
                        Ok((nested_data, nested_bits)) => {
                            println!("loop {li} / loop_count: {loop_count} got nested_data {} nested_bits {}", nested_data, nested_bits);
                            bit_offset += nested_bits;
                            output_array.push(nested_data);
                        }
                        Err((loop_partially_parsed_data, errstr)) => {
                            println!("loop {li} / loop_count: {loop_count} got error {errstr} - so breaking now and discard _loop_parsed_data {loop_partially_parsed_data}");
                            break;
                        }
                    }

                }
                parsed_data.insert(field.name.clone(), Value::Array(output_array));
            } else {
                let final_value= if prev_field_refer_val.is_some() {
                    prev_field_refer_val.unwrap()
                } else {
                    let field_type = &field.field_type.clone().unwrap();

                    let (value, new_bit_offset) = if !dry_run { read_bits(data, bit_offset, field_bits)
                        //return parsed_data so far when error too - don't throw it away
                        .map_err(|err| (Value::Object(parsed_data.clone()), err))? } else { (0, bit_offset as usize + field_bits as usize) };
                    bit_offset = new_bit_offset;
                    let final_value_cand: Number = match field_type.as_str() {
                        "u8" | "u8_le" => Number::from(value as u8 as u32),
                        "u8_be" => Number::from(value as u8),
                        "u16" | "u16_le" => Number::from(u16::from_le_bytes((value as u16).to_le_bytes())),
                        "u16_be" => Number::from(u16::from_be_bytes((value as u16).to_be_bytes())),
                        "u32" | "u32_le" => Number::from(u32::from_le_bytes((value as u32).to_le_bytes())),
                        "u32_be" => Number::from(u32::from_be_bytes((value as u32).to_be_bytes())),
                        "u64" | "u64_le" => Number::from(u64::from_le_bytes((value as u64).to_le_bytes())),
                        "u64_be" => Number::from(u64::from_be_bytes((value as u64).to_be_bytes())),
                        "f32" | "f32_le" => Number::from_f64(f32::from_le_bytes((value as u32).to_le_bytes()) as f64).unwrap(),
                        "f32_be" => Number::from_f64(f32::from_be_bytes((value as u32).to_be_bytes()) as f64).unwrap(),
                        "f64" | "f64_le" => Number::from_f64(f64::from_le_bytes((value as u64).to_le_bytes()) as f64).unwrap(),
                        "f64_be" => Number::from_f64(f64::from_be_bytes((value as u64).to_be_bytes()) as f64).unwrap(),
                        "i16" | "i16_le" => Number::from(i16::from_le_bytes((value as u16).to_le_bytes())),
                        "i16_be" => Number::from(i16::from_be_bytes((value as u16).to_be_bytes())),
                        t if t.starts_with('u') => Number::from(value),
                        _ => { panic!("unsupported field_type: {}", field_type) },
                    };
                    final_value_cand
                };
                println!("final_value: {:?}", final_value);
                bit_offset = process_field(data, field, dry_run, final_value, bit_offset, prev_val_cache, &mut parsed_data)?;
            }
        } else if let Some(fields) = &field.branches { //nested fields
            let (nested_data, nested_bits) = parse_schema(&data[(bit_offset / 8)..], fields, dry_run, prev_val_cache)?;
            bit_offset += nested_bits;
            parsed_data.insert(field.name.clone(), nested_data);
        } else {
            panic!("unsupported usage: {:?}", field);
        }
    }
    Ok((Value::Object(parsed_data), bit_offset))
}


pub fn schema_to_tree(schema_json: Value) -> Tree {
    serde_json::from_value(schema_json).expect("Unable to parse JSON schema")
}

#[test]
fn test_nested_packet() {
    let schema_json = json!({
        "branches": [
            { "name": "header", "type": "u8" },
            { "name": "payload", "branches": [
                { "name": "field1", "type": "u8" },
                { "name": "field2", "type": "u16_le" }
            ]}
        ]
    });

    let data = hex_to_bin("01 02 E8 03");

    let schema = schema_to_tree(schema_json);
    let mut previous_values = HashMap::new();
    let (parsed_json, _) = parse_schema(&data, &schema.branches, false, &mut previous_values).unwrap();

    assert_eq!(parsed_json, json!({
        "header": 1,
        "payload": {
            "field1": 2,
            "field2": 1000
        }
    }));
}

#[test]
fn test_match_num_to_string() {
    let schema_json = json!({
        "branches": [
            { "name": "status", "type": "u8", "match": {
                "0": "Inactive",
                "1": "Active"
            }}
        ]
    });

    let data = hex_to_bin("01");

    let schema = schema_to_tree(schema_json);
    let mut previous_values = HashMap::new();
    let (parsed_json, _) = parse_schema(&data, &schema.branches, false, &mut previous_values).unwrap();
    assert_eq!(parsed_json, json!({
        "status": 1,
        "status_matched": "Active"
    }));
}

#[test]
fn test_loop_fields() {
    let schema_json = json!({
        "branches": [
            { "name": "count", "type": "u8" },
            { "name": "reserved", "type": "u8" },
            { "name": "items", "loop_count":"count", "branches": [
                { "name": "item", "type": "u8" }
            ]}
        ]
    });

    let data = hex_to_bin("03 FF 01 02 03");

    let schema = schema_to_tree(schema_json);
    let mut previous_values = HashMap::new();
    let (parsed_json, _) = parse_schema(&data, &schema.branches, false, &mut previous_values).unwrap();

    assert_eq!(parsed_json, json!({
        "count": 3,
        "reserved": 255,
        "items": [
            { "item": 1 },
            { "item": 2 },
            { "item": 3 }
        ]
    }));
}

#[test]
fn test_loop_static_count() {
    let schema_json = json!({
        "branches": [
            { "name": "reserved", "type": "u8" },
            { "name": "items", "loop_count": 3, "branches": [
                { "name": "item", "type": "u8" }
            ]}
        ]
    });

    let data = hex_to_bin("FE 01 02 03");

    let schema = schema_to_tree(schema_json);
    let mut previous_values = HashMap::new();
    let (parsed_json, _) = parse_schema(&data, &schema.branches, false, &mut previous_values).unwrap();

    assert_eq!(parsed_json, json!({
        "reserved": 254,
        "items": [
            { "item": 1 },
            { "item": 2 },
            { "item": 3 }
        ]
    }));
}

#[test]
fn test_loop_until_no_more_data() {
    let schema_json = json!({
        "branches": [
            { "name": "reserved", "type": "u8" },
            { "name": "items", "loop_count": 999, "branches": [
                { "name": "item", "type": "u8" }
            ]}
        ]
    });

    let data = hex_to_bin("FE 01 02 03");

    let schema = schema_to_tree(schema_json);
    let mut previous_values = HashMap::new();
    let (parsed_json, bit_offset) = parse_schema(&data, &schema.branches, false, &mut previous_values).unwrap();
    println!("bit_offset: {bit_offset}");
    assert_eq!(bit_offset, data.len()*8);
    assert_eq!(parsed_json, json!({
        "reserved": 254,
        "items": [
            { "item": 1 },
            { "item": 2 },
            { "item": 3 }
        ]
    }));
}


#[test]
fn test_json_default_val_if_not_present() {
    macro_rules! get_or_default {
    ($obj:expr, $key:expr, $default:expr) => {
        $obj.get($key).cloned().ok_or($default).unwrap_or_else(|err| serde_json::Value::from(err))
    };
    }

    let schema_json = json!({
        "a": 1,
        "b": 2
    });
    let c = schema_json.get("c").cloned().ok_or(3).unwrap_or_else(|err| Value::from(err));
    assert_eq!(c, 3);

    let c_alt = get_or_default!(schema_json, "c", 3);
    assert_eq!(c_alt, 3);
}

#[test]
fn test_match_to_nested_packet() {
    let schema_json = json!({
        "branches": [
            { "name": "type", "type": "u8", "match": {
                "0": "Type0",
                "1": {
                    "name": "Type1",
                    "branches": [
                        { "name": "field1", "type": "u8" },
                        { "name": "field2", "type": "u16_le" }
                    ]
                }
            }}
        ]
    });
    let data = hex_to_bin("01 02 E8 03");

    let schema = schema_to_tree(schema_json);
    let mut previous_values = HashMap::new();
    let (parsed_json, _) = parse_schema(&data, &schema.branches, false, &mut previous_values).unwrap();

    assert_eq!(parsed_json, json!({
        "type": 1,
        "type_matched": {
            "name": "Type1",
            "field1": 2,
            "field2": 1000
        }
    }));
}

#[test]
fn test_match_non_immediate_field_to_nested_packet() {
    let schema_json = json!({
        "branches": [
            { "name": "type", "type": "u8"},
            { "name": "reserved", "type": "u8"},
            //u0 infers 'dont read' and requires name declared earlier
            { "name": "type", "type": "u0", "match": {
                "0": "Type0",
                "1": {
                    "name": "Type1",
                    "branches": [
                        { "name": "field1", "type": "u8" },
                        { "name": "field2", "type": "u16_le" }
                    ]
                }
            }}
        ]
    });
    let data = hex_to_bin("01 ff 02 E8 03");

    let schema = schema_to_tree(schema_json);
    let mut previous_values = HashMap::new();
    let (parsed_json, _) = parse_schema(&data, &schema.branches, false, &mut previous_values).unwrap();

    assert_eq!(parsed_json, json!({
        "reserved": 255,
        "type": 1,
        "type_matched": {
            "name": "Type1",
            "field1": 2,
            "field2": 1000
        }
    }));
}

#[test]
fn test_dry_run() {
    let schema_json = json!({
        "branches": [
            { "name": "fix_status", "type": "u8" },
            { "name": "rcr", "type": "u8" },
            { "name": "millisecond", "type": "u16_le" }
        ]
    });

    let schema = schema_to_tree(schema_json);
    let mut previous_values = HashMap::new();
    let (parsed_json, _) = parse_schema(&[], &schema.branches, true, &mut previous_values).unwrap();
    assert_eq!(parsed_json, json!({
        "fix_status": 0,
        "millisecond": 0,
        "rcr": 0
    }));
}

#[test]
fn test_coral_reef() {
    let coral_reef_structure_json = json!({
        "branches": [
            { "name": "nature_reserve_id", "type": "u16" },
            { "name": "n_colonies", "type": "u32" },
            { "name": "region_id", "type": "u16" },
            {
            "name": "colonies", "loop_count": "n_colonies", "branches": [
                { "name": "n_polyps", "type": "u32" },
                { "name": "polyps", "loop_count": "n_polyps", "branches": [
                    { "name": "polyp_id", "type": "u32" },
                    { "name": "polyp_type", "type": "u8" }
                ]
                }
            ]
            }
        ]
    });

    let data = hex_to_bin(r#"
    fe ff
    01 00 00 00
    04 00
    02 00 00 00
    00 00 00 00
    01
    01 00 00 00
    01
    "#);

    let schema = schema_to_tree(coral_reef_structure_json);
    let mut previous_values = HashMap::new();
    let (parsed_json, _) = parse_schema(&data, &schema.branches, false, &mut previous_values).unwrap();

    assert_eq!(parsed_json, json!({
        "n_colonies": 1,
        "nature_reserve_id": 65534,
        "region_id": 4,
        "colonies": [
            {
                "n_polyps": 2,
                "polyps": [
                { "polyp_id": 0, "polyp_type": 1},
                { "polyp_id": 1, "polyp_type": 1}
            ]
            }
        ]
    }));
}


#[test]
fn test_eval_expr()
{
    let mut eval_context = HashMapContext::new();
    eval_context.set_value("a".to_string(), 1.into()).unwrap();
    assert_eq!(eval_int_with_context("a + 1 + 2 + 3", &eval_context), Ok(7));

    //cases for  eval int/float/str
    let schema_json = json!({
        "branches": [
            { "name": "sth", "type": "u8", "eval": "value_int + 1"},
        ]
    });
    let data = hex_to_bin("01");
    let schema = schema_to_tree(schema_json);
    let mut previous_values = HashMap::new();
    let (parsed_json, bit_offset) = parse_schema(&data, &schema.branches, false, &mut previous_values).unwrap();
    assert_eq!(bit_offset, 8);
    assert_eq!(parsed_json, json!({
        "sth": 2,
    }));

    let schema_json = json!({
        "branches": [
            { "name": "sth", "type": "u8", "eval": "value_int + 1.0"},
        ]
    });
    let data = hex_to_bin("01");
    let schema = schema_to_tree(schema_json);
    let mut previous_values = HashMap::new();
    let (parsed_json, bit_offset) = parse_schema(&data, &schema.branches, false, &mut previous_values).unwrap();
    assert_eq!(bit_offset, 8);
    assert_eq!(parsed_json, json!({
        "sth": 2.0,
    }));

    let schema_json = json!({
        "branches": [
            { "name": "sth", "type": "u8", "eval": "math::pow(value_int, 2)"},
        ]
    });
    let data = hex_to_bin("0A");
    let schema = schema_to_tree(schema_json);
    let mut previous_values = HashMap::new();
    let (parsed_json, bit_offset) = parse_schema(&data, &schema.branches, false, &mut previous_values).unwrap();
    assert_eq!(bit_offset, 8);
    assert_eq!(parsed_json, json!({
        "sth": 100.0,
    }));
}