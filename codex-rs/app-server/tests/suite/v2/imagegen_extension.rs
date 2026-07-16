#[cfg(windows)]
use std::io::Read;
#[cfg(windows)]
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use app_test_support::ChatGptAuthFixture;
use app_test_support::TestAppServer;
use app_test_support::to_response;
use app_test_support::write_chatgpt_auth;
#[cfg(windows)]
use base64::Engine;
#[cfg(windows)]
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_app_server_protocol::ImageGenerationItem;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadItem;
#[cfg(windows)]
use codex_app_server_protocol::ThreadReadParams;
#[cfg(windows)]
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_config::types::AuthCredentialsStoreMode;
use core_test_support::responses;
use core_test_support::skip_if_remote;
#[cfg(windows)]
use flate2::Compression;
#[cfg(windows)]
use flate2::read::ZlibDecoder;
#[cfg(windows)]
use flate2::write::ZlibEncoder;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const RESULT: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
const TINY_PNG_BYTES: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 240, 31, 0,
    5, 0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];
const TINY_PNG_DATA_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

#[derive(Clone, Copy)]
enum ImagegenTestMode {
    Direct,
    CodeModeOnly,
    AgentRouterApiKey,
    AgentRouterCodeModeOnly,
}

// macOS and Windows Bazel CI can spend tens of seconds starting app-server
// subprocesses or processing test RPCs under load.
#[cfg(any(target_os = "macos", windows))]
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(not(any(target_os = "macos", windows)))]
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(windows)]
const SLOW_IMAGE_READ_TIMEOUT: Duration = Duration::from_secs(120);

#[tokio::test]
async fn standalone_image_generation_returns_saved_path_hint_to_model() -> Result<()> {
    let call_id = "image-run-1";
    let server = responses::start_mock_server().await;
    mount_image_response(&server).await;

    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-1"),
                responses::ev_function_call_with_namespace(
                    call_id,
                    "image_gen",
                    "imagegen",
                    &json!({
                        "prompt": "paint a blue whale",
                    })
                    .to_string(),
                ),
                responses::ev_completed("resp-1"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("msg-1", "Done"),
                responses::ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri(), ImagegenTestMode::Direct)?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("access-chatgpt"),
        AuthCredentialsStoreMode::File,
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;
    start_image_generation_turn(&mut mcp).await?;

    let completed = timeout(
        DEFAULT_READ_TIMEOUT,
        wait_for_image_generation_completed(&mut mcp),
    )
    .await??;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let ThreadItem::ImageGeneration(ImageGenerationItem {
        status,
        revised_prompt,
        result,
        saved_path: Some(saved_path),
        ..
    }) = completed.item
    else {
        panic!("expected completed image generation item with saved path");
    };
    assert_eq!(status, "completed");
    assert_eq!(revised_prompt.as_deref(), Some("paint a blue whale"));
    assert_eq!(result, RESULT);
    assert_eq!(std::fs::read(&saved_path)?, TINY_PNG_BYTES);

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    let output = requests[1].function_call_output(call_id);
    assert_eq!(
        output["output"][0],
        json!({
            "type": "input_image",
            "image_url": format!("data:image/png;base64,{RESULT}"),
            "detail": "high",
        })
    );
    let output_hint = output["output"][1]["text"]
        .as_str()
        .context("image output should include model-visible path hint")?;
    assert!(
        output_hint.contains(&saved_path.display().to_string()),
        "output hint should identify the path the extension saved"
    );
    assert!(
        !requests[1]
            .message_input_texts("developer")
            .iter()
            .any(|text| text.contains("Generated images are saved to")),
        "standalone image generation should not emit the legacy developer-message hint"
    );

    Ok(())
}

