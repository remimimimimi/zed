//! Batched math rendering for LaTeX and Typst.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::thread;
use std::{fs, iter};

use futures::stream::{self, StreamExt};
use smol::io;
use smol::process::{self, Command};
use tempfile::TempDir;
use tracing::{debug, trace};

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum Backend {
    LaTeX,
    Typst,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderConfig {
    pub preamble_latex: String,
    pub preamble_typst: String,
    pub postamble_latex: String,
    pub postamble_typst: String,
    pub latex_cmd: String,
    pub latex_svg_cmd: String,
    pub typst_cmd: String,
    pub max_parallel: usize,
}

#[derive(Debug)]
pub struct RenderContext {
    config: RenderConfig,
    cache: Mutex<HashMap<CacheKey, process::Output>>,
    availability: Mutex<HashMap<String, bool>>,
    cache_dir: TempDir,
}

#[derive(Debug, Clone)]
struct RenderCommand {
    program: String,
    args: Vec<String>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    backend: Backend,
    hash: String,
}

#[derive(Debug, Clone)]
struct CachedPaths {
    input_path: PathBuf,
    output_path: PathBuf,
    extra_paths: Vec<PathBuf>,
}

impl CachedPaths {
    fn all_paths(&self) -> impl Iterator<Item = &PathBuf> {
        iter::once(&self.input_path)
            .chain(iter::once(&self.output_path))
            .chain(self.extra_paths.iter())
    }
}

impl RenderConfig {
    pub fn new() -> Self {
        Self {
            // Use standalone with zero border to eliminate padding around rendered math.
            preamble_latex: "\\documentclass[preview,border=0pt,12pt]{standalone}\n\\usepackage[T1]{fontenc}\n\\usepackage[utf8]{inputenc}\n\\usepackage{lmodern}\n\\usepackage{amsmath,amssymb}\n\\usepackage{mathtools}\n\\begin{document}".into(),
            preamble_typst: "#set page(width: auto, height: auto, margin: 0pt)\n".into(),
            postamble_latex: "\\end{document}".into(),
            postamble_typst: "".into(),
            latex_cmd: "latex".into(),
            latex_svg_cmd: "dvisvgm".into(),
            typst_cmd: "typst".into(),
            max_parallel: thread::available_parallelism().map(NonZero::get).unwrap_or(1),
        }
    }
}

impl RenderContext {
    pub fn new() -> Self {
        Self::with_config(RenderConfig::new())
    }

    pub fn with_config(config: RenderConfig) -> Self {
        let cache_dir = tempfile::Builder::new()
            .prefix("inline-preview-cache-")
            .tempdir()
            .expect("failed to create render cache dir");
        Self {
            config,
            cache: Mutex::new(HashMap::new()),
            availability: Mutex::new(HashMap::new()),
            cache_dir,
        }
    }

    pub fn config(&self) -> &RenderConfig {
        &self.config
    }

    pub fn set_config(&mut self, config: RenderConfig) {
        let preamble_changed = self.config.preamble_latex != config.preamble_latex
            || self.config.preamble_typst != config.preamble_typst;
        let postamble_changed = self.config.postamble_latex != config.postamble_latex
            || self.config.postamble_typst != config.postamble_typst;
        let commands_changed = self.config.latex_cmd != config.latex_cmd
            || self.config.latex_svg_cmd != config.latex_svg_cmd
            || self.config.typst_cmd != config.typst_cmd;
        if preamble_changed || postamble_changed || commands_changed {
            self.invalidate_cache("config change");
        }
        if commands_changed {
            self.availability.lock().unwrap().clear();
        }
        self.config = config;
    }

    pub fn backend_available(&self, backend: Backend) -> bool {
        match backend {
            Backend::LaTeX => {
                self.command_available(&self.config.latex_cmd)
                    && self.command_available(&self.config.latex_svg_cmd)
            }
            Backend::Typst => self.command_available(&self.config.typst_cmd),
        }
    }

    fn command_available(&self, command: &str) -> bool {
        let mut availability = self.availability.lock().unwrap();
        if let Some(&cached) = availability.get(command) {
            return cached;
        }
        let available = which::which(command).is_ok();
        availability.insert(command.to_string(), available);
        available
    }

    fn render_cmds(
        &self,
        backend: Backend,
        input_path: &Path,
        output_path: &Path,
    ) -> Vec<RenderCommand> {
        let input_str = input_path.to_string_lossy().to_string();
        let output_str = output_path.to_string_lossy().to_string();
        match backend {
            Backend::LaTeX => {
                let output_dir = input_path.parent().unwrap_or_else(|| Path::new("."));
                let output_dir_str = output_dir.to_string_lossy().to_string();
                let dvi_str = input_path
                    .with_extension("dvi")
                    .to_string_lossy()
                    .to_string();
                vec![
                    RenderCommand {
                        program: self.config.latex_cmd.clone(),
                        args: vec![
                            "-interaction=nonstopmode".into(),
                            "-halt-on-error".into(),
                            "-output-directory".into(),
                            output_dir_str,
                            input_str.clone(),
                        ],
                    },
                    RenderCommand {
                        program: self.config.latex_svg_cmd.clone(),
                        args: vec![
                            "--bbox=min".into(),
                            "--exact-bbox".into(),
                            "--no-fonts".into(),
                            "-o".into(),
                            output_str,
                            dvi_str,
                        ],
                    },
                ]
            }
            Backend::Typst => vec![RenderCommand {
                program: self.config.typst_cmd.clone(),
                args: vec!["compile".into(), input_str, output_str],
            }],
        }
    }

    fn cache_key(&self, backend: Backend, fragment_content: &str) -> CacheKey {
        CacheKey {
            backend,
            hash: fragment_hash(fragment_content),
        }
    }

    fn cached_paths(&self, backend: Backend, hash: &str) -> CachedPaths {
        let prefix = match backend {
            Backend::LaTeX => "latex",
            Backend::Typst => "typst",
        };
        let base_name = format!("{prefix}_{hash}");
        let cache_dir = self.cache_dir.path();
        let input_path = match backend {
            Backend::LaTeX => cache_dir.join(format!("{base_name}.tex")),
            Backend::Typst => cache_dir.join(format!("{base_name}.typ")),
        };
        let output_path = cache_dir.join(format!("{base_name}.svg"));
        let mut extra_paths = Vec::new();
        if backend == Backend::LaTeX {
            extra_paths.push(cache_dir.join(format!("{base_name}.dvi")));
            extra_paths.push(cache_dir.join(format!("{base_name}.aux")));
            extra_paths.push(cache_dir.join(format!("{base_name}.log")));
        }
        CachedPaths {
            input_path,
            output_path,
            extra_paths,
        }
    }

    /// Creates a file with preamble, fragment, and postamble to render.
    fn write_render_file(
        &self,
        backend: Backend,
        fragment_content: &str,
        input_path: &Path,
    ) -> Result<(), io::Error> {
        if let Some(parent) = input_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::File::create(input_path)?;
        let preamble = match backend {
            Backend::LaTeX => &self.config.preamble_latex,
            Backend::Typst => &self.config.preamble_typst,
        };
        let postamble = match backend {
            Backend::LaTeX => &self.config.postamble_latex,
            Backend::Typst => &self.config.postamble_typst,
        };
        if !preamble.is_empty() {
            writeln!(file, "{}", preamble)?;
        }

        writeln!(file, "\n\n{}", fragment_content)?;
        if !postamble.is_empty() {
            writeln!(file, "\n{}", postamble)?;
        }

        Ok(())
    }

    pub async fn render_one(
        &self,
        backend: Backend,
        fragment_content: String,
    ) -> Result<(PathBuf, process::Output), io::Error> {
        let key = self.cache_key(backend, &fragment_content);
        trace!(backend = ?backend, hash = %key.hash, "render request");
        let paths = self.cached_paths(backend, &key.hash);
        if let Some(output) = self.cached_output(&key, &paths.output_path) {
            debug!(backend = ?backend, hash = %key.hash, "render cache hit");
            return Ok((paths.output_path, output));
        }
        debug!(backend = ?backend, hash = %key.hash, "render cache miss");

        self.write_render_file(backend, &fragment_content, &paths.input_path)?;
        let commands = self.render_cmds(backend, &paths.input_path, &paths.output_path);
        let mut last_output = None;
        for command in commands {
            let output = Command::new(command.program)
                .args(command.args)
                .stdin(Stdio::null())
                .output()
                .await?;
            if !output.status.success() {
                return Ok((paths.output_path, output));
            }
            last_output = Some(output);
        }

        if let Some(output) = last_output {
            if paths.output_path.is_file() {
                let mut cache = self.cache.lock().unwrap();
                cache.insert(key, output.clone());
            }
            Ok((paths.output_path, output))
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "missing render commands",
            ))
        }
    }

    pub fn render_batch(
        &self,
        backend: Backend,
        fragments: Vec<String>,
    ) -> impl stream::Stream<Item = Result<(PathBuf, process::Output), io::Error>> + use<'_> {
        let max_parallel = self.config.max_parallel;
        stream::iter(fragments)
            .map(move |fragment| self.render_one(backend, fragment))
            .buffered(max_parallel)
    }

    fn cached_output(&self, key: &CacheKey, output_path: &Path) -> Option<process::Output> {
        let mut cache = self.cache.lock().unwrap();
        if let Some(output) = cache.get(key) {
            if output_path.is_file() {
                return Some(output.clone());
            }
            debug!(backend = ?key.backend, hash = %key.hash, "render cache entry missing on disk; evicting");
            cache.remove(key);
        }
        None
    }

    fn invalidate_cache(&self, reason: &'static str) {
        let keys = {
            let mut cache = self.cache.lock().unwrap();
            let keys = cache.keys().cloned().collect::<Vec<_>>();
            cache.clear();
            keys
        };
        debug!(reason, entries = keys.len(), "invalidating render cache");
        for key in keys {
            let paths = self.cached_paths(key.backend, &key.hash);
            for path in paths.all_paths() {
                let _ = remove_file_if_exists(path);
            }
        }
    }
}

