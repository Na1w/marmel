use anyhow::Result;
use std::io::ErrorKind;
use std::time::Duration;
use tokio::process::Command;
use tracing::{info, warn};

#[derive(Clone, Debug)]
pub struct KetchClient {
    pub bin_path: String,
}

impl KetchClient {
    pub fn new(bin_path: Option<String>) -> Self {
        let bin = bin_path
            .or_else(|| std::env::var("KETCH_BIN").ok())
            .unwrap_or_else(|| "ketch".to_string());

        Self { bin_path: bin }
    }

    /// Query Context7 library and framework documentation using `ketch docs`.
    pub async fn docs(
        &self,
        query: &str,
        library: Option<&str>,
        tokens: Option<u32>,
    ) -> Result<String> {
        info!(
            bin = %self.bin_path,
            query = %query,
            library = ?library,
            tokens = ?tokens,
            "Executing ketch docs"
        );

        let mut cmd = Command::new(&self.bin_path);
        cmd.arg("docs");
        cmd.arg(query);

        if let Some(lib) = library {
            cmd.arg("--library");
            cmd.arg(lib);
        }

        if let Some(tok) = tokens {
            cmd.arg("--tokens");
            cmd.arg(tok.to_string());
        }

        // Spawn with 30s timeout
        let child_res = cmd.output();
        let timeout_fut = tokio::time::timeout(Duration::from_secs(30), child_res).await;

        match timeout_fut {
            Ok(Ok(output)) => {
                if output.status.success() {
                    let text = String::from_utf8_lossy(&output.stdout).to_string();
                    if text.trim().is_empty() {
                        Ok(format!(
                            "No documentation results found via ketch for query \"{}\"{}",
                            query,
                            library
                                .map(|l| format!(" in library \"{l}\""))
                                .unwrap_or_default()
                        ))
                    } else {
                        let mut formatted = format!(
                            "# Ketch Documentation (Context7)\n\n**Query:** `{}`\n",
                            query
                        );
                        if let Some(lib) = library {
                            formatted.push_str(&format!("**Target Library:** `{}`\n\n", lib));
                        } else {
                            formatted.push('\n');
                        }
                        formatted.push_str(&text);
                        Ok(formatted)
                    }
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    warn!(
                        code = ?output.status.code(),
                        stderr = %stderr,
                        "ketch docs command returned non-zero exit code"
                    );
                    Ok(format!(
                        "⚠️ ketch docs returned exit status {}.\n\nError details:\n```\n{}\n```\nFalling back to workspace search.",
                        output.status,
                        stderr.trim()
                    ))
                }
            }
            Ok(Err(e)) if e.kind() == ErrorKind::NotFound => {
                warn!(bin = %self.bin_path, "ketch binary not found in PATH");
                Ok(self.format_not_installed_message(query, library))
            }
            Ok(Err(e)) => {
                warn!(error = %e, "Failed to spawn ketch CLI");
                Ok(format!(
                    "⚠️ Failed to spawn ketch CLI (`{}`): {}\n\nFalling back to workspace code inspection.",
                    self.bin_path, e
                ))
            }
            Err(_) => {
                warn!("ketch docs command timed out after 30s");
                Ok("⚠️ ketch docs query timed out after 30 seconds. Falling back to workspace search.".to_string())
            }
        }
    }

    fn format_not_installed_message(&self, query: &str, library: Option<&str>) -> String {
        let lib_info = library
            .map(|l| format!(" for library `{l}`"))
            .unwrap_or_default();

        format!(
            "⚠️ **ketch CLI is not installed or not in PATH** (checked `{}`).\n\n\
            **Graceful Degradation:**\n\
            Could not retrieve Context7 documentation for query \"{}\"{}.\n\n\
            To enable authoritative library documentation retrieval:\n\
            - **macOS (Homebrew):** `brew install 1broseidon/tap/ketch`\n\
            - **Go:** `go install github.com/1broseidon/ketch@latest`\n\
            - **Binary releases:** `https://github.com/1broseidon/ketch/releases`\n\
            - **Custom path:** set `KETCH_BIN=/path/to/ketch` in environment or configuration.\n\n\
            Please proceed with workspace code inspection (`read_file`, `grep_search`) or query SearchWala for general documentation.",
            self.bin_path, query, lib_info
        )
    }
}