#[tokio::test]
async fn agentrouter_api_key_executes_standalone_image_generation() -> Result<()> {
    let call_id = "agentrouter-image-run-1";
    let server = responses::start_mock_server().await;
    mount_image_response(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/codex/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "models": [] })))
        .mount(&server)
        .await;

    responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-1"),
                responses::ev_function_call_with_namespace(
                    call_id,
                    "image_gen",
                    "imagegen",
                    &json!({
                        "prompt": "paint an AgentRouter lighthouse",
                    })
                    .to_string(),
                ),
                responses::ev_completed("resp-1"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("msg-1", "Done"),
                responses::ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        &server.uri(),
        ImagegenTestMode::AgentRouterApiKey,
    )?;
    std::fs::write(
        codex_home.path().join("auth.json"),
        r#"{"auth_mode":"apikey","OPENAI_API_KEY":"test-agentrouter-key"}"#,
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;
    start_image_generation_turn(&mut mcp).await?;

    let completed = timeout(
        DEFAULT_READ_TIMEOUT,
        wait_for_image_generation_completed(&mut mcp),
    )
    .await??;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let ThreadItem::ImageGeneration(ImageGenerationItem {
        status,
        saved_path: Some(saved_path),
        ..
    }) = completed.item
    else {
        panic!("expected completed AgentRouter image generation item with saved path");
    };
    assert_eq!(status, "completed");
    assert_eq!(std::fs::read(saved_path)?, TINY_PNG_BYTES);

    Ok(())
}

#[tokio::test]
async fn standalone_image_generation_failure_emits_terminal_item() -> Result<()> {
    let call_id = "image-run-failed";
    let server = responses::start_mock_server().await;
    Mock::given(method("POST"))
        .and(path("/api/codex/images/generations"))
        .respond_with(ResponseTemplate::new(500).set_body_string("image backend failed"))
        .expect(1)
        .mount(&server)
        .await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-1"),
                responses::ev_function_call_with_namespace(
                    call_id,
                    "image_gen",
                    "imagegen",
                    &json!({"prompt": "paint a blue whale"}).to_string(),
                ),
                responses::ev_completed("resp-1"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("msg-1", "I could not generate the image."),
                responses::ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri(), ImagegenTestMode::Direct)?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("access-chatgpt"),
        AuthCredentialsStoreMode::File,
    )?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;
    start_image_generation_turn(&mut mcp).await?;

    let completed = timeout(
        DEFAULT_READ_TIMEOUT,
        wait_for_image_generation_completed(&mut mcp),
    )
    .await??;
    assert_eq!(
        completed.item,
        ThreadItem::ImageGeneration(ImageGenerationItem {
            id: call_id.to_string(),
            status: "failed".to_string(),
            revised_prompt: Some("paint a blue whale".to_string()),
            result: String::new(),
            saved_path: None,
        })
    );

    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    let (output, _) = requests[1]
        .function_call_output_content_and_success(call_id)
        .context("image generation function output should be present")?;
    assert!(
        output
            .as_deref()
            .is_some_and(|text| text.contains("image generation failed"))
    );

    Ok(())
}

#[tokio::test]
async fn standalone_image_edit_uses_attached_model_visible_image() -> Result<()> {
    skip_if_remote!(
        Ok(()),
        "remote executors use different imagegen storage approaches, so host-local image paths are unavailable"
    );

    let edit_request = run_image_edit_test(|codex_home| {
        let image_path = codex_home.join("attached.png");
        std::fs::write(&image_path, TINY_PNG_BYTES)?;
        Ok((
            json!({
                "prompt": "add a red hat",
                "referenced_image_paths": [image_path.display().to_string()],
            }),
            vec![
                V2UserInput::Text {
                    text: "Edit the attached image".to_string(),
                    text_elements: Vec::new(),
                },
                V2UserInput::LocalImage {
                    path: image_path,
                    detail: None,
                },
            ],
        ))
    })
    .await?;
    assert_eq!(edit_request["prompt"], "add a red hat");
    assert_eq!(edit_request["images"][0]["image_url"], TINY_PNG_DATA_URL);

    Ok(())
}

#[tokio::test]
async fn standalone_image_edit_uses_recent_pathless_image() -> Result<()> {
    let image_url = TINY_PNG_DATA_URL;
    let edit_request = run_image_edit_test(|_| {
        Ok((
            json!({
                "prompt": "add a red hat",
                "num_last_images_to_include": 1,
            }),
            vec![
                V2UserInput::Text {
                    text: "Edit the attached image".to_string(),
                    text_elements: Vec::new(),
                },
                V2UserInput::Image {
                    url: image_url.to_string(),
                    detail: None,
                },
            ],
        ))
    })
    .await?;
    assert_eq!(edit_request["prompt"], "add a red hat");
    assert_eq!(edit_request["images"][0]["image_url"], image_url);

    Ok(())
}

