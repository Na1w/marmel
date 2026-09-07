use anyhow::Result;
use mcp_deep_research::{KetchClient, McpServer, SearchWalaClient};
use tokio::io::BufReader;

#[tokio::main]
async fn main() -> Result<()> {
    // MCP communicates over stdio; ensure all diagnostic logs go strictly to stderr.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let searchwala = SearchWalaClient::new(None);
    let ketch = KetchClient::new(None);
    let server = McpServer::new(searchwala, ketch);

    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();

    server.run(stdin, stdout).await
}
