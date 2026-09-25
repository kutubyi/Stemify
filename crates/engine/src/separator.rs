use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ort::ep;
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;

pub const WIN: usize = 343_980;
pub const STEMS: [&str; 6] = ["drums", "bass", "other", "vocals", "guitar", "piano"];
pub const ALL_MASK: u32 = 0b11_1111;

pub fn stems_mask(names: &[String]) -> u32 {
    names.iter().filter_map(|name| STEMS.iter().position(|s| *s == name.as_str())).fold(0, |mask, i| mask | 1 << i)
}

fn oe<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn data_dir() -> PathBuf {
    std::env::var_os("STEMIFY_DATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("Stemify")))
        .unwrap_or_else(|| PathBuf::from("Stemify"))
}

fn init_ort(runtime_dir: &Path) -> Result<(), String> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| {
        let mut paths = vec![runtime_dir.to_path_buf()];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
        std::env::set_var("PATH", std::env::join_paths(paths).map_err(oe)?);

        let dll = runtime_dir.join("onnxruntime.dll");
        ort::init_from(&dll)
            .map_err(oe)?
            .with_execution_providers([ep::CUDA::default().build().error_on_failure()])
            .commit();
        Ok(())
    })
    .clone()
}

pub struct Separator {
    session: Session,
}

impl Separator {
    pub fn load() -> Result<Separator, String> {
        let data = data_dir();
        let model = data.join("models").join("htdemucs_6s.onnx");
        if !model.exists() {
            return Err(format!("The stem model was not found at {}.", model.display()));
        }
        init_ort(&data.join("runtime")).map_err(|e| format!("Could not start the GPU runtime: {e}"))?;

        let session = Session::builder()
            .map_err(oe)?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(oe)?
            .commit_from_file(&model)
            .map_err(oe)?;
        let mut separator = Separator { session };
        for _ in 0..2 {
            separator.separate(vec![0f32; 2 * WIN])?;
        }
        Ok(separator)
    }

    pub fn separate(&mut self, window: Vec<f32>) -> Result<Vec<f32>, String> {
        let tensor = Tensor::from_array(([1usize, 2, WIN], window)).map_err(oe)?;
        let outputs = self.session.run(ort::inputs!["mix" => tensor]).map_err(oe)?;
        let (_, stems) = outputs["stems"].try_extract_tensor::<f32>().map_err(oe)?;
        Ok(stems.to_vec())
    }
}