#[tokio::test]
async fn standalone_image_generation_is_exposed_directly_in_code_mode_only() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("msg-1", "Done"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        &server.uri(),
        ImagegenTestMode::CodeModeOnly,
    )?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("access-chatgpt"),
        AuthCredentialsStoreMode::File,
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;
    start_image_generation_turn(&mut mcp).await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let request = response_mock.single_request();
    assert!(request.tool_by_name("image_gen", "imagegen").is_some());
    assert!(!request.body_contains_text("image_gen__imagegen"));

    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_only_direct_image_generation_survives_slow_large_backend() -> Result<()> {
    let call_id = "slow-large-image-run-1";
    let server = responses::start_mock_server().await;
    let png = large_png_fixture()?;
    let result = BASE64_STANDARD.encode(&png);
    mount_delayed_image_response(&server, &result, Duration::from_secs(75)).await;
    Mock::given(method("GET"))
        .and(path("/api/codex/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "models": [] })))
        .mount(&server)
        .await;

    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-1"),
                responses::ev_function_call_with_namespace(
                    call_id,
                    "image_gen",
                    "imagegen",
                    &json!({"prompt": "paint a lighthouse in a storm"}).to_string(),
                ),
                responses::ev_completed("resp-1"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("msg-1", "Done"),
                responses::ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        &server.uri(),
        ImagegenTestMode::AgentRouterCodeModeOnly,
    )?;
    std::fs::write(
        codex_home.path().join("auth.json"),
        r#"{"auth_mode":"apikey","OPENAI_API_KEY":"test-agentrouter-key"}"#,
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;
    let thread_id = start_image_generation_turn(&mut mcp).await?;

    let completed = timeout(
        SLOW_IMAGE_READ_TIMEOUT,
        wait_for_image_generation_completed(&mut mcp),
    )
    .await??;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let ThreadItem::ImageGeneration(ImageGenerationItem {
        status,
        result: completed_result,
        saved_path: Some(saved_path),
        ..
    }) = completed.item
    else {
        panic!("expected completed image generation item with saved path");
    };
    assert_eq!(status, "completed");
    assert!(!completed_result.is_empty());
    assert_eq!(completed_result, result);
    let saved_png = std::fs::read(&saved_path)?;
    assert_eq!(saved_png, png);
    assert_valid_png(&saved_png, 640, 640)?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].tool_by_name("image_gen", "imagegen").is_some());
    assert!(!requests[0].body_contains_text("image_gen__imagegen"));
    assert!(
        requests[0].body_json()["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "exec")),
        "other tools should remain behind the code-mode executor"
    );

    let read_id = mcp
        .send_thread_read_request(ThreadReadParams {
            thread_id,
            include_turns: true,
        })
        .await?;
    let read_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(read_id)),
    )
    .await??;
    let ThreadReadResponse { thread } = to_response::<ThreadReadResponse>(read_response)?;
    let persisted = thread
        .turns
        .iter()
        .flat_map(|turn| &turn.items)
        .find(|item| matches!(item, ThreadItem::ImageGeneration(image) if image.id == call_id))
        .context("thread/read should preserve the imageGeneration item for Shell rendering")?;
    let ThreadItem::ImageGeneration(persisted_image) = persisted else {
        unreachable!("matching item should be image generation");
    };
    assert_eq!(persisted_image.status, "completed");
    assert_eq!(persisted_image.saved_path.as_ref(), Some(&saved_path));
    let persisted_wire_item = serde_json::to_value(persisted)?;
    assert_eq!(persisted_wire_item["type"], "imageGeneration");
    assert_eq!(
        persisted_wire_item["savedPath"],
        saved_path.display().to_string()
    );

    Ok(())
}

async fn start_image_generation_turn(mcp: &mut TestAppServer) -> Result<String> {
    start_turn(
        mcp,
        vec![V2UserInput::Text {
            text: "Generate an image".to_string(),
            text_elements: Vec::new(),
        }],
    )
    .await
}

