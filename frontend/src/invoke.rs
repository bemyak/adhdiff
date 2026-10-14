//! Hand-rolled bridge to the Tauri IPC internals (`window.__TAURI_INTERNALS__`),
//! which Tauri injects regardless of `app.withGlobalTauri`.

use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI_INTERNALS__"], catch, js_name = invoke)]
    fn tauri_invoke(cmd: &str, args: JsValue) -> Result<js_sys::Promise, JsValue>;
}

/// Call a Tauri command that takes no arguments and decode its JSON reply.
/// The reply crosses the IPC boundary as a plain JS object; stringify +
/// serde_json keeps this free of a serde-wasm-bindgen dependency.
pub async fn invoke<T: serde::de::DeserializeOwned>(cmd: &str) -> Result<T, String> {
    invoke_maybe(cmd, JsValue::UNDEFINED).await
}

/// Same, with a serializable argument object.
pub async fn invoke_args<T, A>(cmd: &str, args: &A) -> Result<T, String>
where
    T: serde::de::DeserializeOwned,
    A: serde::Serialize,
{
    let json = serde_json::to_string(args).map_err(|e| e.to_string())?;
    let value = js_sys::JSON::parse(&json).map_err(|e| format!("{e:?}"))?;
    invoke_maybe(cmd, value).await
}

async fn invoke_maybe<T: serde::de::DeserializeOwned>(
    cmd: &str,
    args: JsValue,
) -> Result<T, String> {
    let promise = tauri_invoke(cmd, args).map_err(|e| format!("{e:?}"))?;
    let value = JsFuture::from(promise)
        .await
        .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))?;
    let json = js_sys::JSON::stringify(&value)
        .map_err(|e| format!("{e:?}"))?
        .as_string()
        .ok_or_else(|| "invoke returned a non-serializable value".to_string())?;
    serde_json::from_str(&json).map_err(|e| e.to_string())
}
