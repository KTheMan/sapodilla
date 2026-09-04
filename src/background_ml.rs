#[cfg(all(feature = "background-ml", not(target_arch = "wasm32")))]
use std::sync::{Mutex, OnceLock};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use image::{GrayImage, Luma, RgbaImage, imageops::FilterType};

pub const BIREFNET_INPUT_SIZE: usize = 1024;
pub const BIREFNET_MODEL_FILENAME: &str = "birefnet_general.onnx";
pub const BIREFNET_MODEL_BYTES: u64 = 972_666_916;
pub const BIREFNET_MODEL_MIN_BYTES: u64 = BIREFNET_MODEL_BYTES;
pub const BIREFNET_MODEL_ENV: &str = "OFFLINE_BG_REMOVAL_MODEL";
pub const BIREFNET_MODEL_URL: &str =
    "https://github.com/danielgatis/rembg/releases/download/v0.0.0/BiRefNet-general-epoch_244.onnx";

pub const fn inference_runtime_enabled() -> bool {
    cfg!(all(feature = "background-ml", not(target_arch = "wasm32")))
}

/// Details about a local model that has passed the inexpensive checks which
/// are possible without parsing the complete ONNX protobuf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelFileInfo {
    pub path: PathBuf,
    pub bytes: u64,
}

/// Small interface around the inference runtime so preprocessing and matting
/// remain testable without loading the nearly-gigabyte production model.
pub trait SegmentationBackend {
    fn infer(&mut self, nchw_rgb: &[f32], side: usize) -> anyhow::Result<Vec<f32>>;
}

/// CPU ONNX Runtime backend for the full BiRefNet model.
///
/// This type only exists for native builds made with `--features
/// background-ml`. Model bytes are always loaded from a caller-provided local
/// path. The default UI provisions the pinned model into the local cache on
/// first use, while callers may still supply an existing model explicitly.
#[cfg(all(feature = "background-ml", not(target_arch = "wasm32")))]
pub struct OrtBiRefNetBackend {
    session: ort::session::Session,
    model_path: PathBuf,
}

#[cfg(all(feature = "background-ml", not(target_arch = "wasm32")))]
impl OrtBiRefNetBackend {
    pub fn new(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        use ort::session::builder::GraphOptimizationLevel;

        let model = inspect_model_file(path.as_ref())?;
        let session = ort::session::Session::builder()
            .map_err(|error| anyhow::anyhow!("could not initialize ONNX Runtime: {error}"))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|error| anyhow::anyhow!("could not configure ONNX Runtime: {error}"))?
            .commit_from_file(&model.path)
            .map_err(|error| {
                anyhow::anyhow!(
                    "could not load the BiRefNet ONNX model at {}: {error}",
                    model.path.display()
                )
            })?;
        if session.inputs().is_empty() {
            bail!("the BiRefNet ONNX model has no inputs");
        }
        if session.outputs().is_empty() {
            bail!("the BiRefNet ONNX model has no outputs");
        }
        Ok(Self {
            session,
            model_path: model.path,
        })
    }

    pub fn from_default_path() -> anyhow::Result<Self> {
        Self::new(default_model_path()?)
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }
}