impl Drop for RenderContext {
    fn drop(&mut self) {
        self.invalidate_cache("context drop");
    }
}

fn fragment_hash(fragment: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    fragment.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn remove_file_if_exists(path: &Path) -> Result<(), io::Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    #[ignore = "requires latex and dvisvgm binaries"]
    fn render_latex_batch() {
        let ctx = RenderContext::new();
        let fragments = vec!["$E=mc^2$".into(), "$a^2+b^2=c^2$".into()];

        let results = smol::block_on(
            ctx.render_batch(Backend::LaTeX, fragments)
                .collect::<Vec<_>>(),
        );

        assert_eq!(results.len(), 2);
        for result in results {
            let (path, output) = result.expect("latex render failed");
            assert!(
                output.status.success(),
                "stderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(path.is_file());
        }
    }

    #[test]
    #[ignore = "requires typst binary"]
    fn render_typst_batch() {
        let ctx = RenderContext::new();
        let fragments = vec!["Hello typst".into(), "Math: $E=m c^2$".into()];

        let results = smol::block_on(
            ctx.render_batch(Backend::Typst, fragments)
                .collect::<Vec<_>>(),
        );

        assert_eq!(results.len(), 2);
        for result in results {
            let (path, output) = result.expect("typst render failed");
            assert!(
                output.status.success(),
                "stderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(path.is_file());
        }
    }
}
