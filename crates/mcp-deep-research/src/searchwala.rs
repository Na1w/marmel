use anyhow::{Context, Result};
use std::time::Duration;
use tracing::{info, warn};

use crate::models::{SearchWalaLlmConfig, SearchWalaRequest, SearchWalaResponse};

#[derive(Clone, Debug)]
pub struct SearchWalaClient {
    pub base_url: String,
    client: reqwest::Client,
}

impl SearchWalaClient {
    pub fn new(base_url: Option<String>) -> Self {
        let url = base_url
            .or_else(|| std::env::var("SEARCHWALA_URL").ok())
            .unwrap_or_else(|| "http://localhost:8000".to_string())
            .trim_end_matches('/')
            .to_string();

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_default();

        Self {
            base_url: url,
            client,
        }
    }

    /// Perform a broad web search querying 90+ search engines via SearchWala.
    pub async fn search(
        &self,
        query: &str,
        max_results: Option<usize>,
        focus_mode: Option<&str>,
    ) -> Result<String> {
        let endpoint = format!("{}/search", self.base_url);
        let req_body = SearchWalaRequest {
            query: query.to_string(),
            max_results: Some(max_results.unwrap_or(20)),
            focus_mode: focus_mode.map(str::to_string),
            llm: None,
            enable_copilot: Some(false),
        };

        info!(endpoint = %endpoint, query = %query, "Executing SearchWala search");

        let response = match self.client.post(&endpoint).json(&req_body).send().await {
            Ok(res) => res,
            Err(e) => {
                warn!(error = %e, "SearchWala connection failed");
                return Ok(self.format_unavailable_message(&e.to_string(), query));
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Ok(format!(
                "⚠️ SearchWala search returned HTTP status {status}.\n\nDetails: {body}\n\nFalling back to workspace search tools."
            ));
        }

        let resp: SearchWalaResponse = response
            .json()
            .await
            .context("failed to deserialize SearchWala search response")?;

        Ok(self.format_search_response(&resp))
    }

    /// Perform a deep iterative multi-source research workflow using SearchWala.
    pub async fn deep_research(
        &self,
        query: &str,
        max_results: Option<usize>,
        focus_mode: Option<&str>,
        llm_cfg: Option<SearchWalaLlmConfig>,
    ) -> Result<String> {
        let has_llm = llm_cfg.is_some();
        let endpoint = if has_llm {
            format!("{}/search/research-llm", self.base_url)
        } else {
            format!("{}/search", self.base_url)
        };

        let req_body = SearchWalaRequest {
            query: query.to_string(),
            max_results: Some(max_results.unwrap_or(50)),
            focus_mode: Some(focus_mode.unwrap_or("research").to_string()),
            llm: llm_cfg,
            enable_copilot: Some(true),
        };

        info!(endpoint = %endpoint, query = %query, "Executing SearchWala deep research");

        let response = match self.client.post(&endpoint).json(&req_body).send().await {
            Ok(res) => res,
            Err(e) => {
                warn!(error = %e, "SearchWala connection failed during deep research");
                return Ok(self.format_unavailable_message(&e.to_string(), query));
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Ok(format!(
                "⚠️ SearchWala deep research returned HTTP status {status}.\n\nDetails: {body}\n\nPlease check SearchWala logs or verify backend configuration."
            ));
        }

        let resp: SearchWalaResponse = response
            .json()
            .await
            .context("failed to deserialize SearchWala research response")?;

        Ok(self.format_research_response(&resp))
    }

    fn format_unavailable_message(&self, error: &str, query: &str) -> String {
        format!(
            "⚠️ **SearchWala is currently unavailable** at `{}`\n\
            Error: {}\n\n\
            **Graceful Degradation:**\n\
            SearchWala is not currently running or reachable for query \"{}\".\n\
            To enable full Deep Research:\n\
            1. Run SearchWala locally (e.g. `cargo run --release` in SearchWala, or `docker run -p 8000:8000 sandeepai369/searchwala`)\n\
            2. Or specify a remote instance with `SEARCHWALA_URL=http://<host>:<port>`\n\n\
            You can continue your investigation by inspecting workspace files (`read_file`, `grep_search`) or looking up library documentation via `ketch_docs`.",
            self.base_url, error, query
        )
    }

    fn format_search_response(&self, resp: &SearchWalaResponse) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "# SearchWala Search Results for: \"{}\"\n\n",
            resp.query
        ));
        out.push_str(&format!(
            "**Sources processed:** {} | **Engines queried:** {}\n\n",
            resp.sources_processed,
            resp.engine_stats.engines_queried.join(", ")
        ));

        if !resp.search_results.is_empty() {
            out.push_str("## Top Results\n\n");
            for (i, hit) in resp.search_results.iter().enumerate() {
                out.push_str(&format!(
                    "[{}] **{}** ({})\nURL: {}\n{}\n\n",
                    i + 1,
                    hit.title,
                    hit.engine,
                    hit.url,
                    hit.snippet.trim()
                ));
            }
        } else if !resp.results.is_empty() {
            out.push_str("## Scraped Sources\n\n");
            for (i, src) in resp.results.iter().enumerate() {
                let snippet: String = src.extracted_text.chars().take(300).collect();
                out.push_str(&format!(
                    "[{}] **{}** ({})\nURL: {}\n{}...\n\n",
                    i + 1,
                    src.title,
                    src.engine,
                    src.url,
                    snippet.trim()
                ));
            }
        } else {
            out.push_str("No results were returned by the queried search engines.\n");
        }

        out
    }

    fn format_research_response(&self, resp: &SearchWalaResponse) -> String {
        let mut out = String::new();
        out.push_str(&format!("# Deep Research Report: \"{}\"\n\n", resp.query));
        out.push_str(&format!(
            "**Sources processed:** {} | **Engines:** {} | **Elapsed:** {:.2}s\n\n",
            resp.sources_processed,
            resp.engine_stats.engines_queried.join(", "),
            resp.elapsed_seconds
        ));

        // If SearchWala returned a synthesized LLM answer, present it first
        if let Some(answer) = &resp.llm_answer {
            out.push_str("## Synthesis\n\n");
            out.push_str(answer.trim());
            out.push_str("\n\n");
        } else {
            // Perform iterative client-side synthesis over extracted sources
            out.push_str("## Iterative Multi-Source Synthesis\n\n");
            for (i, src) in resp.results.iter().enumerate() {
                let text_preview = src.extracted_text.trim();
                if text_preview.is_empty() {
                    continue;
                }
                let preview: String = text_preview.chars().take(400).collect();
                out.push_str(&format!(
                    "### [{}] {}\n**Source:** {}\n\n> {}\n\n",
                    i + 1,
                    src.title,
                    src.url,
                    preview.replace('\n', "\n> ")
                ));
            }
        }

        // Always provide the authoritative cited references list
        out.push_str("## Sources & Citations\n\n");
        let all_sources = if !resp.results.is_empty() {
            resp.results
                .iter()
                .enumerate()
                .map(|(i, s)| (i + 1, s.title.as_str(), s.url.as_str(), s.engine.as_str()))
                .collect::<Vec<_>>()
        } else {
            resp.search_results
                .iter()
                .enumerate()
                .map(|(i, s)| (i + 1, s.title.as_str(), s.url.as_str(), s.engine.as_str()))
                .collect::<Vec<_>>()
        };

        if all_sources.is_empty() {
            out.push_str("*(No direct source URLs recorded)*\n");
        } else {
            for (idx, title, url, engine) in all_sources {
                out.push_str(&format!("[{idx}] [{title}]({url}) — Engine: {engine}\n"));
            }
        }

        out
    }
}