#[cfg(all(feature = "background-ml", not(target_arch = "wasm32")))]
impl SegmentationBackend for OrtBiRefNetBackend {
    fn infer(&mut self, nchw_rgb: &[f32], side: usize) -> anyhow::Result<Vec<f32>> {
        use ort::value::Tensor;

        if side == 0 {
            bail!("background model input side must not be zero");
        }
        let expected = 3usize
            .checked_mul(side)
            .and_then(|value| value.checked_mul(side))
            .context("background model input dimensions overflowed")?;
        if nchw_rgb.len() != expected {
            bail!(
                "background model received {} input values; expected {}",
                nchw_rgb.len(),
                expected
            );
        }

        let input = Tensor::from_array((
            [1usize, 3, side, side],
            nchw_rgb.to_vec().into_boxed_slice(),
        ))
        .context("could not create the background model input tensor")?;
        let outputs = self
            .session
            .run(ort::inputs![input])
            .map_err(|error| anyhow::anyhow!("BiRefNet inference failed: {error}"))?;
        if outputs.len() == 0 {
            bail!("the BiRefNet model returned no output");
        }
        let output = &outputs[0];
        let (shape, logits) = output
            .try_extract_tensor::<f32>()
            .context("the first BiRefNet output is not a CPU float32 tensor")?;
        let expected_output = side
            .checked_mul(side)
            .context("background model output dimensions overflowed")?;
        if logits.len() != expected_output {
            bail!(
                "the first BiRefNet output has shape {:?} ({} values); expected {} mask values",
                &**shape,
                logits.len(),
                expected_output
            );
        }
        Ok(logits.to_vec())
    }
}

/// Run inference through a process-wide session cache. Building and optimizing
/// the nearly-gigabyte BiRefNet model dominates repeated background-removal
/// actions, while ONNX Runtime sessions are explicitly designed for reuse.
#[cfg(all(feature = "background-ml", not(target_arch = "wasm32")))]
pub fn remove_background_with_cached_model(
    image: &RgbaImage,
    model_path: &Path,
) -> anyhow::Result<RgbaImage> {
    static BACKEND: OnceLock<Mutex<Option<OrtBiRefNetBackend>>> = OnceLock::new();

    let model = inspect_model_file(model_path)?;
    let mut cached = BACKEND
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| anyhow::anyhow!("background model cache lock was poisoned"))?;
    if cached
        .as_ref()
        .is_none_or(|backend| backend.model_path() != model.path)
    {
        // Drop a previously selected near-gigabyte session before loading the
        // replacement so changing models does not temporarily double memory.
        *cached = None;
        *cached = Some(OrtBiRefNetBackend::new(&model.path)?);
    }
    remove_background(
        image,
        cached
            .as_mut()
            .expect("background model cache was initialized"),
    )
}

/// Run the complete BiRefNet image pipeline using a local inference backend.
pub fn remove_background(
    image: &RgbaImage,
    backend: &mut impl SegmentationBackend,
) -> anyhow::Result<RgbaImage> {
    let input = preprocess_birefnet(image, BIREFNET_INPUT_SIZE);
    let logits = backend.infer(&input, BIREFNET_INPUT_SIZE)?;
    if logits.len() != BIREFNET_INPUT_SIZE * BIREFNET_INPUT_SIZE {
        bail!(
            "background model returned {} values; expected {}",
            logits.len(),
            BIREFNET_INPUT_SIZE * BIREFNET_INPUT_SIZE
        );
    }
    apply_logits(image, &logits, BIREFNET_INPUT_SIZE, BIREFNET_INPUT_SIZE)
}

/// Resize and normalize RGB into ImageNet NCHW float32 input.
pub fn preprocess_birefnet(image: &RgbaImage, side: usize) -> Vec<f32> {
    let resized = image::imageops::resize(image, side as u32, side as u32, FilterType::Lanczos3);
    let pixels = side * side;
    let mut tensor = vec![0.0; pixels * 3];
    let maximum = resized
        .pixels()
        .flat_map(|pixel| pixel.0[..3].iter().copied())
        .max()
        .map(f32::from)
        .unwrap_or(1.0)
        .max(1e-6);
    const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
    const STD: [f32; 3] = [0.229, 0.224, 0.225];
    for (index, pixel) in resized.pixels().enumerate() {
        for channel in 0..3 {
            let value = f32::from(pixel[channel]) / maximum;
            tensor[channel * pixels + index] = (value - MEAN[channel]) / STD[channel];
        }
    }
    tensor
}

