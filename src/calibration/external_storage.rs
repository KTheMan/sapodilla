//! User-visible calibration registry storage for the web application.

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use super::CalibrationRegistry;

const EXTERNAL_CALIBRATION_ENVELOPE_VERSION: u8 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ExternalCalibrationEnvelope {
    version: u8,
    generation: u64,
    saved_at: u64,
    registry: CalibrationRegistry,
    registry_sha1: String,
}

fn registry_sha1(registry: &CalibrationRegistry) -> Result<String, String> {
    serde_json::to_vec(registry)
        .map(|encoded| hex::encode(Sha1::digest(encoded)))
        .map_err(|error| format!("could not encode calibration registry checksum: {error}"))
}

pub fn encode_registry(
    registry: CalibrationRegistry,
    generation: u64,
    saved_at: u64,
) -> Result<String, String> {
    let registry_sha1 = registry_sha1(&registry)?;
    serde_json::to_string_pretty(&ExternalCalibrationEnvelope {
        version: EXTERNAL_CALIBRATION_ENVELOPE_VERSION,
        generation,
        saved_at,
        registry,
        registry_sha1,
    })
    .map_err(|error| format!("could not encode calibration registry: {error}"))
}

pub fn decode_registry(encoded: &str) -> Result<CalibrationRegistry, String> {
    let envelope = serde_json::from_str::<ExternalCalibrationEnvelope>(encoded)
        .map_err(|error| format!("calibration registry file is invalid: {error}"))?;
    if envelope.version != EXTERNAL_CALIBRATION_ENVELOPE_VERSION {
        return Err(format!(
            "unsupported calibration registry file version {}",
            envelope.version
        ));
    }
    let expected = registry_sha1(&envelope.registry)?;
    if !expected.eq_ignore_ascii_case(&envelope.registry_sha1) {
        return Err("calibration registry file checksum does not match".into());
    }
    envelope
        .registry
        .sanitize()
        .map_err(|error| format!("calibration registry file is invalid: {error}"))
}