async fn run_image_edit_test(
    input: impl FnOnce(&Path) -> Result<(serde_json::Value, Vec<V2UserInput>)>,
) -> Result<serde_json::Value> {
    let call_id = "image-edit-1";
    let server = responses::start_mock_server().await;
    mount_image_edit_response(&server).await;

    let codex_home = TempDir::new()?;
    let (arguments, input) = input(codex_home.path())?;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-1"),
                responses::ev_function_call_with_namespace(
                    call_id,
                    "image_gen",
                    "imagegen",
                    &arguments.to_string(),
                ),
                responses::ev_completed("resp-1"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("msg-1", "Done"),
                responses::ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    create_config_toml(codex_home.path(), &server.uri(), ImagegenTestMode::Direct)?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("access-chatgpt"),
        AuthCredentialsStoreMode::File,
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;
    start_turn(&mut mcp, input).await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        wait_for_image_generation_completed(&mut mcp),
    )
    .await??;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    assert_eq!(response_mock.requests().len(), 2);
    let requests = server
        .received_requests()
        .await
        .context("failed to fetch received requests")?;
    Ok(requests
        .iter()
        .find(|request| request.url.path() == "/api/codex/images/edits")
        .context("image edit request should be sent")?
        .body_json::<serde_json::Value>()?)
}

async fn start_turn(mcp: &mut TestAppServer, input: Vec<V2UserInput>) -> Result<String> {
    let thread_req = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let thread_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_req)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_resp)?;
    let thread_id = thread.id;

    let turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread_id.clone(),
            client_user_message_id: None,
            input,
            ..Default::default()
        })
        .await?;
    let turn_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_req)),
    )
    .await??;
    let _turn: TurnStartResponse = to_response::<TurnStartResponse>(turn_resp)?;

    Ok(thread_id)
}

async fn wait_for_image_generation_completed(
    mcp: &mut TestAppServer,
) -> Result<ItemCompletedNotification> {
    loop {
        let notification = mcp
            .read_stream_until_notification_message("item/completed")
            .await?;
        let completed: ItemCompletedNotification = serde_json::from_value(
            notification
                .params
                .context("item/completed notification should include params")?,
        )?;
        if matches!(&completed.item, ThreadItem::ImageGeneration(_)) {
            return Ok(completed);
        }
    }
}

async fn mount_image_response(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/api/codex/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "created": 1,
            "data": [{"b64_json": RESULT}],
        })))
        .expect(1)
        .mount(server)
        .await;
}

#[cfg(windows)]
async fn mount_delayed_image_response(server: &MockServer, result: &str, delay: Duration) {
    Mock::given(method("POST"))
        .and(path("/api/codex/images/generations"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(delay)
                .set_body_json(json!({
                    "created": 1,
                    "data": [{"b64_json": result}],
                })),
        )
        .expect(1)
        .mount(server)
        .await;
}

#[cfg(windows)]
fn large_png_fixture() -> Result<Vec<u8>> {
    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 640;
    let mut raw = Vec::with_capacity((HEIGHT * (1 + WIDTH * 4)) as usize);
    let mut state = 0x6d2b_79f5_u32;
    for _ in 0..HEIGHT {
        raw.resize(raw.len() + 1, 0);
        for _ in 0..WIDTH {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            raw.extend_from_slice(&state.to_be_bytes());
        }
    }

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&raw)?;
    let compressed = encoder.finish()?;

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&WIDTH.to_be_bytes());
    ihdr.extend_from_slice(&HEIGHT.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    append_png_chunk(&mut png, *b"IHDR", &ihdr);
    append_png_chunk(&mut png, *b"IDAT", &compressed);
    append_png_chunk(&mut png, *b"IEND", &[]);

    anyhow::ensure!(png.len() > 1024 * 1024, "PNG fixture should exceed 1 MiB");
    assert_valid_png(&png, WIDTH, HEIGHT)?;
    Ok(png)
}

