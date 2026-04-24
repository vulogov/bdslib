use super::params::rpc_err;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;

#[derive(serde::Deserialize)]
struct EvalParams {
    context: String,
    script: String,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/eval", |params, _ctx, _| async move {
            let p: EvalParams = params.parse()?;

            tokio::task::spawn_blocking(move || {
                // Acquire (or lazily create) the named BUND VM instance.
                let mut guard = bdslib::context::get(&p.context)
                    .map_err(|e| rpc_err(-32001, e))?;

                // Run the script against the VM.
                bdslib::vm::helpers::eval::bund_compile_and_eval(
                    &mut guard.vm,
                    p.script,
                )
                .map_err(|e| rpc_err(-32002, e))?;

                // Collect the workbench (result stack) as a JSON array.
                let result: Vec<serde_json::Value> = guard
                    .vm
                    .stack
                    .workbench
                    .stack
                    .iter()
                    .map(|v| v.cast_value_to_json().unwrap_or(serde_json::Value::Null))
                    .collect();

                Ok::<serde_json::Value, ErrorObject>(serde_json::json!({ "result": result }))
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?
        })
        .unwrap();
}
