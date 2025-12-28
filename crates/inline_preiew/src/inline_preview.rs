//! Batched math rendering for LaTeX and Typst.

// TODO: Hash file content before rendering to check if we already did the job.

use std::num::NonZero;
use std::thread;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::io::Write;

use futures::stream::{self, StreamExt};
use smol::process::{self, Command};
use smol::io;
use tempfile::NamedTempFile;

#[derive(Debug, Clone, Copy)]
pub enum Backend {
    LaTeX,
    Typst
}

#[derive(Debug, Clone)]
pub struct RenderContext {
    preamble_latex: String,
    preamble_typst: String,
    postamble_latex: String,
    postamble_typst: String,
    latex_cmd: String,
    latex_svg_cmd: String,
    typst_cmd: String,
    max_parallel: usize,
}

#[derive(Debug, Clone)]
struct RenderCommand {
    program: String,
    args: Vec<String>,
}

impl RenderContext {
    pub fn new() -> Self {
        Self {
            preamble_latex: "\\documentclass{article}\n\\pagestyle{empty}\n\\begin{document}".into(),
            preamble_typst: "".into(),
            postamble_latex: "\\end{document}".into(),
            postamble_typst: "".into(),
            latex_cmd: "latex".into(),
            latex_svg_cmd: "dvisvgm".into(),
            typst_cmd: "typst".into(),
            max_parallel: thread::available_parallelism().map(NonZero::get).unwrap_or(1),
        }
    }

    fn render_cmd(&self, backend: Backend, input_path: &Path) -> (PathBuf, Vec<RenderCommand>) {
        let output_path = input_path.with_extension("svg");
        let input_str = input_path.to_string_lossy().to_string();
        let output_str = output_path.to_string_lossy().to_string();
        match backend {
            Backend::LaTeX => {
                let output_dir = input_path.parent().unwrap_or_else(|| Path::new("."));
                let output_dir_str = output_dir.to_string_lossy().to_string();
                let dvi_path = input_path.with_extension("dvi");
                let dvi_str = dvi_path.to_string_lossy().to_string();
                (
                    output_path,
                    vec![
                        RenderCommand {
                            program: self.latex_cmd.clone(),
                            args: vec![
                                "-interaction=nonstopmode".into(),
                                "-halt-on-error".into(),
                                "-output-directory".into(),
                                output_dir_str,
                                input_str.clone(),
                            ],
                        },
                        RenderCommand {
                            program: self.latex_svg_cmd.clone(),
                            args: vec![
                                "-o".into(),
                                output_str,
                                dvi_str,
                            ],
                        },
                    ],
                )
            }
            Backend::Typst => (
                output_path,
                vec![RenderCommand {
                    program: self.typst_cmd.clone(),
                    args: vec!["compile".into(), input_str, output_str],
                }],
            ),
        }
    }

    /// Creates temporary file with preamble and content to render.
    fn file_to_render(&self, backend: Backend, fragment_content: String) -> Result<NamedTempFile, io::Error> {
        let suffix = match backend {
            Backend::LaTeX => ".tex",
            Backend::Typst => ".typ",
        };
        let mut tmp = tempfile::Builder::new()
            .prefix("inline-preview-")
            .suffix(suffix)
            .tempfile()?;
        let preamble = match backend {
            Backend::LaTeX => &self.preamble_latex,
            Backend::Typst => &self.preamble_typst,
        };
        let postamble = match backend {
            Backend::LaTeX => &self.postamble_latex,
            Backend::Typst => &self.postamble_typst,
        };
        if !preamble.is_empty() {
            writeln!(tmp, "{}", preamble)?;
        }

        writeln!(tmp, "\n\n{}", fragment_content)?;
        if !postamble.is_empty() {
            writeln!(tmp, "\n{}", postamble)?;
        }

        Ok(tmp)
    }

    pub async fn render_one(&self, backend: Backend, fragment_content: String) -> Result<(PathBuf, process::Output), io::Error> {
        let input_file = self.file_to_render(backend, fragment_content)?;
        let (output_path, commands) = self.render_cmd(backend, input_file.path());
        let mut last_output = None;
        for command in commands {
            let output = Command::new(command.program)
                .args(command.args)
                .stdin(Stdio::null())
                .output()
                .await?;
            if !output.status.success() {
                return Ok((output_path, output));
            }
            last_output = Some(output);
        }

        if let Some(output) = last_output {
            Ok((output_path, output))
        } else {
            Err(io::Error::new(io::ErrorKind::Other, "missing render commands"))
        }
    }

    pub fn render_batch(
        &self,
        backend: Backend,
        fragments: Vec<String>,
    ) -> impl stream::Stream<Item = Result<(PathBuf, process::Output), io::Error>> + use<'_> {
        let max_parallel = self.max_parallel;
        stream::iter(fragments)
            .map(move |fragment| self.render_one(backend, fragment))
            .buffered(max_parallel)
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
        let fragments = vec![
            "$E=mc^2$".into(),
            "$a^2+b^2=c^2$".into(),
        ];

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