#[cfg(windows)]
fn append_png_chunk(png: &mut Vec<u8>, chunk_type: [u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    png.extend_from_slice(&chunk_type);
    png.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(chunk_type.len() + data.len());
    crc_input.extend_from_slice(&chunk_type);
    crc_input.extend_from_slice(data);
    png.extend_from_slice(&png_crc32(&crc_input).to_be_bytes());
}

#[cfg(windows)]
fn png_crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

#[cfg(windows)]
fn assert_valid_png(png: &[u8], expected_width: u32, expected_height: u32) -> Result<()> {
    anyhow::ensure!(
        png.starts_with(b"\x89PNG\r\n\x1a\n"),
        "invalid PNG signature"
    );
    let mut offset = 8;
    let mut idat = Vec::new();
    let mut saw_ihdr = false;
    let mut saw_iend = false;
    while offset < png.len() {
        anyhow::ensure!(offset + 12 <= png.len(), "truncated PNG chunk");
        let length = u32::from_be_bytes(png[offset..offset + 4].try_into()?) as usize;
        let chunk_type: [u8; 4] = png[offset + 4..offset + 8].try_into()?;
        let data_start = offset + 8;
        let data_end = data_start + length;
        anyhow::ensure!(data_end + 4 <= png.len(), "truncated PNG chunk body");
        let expected_crc = u32::from_be_bytes(png[data_end..data_end + 4].try_into()?);
        anyhow::ensure!(
            png_crc32(&png[offset + 4..data_end]) == expected_crc,
            "invalid PNG chunk CRC"
        );
        match &chunk_type {
            b"IHDR" => {
                anyhow::ensure!(length == 13, "invalid IHDR length");
                let width = u32::from_be_bytes(png[data_start..data_start + 4].try_into()?);
                let height = u32::from_be_bytes(png[data_start + 4..data_start + 8].try_into()?);
                anyhow::ensure!(
                    width == expected_width && height == expected_height,
                    "unexpected PNG dimensions"
                );
                anyhow::ensure!(
                    png[data_start + 8..data_end] == [8, 6, 0, 0, 0],
                    "unexpected PNG encoding"
                );
                saw_ihdr = true;
            }
            b"IDAT" => idat.extend_from_slice(&png[data_start..data_end]),
            b"IEND" => {
                anyhow::ensure!(length == 0, "invalid IEND length");
                saw_iend = true;
            }
            _ => {}
        }
        offset = data_end + 4;
    }
    anyhow::ensure!(saw_ihdr && saw_iend && !idat.is_empty(), "incomplete PNG");

    let mut decoded = Vec::new();
    ZlibDecoder::new(idat.as_slice()).read_to_end(&mut decoded)?;
    let row_bytes = 1 + expected_width as usize * 4;
    anyhow::ensure!(
        decoded.len() == row_bytes * expected_height as usize,
        "unexpected decoded PNG size"
    );
    anyhow::ensure!(
        decoded.chunks_exact(row_bytes).all(|row| row[0] == 0),
        "unexpected PNG row filter"
    );
    Ok(())
}

async fn mount_image_edit_response(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/api/codex/images/edits"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "created": 1,
            "data": [{"b64_json": RESULT}],
        })))
        .expect(1)
        .mount(server)
        .await;
}

fn create_config_toml(
    codex_home: &Path,
    server_uri: &str,
    mode: ImagegenTestMode,
) -> std::io::Result<()> {
    let (provider_name, feature_config) = match mode {
        ImagegenTestMode::Direct => ("OpenAI", ""),
        ImagegenTestMode::CodeModeOnly => (
            "OpenAI",
            "code_mode_only = true\n\n[features.code_mode]\ndirect_only_tool_namespaces = [\"image_gen\"]",
        ),
        ImagegenTestMode::AgentRouterApiKey => ("AgentRouter", ""),
        ImagegenTestMode::AgentRouterCodeModeOnly => (
            "AgentRouter",
            "code_mode_only = true\n\n[features.code_mode]\ndirect_only_tool_namespaces = [\"image_gen\"]",
        ),
    };
    std::fs::write(
        codex_home.join("config.toml"),
        format!(
            r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "read-only"
model_provider = "openai-custom"
chatgpt_base_url = "{server_uri}"

[features]
{feature_config}

[model_providers.openai-custom]
name = "{provider_name}"
base_url = "{server_uri}/api/codex"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
supports_websockets = false
requires_openai_auth = true
"#
        ),
    )
}
