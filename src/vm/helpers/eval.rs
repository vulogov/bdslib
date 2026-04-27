use rust_dynamic::types::*;
use rust_dynamic::value::Value;
use rust_multistackvm::multistackvm::{VM};
use bund_language_parser::bund_parse;
use easy_error::{Error, bail};

/// Convert any `rust_dynamic` Value to its natural `serde_json` representation.
/// Handles all Val variants explicitly; exotic types (Metrics, Operator, etc.)
/// become `null`.
pub fn dynamic_to_json(v: Value) -> serde_json::Value {
    match v.data {
        Val::Null | Val::Exit                   => serde_json::Value::Null,
        Val::Bool(b)                            => serde_json::json!(b),
        Val::I64(i)                             => serde_json::json!(i),
        Val::F64(f)                             => serde_json::json!(f),
        Val::String(s) | Val::Token(s)          => serde_json::Value::String(s),
        Val::List(items)
        | Val::Lambda(items)
        | Val::Queue(items)                     => serde_json::Value::Array(
                                                       items.into_iter().map(dynamic_to_json).collect()),
        Val::Matrix(rows)                       => serde_json::Value::Array(
                                                       rows.into_iter()
                                                           .map(|row| serde_json::Value::Array(
                                                               row.into_iter().map(dynamic_to_json).collect()))
                                                           .collect()),
        Val::Map(map)                           => serde_json::Value::Object(
                                                       map.into_iter()
                                                           .map(|(k, v)| (k, dynamic_to_json(v)))
                                                           .collect()),
        Val::ValueMap(map)                      => serde_json::Value::Object(
                                                       map.into_iter()
                                                           .filter_map(|(k, v)| {
                                                               k.cast_string().ok()
                                                                .map(|ks| (ks, dynamic_to_json(v)))
                                                           })
                                                           .collect()),
        Val::Json(j)                            => j,
        Val::Binary(b)                          => serde_json::Value::Array(
                                                       b.into_iter().map(|byte| serde_json::json!(byte)).collect()),
        Val::Time(t)                            => serde_json::Value::String(t.to_string()),
        Val::Embedding(e)                       => serde_json::Value::Array(
                                                       e.into_iter().map(|f| serde_json::json!(f as f64)).collect()),
        Val::Error(e)                           => serde_json::json!({ "error": format!("{e:?}") }),
        _                                       => serde_json::Value::Null,
    }
}

#[time_graph::instrument]
pub fn bund_compile_and_eval(vm: &mut VM, code: String) -> Result<&mut VM, Error>  {
    let source = format!("{}\n", code.clone());
    match bund_parse(&source) {
        Ok(words) => {
            for word in words {
                match word.dt {
                    NONE => {
                        continue;
                    }
                    EXIT => {
                        break;
                    }
                    ERROR => {
                        match word.cast_error() {
                            Ok(error) => {
                                bail!("Detected an Error posted on the stack: {:?}", error);
                            }
                            Err(err) => {
                                bail!("Detected an Error posted on the stack, but extraction of error is failed: {}", err);
                            }
                        }
                    }
                    _ => {
                        match vm.apply(word.clone()) {
                            Ok(_) => {}
                            Err(err) => {
                                bail!("Attempt to evaluate value {:?} returned error: {}", &word, err);
                            }
                        }
                    }
                }
            }
        }
        Err(err) => {
            bail!("{}", err);
        }
    }
    Ok(vm)
}
