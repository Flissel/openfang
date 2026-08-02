//! HTTP coverage for the OpenAI-compatible embeddings route registration.

use openfang_api::server;
use openfang_kernel::OpenFangKernel;
use openfang_types::config::{DefaultModelConfig, KernelConfig, MemoryConfig};
use std::collections::HashMap;
use std::sync::Arc;

#[tokio::test]
async fn embeddings_route_is_registered_and_fails_closed_without_openai_config() {
    let temp_dir = tempfile::tempdir().expect("temporary OpenFang home should exist");
    let config = KernelConfig {
        home_dir: temp_dir.path().to_path_buf(),
        data_dir: temp_dir.path().join("data"),
        default_model: DefaultModelConfig {
            provider: "ollama".to_string(),
            model: "test-model".to_string(),
            api_key_env: "OLLAMA_API_KEY".to_string(),
            base_url: None,
            ..Default::default()
        },
        api_key: "test-route-key".to_string(),
        ..KernelConfig::default()
    };
    let kernel = Arc::new(OpenFangKernel::boot_with_config(config).expect("kernel should boot"));
    kernel.set_self_handle();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("test listener should have an address");
    let (app, _state) = server::build_router(kernel.clone(), address).await;
    let server_task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("test server should run");
    });

    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/embeddings"))
        .bearer_auth("test-route-key")
        .json(&serde_json::json!({
            "model": "text-embedding-3-small",
            "input": "route must exist"
        }))
        .send()
        .await
        .expect("test server should respond");

    assert_eq!(response.status(), 503);
    let body: serde_json::Value = response.json().await.expect("error body should be JSON");
    assert_eq!(body["error"]["code"], "embedding_provider_not_configured");

    kernel.shutdown();
    server_task.abort();
}

#[tokio::test]
async fn embeddings_route_uses_the_configured_openai_driver_for_a_batch() {
    const TEST_KEY_ENV: &str = "OPENFANG_EMBEDDINGS_ROUTE_TEST_KEY";
    unsafe { std::env::set_var(TEST_KEY_ENV, "test-openai-key") };

    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock OpenAI listener should bind");
    let upstream_address = upstream_listener
        .local_addr()
        .expect("mock OpenAI listener should have an address");
    let upstream_app = axum::Router::new().route(
        "/v1/embeddings",
        axum::routing::post(|| async {
            axum::Json(serde_json::json!({
                "data": [
                    {"embedding": [0.25, -0.5]},
                    {"embedding": [1.0, 0.0]}
                ]
            }))
        }),
    );
    let upstream_task = tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("mock OpenAI server should run");
    });

    let temp_dir = tempfile::tempdir().expect("temporary OpenFang home should exist");
    let config = KernelConfig {
        home_dir: temp_dir.path().to_path_buf(),
        data_dir: temp_dir.path().join("data"),
        default_model: DefaultModelConfig {
            provider: "ollama".to_string(),
            model: "test-model".to_string(),
            api_key_env: "OLLAMA_API_KEY".to_string(),
            base_url: None,
            ..Default::default()
        },
        memory: MemoryConfig {
            embedding_model: "text-embedding-3-small".to_string(),
            embedding_provider: Some("openai".to_string()),
            embedding_api_key_env: Some(TEST_KEY_ENV.to_string()),
            ..MemoryConfig::default()
        },
        provider_urls: HashMap::from([(
            "openai".to_string(),
            format!("http://{upstream_address}/v1"),
        )]),
        api_key: "test-route-key".to_string(),
        ..KernelConfig::default()
    };
    let kernel = Arc::new(OpenFangKernel::boot_with_config(config).expect("kernel should boot"));
    kernel.set_self_handle();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("test listener should have an address");
    let (app, _state) = server::build_router(kernel.clone(), address).await;
    let server_task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("test server should run");
    });

    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/embeddings"))
        .bearer_auth("test-route-key")
        .json(&serde_json::json!({
            "model": "text-embedding-3-small",
            "input": ["first", "second"]
        }))
        .send()
        .await
        .expect("test server should respond");

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.expect("response should be JSON");
    assert_eq!(body["object"], "list");
    assert_eq!(body["model"], "text-embedding-3-small");
    assert_eq!(body["data"][0]["index"], 0);
    assert_eq!(
        body["data"][0]["embedding"],
        serde_json::json!([0.25, -0.5])
    );
    assert_eq!(body["data"][1]["index"], 1);
    assert_eq!(body["usage"]["prompt_tokens"], 0);

    kernel.shutdown();
    server_task.abort();
    upstream_task.abort();
    unsafe { std::env::remove_var(TEST_KEY_ENV) };
}

#[tokio::test]
async fn embeddings_route_rejects_openai_provider_with_the_local_default_model() {
    const TEST_KEY_ENV: &str = "OPENFANG_EMBEDDINGS_DEFAULT_MODEL_TEST_KEY";
    unsafe { std::env::set_var(TEST_KEY_ENV, "test-openai-key") };

    let temp_dir = tempfile::tempdir().expect("temporary OpenFang home should exist");
    let config = KernelConfig {
        home_dir: temp_dir.path().to_path_buf(),
        data_dir: temp_dir.path().join("data"),
        default_model: DefaultModelConfig {
            provider: "ollama".to_string(),
            model: "test-model".to_string(),
            api_key_env: "OLLAMA_API_KEY".to_string(),
            base_url: None,
            ..Default::default()
        },
        memory: MemoryConfig {
            embedding_provider: Some("openai".to_string()),
            embedding_api_key_env: Some(TEST_KEY_ENV.to_string()),
            ..MemoryConfig::default()
        },
        provider_urls: HashMap::from([("openai".to_string(), "http://127.0.0.1:1/v1".to_string())]),
        api_key: "test-route-key".to_string(),
        ..KernelConfig::default()
    };
    let kernel = Arc::new(OpenFangKernel::boot_with_config(config).expect("kernel should boot"));
    kernel.set_self_handle();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("test listener should have an address");
    let (app, _state) = server::build_router(kernel.clone(), address).await;
    let server_task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("test server should run");
    });

    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/embeddings"))
        .bearer_auth("test-route-key")
        .json(&serde_json::json!({
            "model": "text-embedding-3-small",
            "input": "must not substitute all-MiniLM-L6-v2"
        }))
        .send()
        .await
        .expect("test server should respond");

    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.expect("error body should be JSON");
    assert_eq!(body["error"]["code"], "embedding_model_not_configured");

    kernel.shutdown();
    server_task.abort();
    unsafe { std::env::remove_var(TEST_KEY_ENV) };
}
