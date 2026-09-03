use super::*;
use crate::types::Message;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn sse_body(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": { "content": text },
                "finish_reason": null
            }]
        })
    )
}

fn request() -> ChatRequest {
    ChatRequest {
        model: String::new(),
        messages: vec![Message::User {
            content: "hi".to_string(),
        }],
        temperature: None,
        top_p: None,
        frequency_penalty: None,
        presence_penalty: None,
        stream: None,
        enable_thinking: None,
        tools: None,
    }
}

#[tokio::test]
async fn test_llm_retry_backoff() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with({
            let calls = calls.clone();
            move |_req: &Request| {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                match n {
                    0 => ResponseTemplate::new(503),
                    1 => ResponseTemplate::new(429),
                    _ => ResponseTemplate::new(200).set_body_string(sse_body("ok")),
                }
            }
        })
        .mount(&server)
        .await;

    let client = ChatClient::new(server.uri(), "test-model".to_string());
    let reply = client.chat(&request()).await.unwrap();

    assert_eq!(reply.content, "ok");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn test_llm_retry_exhaustion() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |_req: &Request| ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let client = ChatClient::new(server.uri(), "test-model".to_string());
    let err = client.chat(&request()).await.unwrap_err();
    assert!(err.to_string().contains("503"));
}

#[tokio::test]
async fn test_llm_prefill_delay_triggers_initial_timeout_and_retries() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with({
            let calls = calls.clone();
            move |_req: &Request| {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    // Stalled prompt pre-fill: delay 1.5s (exceeding initial_timeout_secs of 1s)
                    ResponseTemplate::new(200)
                        .set_delay(Duration::from_millis(1500))
                        .set_body_string(sse_body("delayed"))
                } else {
                    // Quick success on retry 3
                    ResponseTemplate::new(200).set_body_string(sse_body("recovered"))
                }
            }
        })
        .mount(&server)
        .await;

    let client =
        ChatClient::new(server.uri(), "test-model".to_string()).with_initial_timeout_secs(1);
    let reply = client.chat(&request()).await.unwrap();

    assert_eq!(reply.content, "recovered");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn test_llm_on_delta_abort_during_prefill() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(1000))
                .set_body_string(sse_body("never seen")),
        )
        .mount(&server)
        .await;

    let client =
        ChatClient::new(server.uri(), "test-model".to_string()).with_initial_timeout_secs(10);

    let mut check_count = 0;
    let reply = client
        .chat_stream(&request(), |_delta| {
            check_count += 1;
            // Abort immediately on the 2nd polling tick (100ms in)
            check_count < 2
        })
        .await
        .unwrap();

    assert_eq!(reply.content, "");
    assert!(check_count >= 2);
}