/// Convert model logits into a min-max normalized alpha matte and apply it.
pub fn apply_logits(
    image: &RgbaImage,
    logits: &[f32],
    mask_width: usize,
    mask_height: usize,
) -> anyhow::Result<RgbaImage> {
    if logits.len() != mask_width.saturating_mul(mask_height) || mask_width == 0 || mask_height == 0
    {
        bail!("invalid background model output dimensions");
    }
    let minimum_logit = logits.iter().copied().fold(f32::INFINITY, f32::min);
    let maximum_logit = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let sigmoid = |value: f32| 1.0 / (1.0 + (-value).exp());
    // Sigmoid is monotonic, so its extrema can be derived without retaining a
    // second 4 MiB probability image for the 1024x1024 production mask.
    let minimum = sigmoid(minimum_logit);
    let maximum = sigmoid(maximum_logit);
    let range = maximum - minimum;
    let mask = GrayImage::from_fn(mask_width as u32, mask_height as u32, |x, y| {
        let value = sigmoid(logits[y as usize * mask_width + x as usize]);
        let normalized = if range <= f32::EPSILON {
            0.0
        } else {
            (value - minimum) / range
        };
        Luma([(normalized.clamp(0.0, 1.0) * 255.0).round() as u8])
    });
    let mask = image::imageops::resize(&mask, image.width(), image.height(), FilterType::Lanczos3);
    let mut output = image.clone();
    for (pixel, matte) in output.pixels_mut().zip(mask.pixels()) {
        pixel[3] = ((u16::from(pixel[3]) * u16::from(matte[0])) / 255) as u8;
    }
    Ok(output)
}

pub fn default_model_path() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os(BIREFNET_MODEL_ENV) {
        if path.is_empty() {
            bail!("{BIREFNET_MODEL_ENV} is set but empty");
        }
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .context("could not determine the user home directory")?;
    Ok(PathBuf::from(home)
        .join(".offline-bg-removal")
        .join("models")
        .join(BIREFNET_MODEL_FILENAME))
}

/// Return the cached model, downloading it atomically on first use.
#[cfg(all(feature = "background-ml", not(target_arch = "wasm32")))]
pub fn ensure_model_available() -> anyhow::Result<PathBuf> {
    let destination = default_model_path()?;
    if inspect_model_file(&destination).is_ok() {
        return Ok(destination);
    }
    let parent = destination
        .parent()
        .context("background model cache path has no parent directory")?;
    std::fs::create_dir_all(parent).context("could not create background model cache")?;
    let partial = destination.with_extension("onnx.part");
    let _ = std::fs::remove_file(&partial);
    let mut response = reqwest::blocking::Client::new()
        .get(BIREFNET_MODEL_URL)
        .send()
        .context("could not download the background model")?
        .error_for_status()
        .context("background model download failed")?;
    if response.content_length() != Some(BIREFNET_MODEL_BYTES) {
        bail!(
            "background model server returned an unexpected size ({:?}; expected {BIREFNET_MODEL_BYTES})",
            response.content_length()
        );
    }
    let mut file = File::create(&partial).context("could not create background model download")?;
    std::io::copy(&mut response, &mut file).context("could not save background model download")?;
    drop(file);
    let bytes = std::fs::metadata(&partial)
        .context("could not inspect background model download")?
        .len();
    if bytes != BIREFNET_MODEL_BYTES {
        let _ = std::fs::remove_file(&partial);
        bail!(
            "downloaded background model is incomplete ({bytes} bytes; expected {BIREFNET_MODEL_BYTES})"
        );
    }
    if destination.exists() {
        std::fs::remove_file(&destination)
            .context("could not replace incomplete background model")?;
    }
    std::fs::rename(&partial, &destination)
        .context("could not finish background model download")?;
    Ok(destination)
}

/// Inspect the local model before asking ONNX Runtime to parse it.
pub fn inspect_model_file(path: &Path) -> anyhow::Result<ModelFileInfo> {
    let info = inspect_model_file_with_minimum(path, BIREFNET_MODEL_MIN_BYTES)?;
    if info.bytes != BIREFNET_MODEL_BYTES {
        bail!(
            "background model at {} has an unexpected size ({} bytes; expected {})",
            path.display(),
            info.bytes,
            BIREFNET_MODEL_BYTES
        );
    }
    Ok(info)
}