pub fn decode_registry_with_backup(
    primary: Option<&str>,
    backup: Option<&str>,
) -> Result<(Option<CalibrationRegistry>, bool), String> {
    let primary_result = primary.map(decode_registry).transpose();
    match primary_result {
        Ok(Some(registry)) => Ok((Some(registry), false)),
        Ok(None) => match backup.map(decode_registry).transpose() {
            Ok(Some(registry)) => Ok((Some(registry), true)),
            Ok(None) => Ok((None, false)),
            Err(error) => Err(format!("calibration registry backup is invalid: {error}")),
        },
        Err(primary_error) => match backup.map(decode_registry).transpose() {
            Ok(Some(registry)) => Ok((Some(registry), true)),
            Ok(None) => Err(format!(
                "calibration registry is invalid and no valid backup is available: {primary_error}"
            )),
            Err(backup_error) => Err(format!(
                "calibration registry and backup are invalid: {primary_error}; {backup_error}"
            )),
        },
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExternalDirectorySnapshot {
    Unavailable,
    Unconfigured,
    PermissionRequired {
        directory_name: String,
    },
    Ready {
        directory_name: String,
        registry_json: Option<String>,
        backup_json: Option<String>,
    },
}

#[cfg(target_arch = "wasm32")]
mod web {
    use super::*;
    use js_sys::Promise;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen_futures::JsFuture;

    #[wasm_bindgen(module = "/src/calibration/web_storage.js")]
    extern "C" {
        #[wasm_bindgen(js_name = calibrationStorageSupported)]
        fn storage_supported() -> bool;
        #[wasm_bindgen(js_name = calibrationStorageRestore)]
        fn storage_restore() -> Promise;
        #[wasm_bindgen(js_name = calibrationStorageChoose)]
        fn storage_choose() -> Promise;
        #[wasm_bindgen(js_name = calibrationStorageReconnect)]
        fn storage_reconnect() -> Promise;
        #[wasm_bindgen(js_name = calibrationStorageWrite)]
        fn storage_write(registry_json: &str, expected_registry_json: &str) -> Promise;
        #[wasm_bindgen(js_name = calibrationStorageForget)]
        fn storage_forget() -> Promise;
    }

    #[derive(Deserialize)]
    struct WireSnapshot {
        state: String,
        directory_name: Option<String>,
        registry_json: Option<String>,
        backup_json: Option<String>,
    }

    fn js_error(error: JsValue) -> String {
        error
            .dyn_ref::<js_sys::Error>()
            .map(js_sys::Error::message)
            .and_then(|message| message.as_string())
            .or_else(|| error.as_string())
            .unwrap_or_else(|| "calibration folder operation failed".into())
    }

    pub async fn snapshot_from(promise: Promise) -> Result<ExternalDirectorySnapshot, String> {
        let value = JsFuture::from(promise).await.map_err(js_error)?;
        let encoded = value
            .as_string()
            .ok_or_else(|| "calibration folder returned an invalid response".to_owned())?;
        let wire = serde_json::from_str::<WireSnapshot>(&encoded)
            .map_err(|error| format!("calibration folder response was invalid: {error}"))?;
        match wire.state.as_str() {
            "unavailable" => Ok(ExternalDirectorySnapshot::Unavailable),
            "unconfigured" => Ok(ExternalDirectorySnapshot::Unconfigured),
            "permission-required" => Ok(ExternalDirectorySnapshot::PermissionRequired {
                directory_name: wire
                    .directory_name
                    .unwrap_or_else(|| "Selected folder".into()),
            }),
            "ready" => Ok(ExternalDirectorySnapshot::Ready {
                directory_name: wire
                    .directory_name
                    .unwrap_or_else(|| "Selected folder".into()),
                registry_json: wire.registry_json,
                backup_json: wire.backup_json,
            }),
            state => Err(format!("calibration folder returned unknown state {state}")),
        }
    }

    pub fn is_supported() -> bool {
        storage_supported()
    }

    pub async fn restore() -> Result<ExternalDirectorySnapshot, String> {
        snapshot_from(storage_restore()).await
    }

    pub fn begin_choose() -> Promise {
        storage_choose()
    }

    pub fn begin_reconnect() -> Promise {
        storage_reconnect()
    }

    pub async fn write_registry(
        registry_json: String,
        expected_registry_json: Option<String>,
    ) -> Result<String, String> {
        JsFuture::from(storage_write(
            &registry_json,
            expected_registry_json.as_deref().unwrap_or_default(),
        ))
        .await
        .map_err(js_error)?
        .as_string()
        .ok_or_else(|| "calibration folder returned an invalid write response".into())
    }

    pub async fn forget() -> Result<(), String> {
        JsFuture::from(storage_forget()).await.map_err(js_error)?;
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
pub use web::*;

#[cfg(not(target_arch = "wasm32"))]
pub fn is_supported() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_registry_envelope_round_trips_and_rejects_corruption() {
        let registry = CalibrationRegistry::from_store(&super::super::CalibrationStore::default());
        let encoded = encode_registry(registry.clone(), 7, 42).unwrap();
        assert_eq!(decode_registry(&encoded), Ok(registry.clone()));

        let mut value = serde_json::from_str::<serde_json::Value>(&encoded).unwrap();
        value["registry_sha1"] = "0000000000000000000000000000000000000000".into();
        assert!(decode_registry(&serde_json::to_string(&value).unwrap()).is_err());

        let (recovered, from_backup) =
            decode_registry_with_backup(Some("not json"), Some(&encoded)).unwrap();
        assert_eq!(recovered, Some(registry));
        assert!(from_backup);
        assert!(decode_registry_with_backup(Some("not json"), Some("also bad")).is_err());
    }

    #[test]
    fn missing_primary_recovers_from_valid_backup() {
        let registry = CalibrationRegistry::from_store(&super::super::CalibrationStore::default());
        let encoded = encode_registry(registry.clone(), 8, 43).unwrap();

        let (recovered, from_backup) = decode_registry_with_backup(None, Some(&encoded)).unwrap();
        assert_eq!(recovered, Some(registry));
        assert!(from_backup);
    }
}