pub fn validate_model_file(path: &Path) -> anyhow::Result<()> {
    inspect_model_file(path).map(|_| ())
}

fn inspect_model_file_with_minimum(
    path: &Path,
    minimum_bytes: u64,
) -> anyhow::Result<ModelFileInfo> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("could not read background model at {}", path.display()))?;
    if !metadata.is_file() {
        bail!("background model path {} is not a file", path.display());
    }
    if metadata.len() < minimum_bytes {
        bail!(
            "background model at {} is incomplete ({} bytes; expected at least {})",
            path.display(),
            metadata.len(),
            minimum_bytes
        );
    }
    // `metadata` alone does not prove the current process can actually read
    // the file. Probe one byte before starting the expensive session load.
    let mut file = File::open(path)
        .with_context(|| format!("could not open background model at {}", path.display()))?;
    let mut first_byte = [0u8; 1];
    file.read_exact(&mut first_byte)
        .with_context(|| format!("could not read background model at {}", path.display()))?;
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("could not resolve background model at {}", path.display()))?;
    Ok(ModelFileInfo {
        path: canonical,
        bytes: metadata.len(),
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use image::Rgba;

    use super::*;

    #[test]
    fn preprocess_is_nchw_and_imagenet_normalized() {
        let image = RgbaImage::from_pixel(1, 1, Rgba([255, 0, 0, 255]));
        let tensor = preprocess_birefnet(&image, 2);
        assert_eq!(tensor.len(), 12);
        assert!((tensor[0] - (1.0 - 0.485) / 0.229).abs() < 1e-5);
        assert!((tensor[4] - (0.0 - 0.456) / 0.224).abs() < 1e-5);
        assert!((tensor[8] - (0.0 - 0.406) / 0.225).abs() < 1e-5);
    }

    #[test]
    fn postprocess_preserves_rgb_and_combines_existing_alpha() {
        let image = RgbaImage::from_raw(2, 1, vec![100, 110, 120, 200, 10, 20, 30, 255]).unwrap();
        let output = apply_logits(&image, &[-10.0, 10.0], 2, 1).unwrap();
        assert_eq!(&output.get_pixel(0, 0).0[..3], &[100, 110, 120]);
        assert!(output.get_pixel(0, 0)[3] < 2);
        assert_eq!(output.get_pixel(1, 0)[3], 255);
    }

    #[test]
    fn malformed_model_output_is_rejected() {
        assert!(
            apply_logits(&RgbaImage::new(1, 1), &[0.0], 0, 1)
                .unwrap_err()
                .to_string()
                .contains("dimensions")
        );
    }

    #[test]
    fn runtime_capability_matches_the_build_configuration() {
        assert_eq!(
            inference_runtime_enabled(),
            cfg!(all(feature = "background-ml", not(target_arch = "wasm32")))
        );
    }

    #[test]
    fn model_inspection_rejects_directories_and_truncated_files() {
        let root =
            std::env::temp_dir().join(format!("sapodilla-model-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        assert!(
            inspect_model_file_with_minimum(&root, 1)
                .unwrap_err()
                .to_string()
                .contains("not a file")
        );

        let path = root.join("model.onnx");
        File::create(&path).unwrap().write_all(&[1, 2, 3]).unwrap();
        assert!(
            inspect_model_file_with_minimum(&path, 4)
                .unwrap_err()
                .to_string()
                .contains("incomplete")
        );
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn model_inspection_returns_a_canonical_readable_file() {
        let path = std::env::temp_dir().join(format!(
            "sapodilla-readable-model-test-{}.onnx",
            std::process::id()
        ));
        File::create(&path).unwrap().write_all(&[1, 2, 3]).unwrap();
        let info = inspect_model_file_with_minimum(&path, 3).unwrap();
        assert!(info.path.is_absolute());
        assert_eq!(info.bytes, 3);
        std::fs::remove_file(path).unwrap();
    }
}
