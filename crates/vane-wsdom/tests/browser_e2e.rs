//! Explicit real-browser coverage for the server-owned syscall bridge.
//!
//! Run with:
//! `cargo test -p vane-wsdom --features browser-e2e --test browser_e2e`.
//! Set `VANE_CHROME_BIN` when Chrome is not in its normal platform location.

use std::{
    env,
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    extract::{State, ws::WebSocketUpgrade},
    response::{Html, Response},
    routing::get,
};
use os_host_api::{HostApi, ImportManifest, LinkRecipe, TunnelResolution, TunneledHostApi};
use tokio::{
    net::TcpListener,
    sync::mpsc,
    time::{Duration, timeout},
};
use vane_riscv::{Mem, template::Flags};
use vane_wsdom::{GuestAbi, RunExit, serve_connection};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    Close(u32),
    Exit(u32),
}

struct RecordingHostApi {
    base: TunneledHostApi,
    calls: Arc<Mutex<Vec<Call>>>,
}

impl RecordingHostApi {
    fn new(calls: Arc<Mutex<Vec<Call>>>) -> Self {
        Self {
            base: TunneledHostApi::for_host(),
            calls,
        }
    }
}

impl HostApi for RecordingHostApi {
    fn import_manifest(&self) -> ImportManifest {
        self.base.import_manifest()
    }

    fn resolve_ambient(&self, guest_name: &str) -> Option<TunnelResolution> {
        self.base.resolve_ambient(guest_name)
    }

    fn link_recipe(&self) -> LinkRecipe {
        self.base.link_recipe()
    }

    fn close(&mut self, fd: u32) -> i64 {
        self.calls.lock().unwrap().push(Call::Close(fd));
        0
    }

    fn exit(&mut self, code: u32) -> i64 {
        self.calls.lock().unwrap().push(Call::Exit(code));
        0
    }
}

#[derive(Clone)]
struct TestState {
    abi: GuestAbi,
    calls: Arc<Mutex<Vec<Call>>>,
    result: mpsc::UnboundedSender<Result<RunExit, String>>,
    websocket_url: String,
}

async fn page(State(state): State<TestState>) -> Html<String> {
    // The page is the checked-in WSDOM browser servant. Replacing its demo URL
    // keeps the E2E lane exactly on the production WebSocket path.
    Html(
        include_str!("../examples/index.html")
            .replace("ws://localhost:4000/ws", &state.websocket_url),
    )
}

async fn websocket(State(state): State<TestState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| async move {
        let result = serve_connection(
            socket,
            program(state.abi),
            0,
            Flags::default(),
            vane_wsdom::DEFAULT_MEM_SIZE,
            state.abi,
            RecordingHostApi::new(state.calls),
            |_| false,
        )
        .await;
        let result = match result {
            wsdom_axum::Output::Done(result) => result.map_err(|error| error.to_string()),
            wsdom_axum::Output::ConnectionClosed => Err("browser closed its WebSocket".into()),
            wsdom_axum::Output::AxumError(error) => Err(error.to_string()),
            wsdom_axum::Output::WsdomError(error) => Err(format!("WSDOM error: {error:?}")),
        };
        let _ = state.result.send(result);
    })
}

fn write_words(words: &[u32]) -> Mem {
    let mut mem = Mem::default();
    for (word_index, word) in words.iter().copied().enumerate() {
        for (byte_index, byte) in word.to_le_bytes().into_iter().enumerate() {
            mem.write_byte((word_index * 4 + byte_index) as u64, byte);
        }
    }
    mem
}

// All three programs make a non-terminal close call, then exit. That proves
// the browser request -> asynchronous server marshal -> synchronous HostApi ->
// matching Promise resume path before asserting the terminal exit outcome.
fn program(abi: GuestAbi) -> Mem {
    match abi {
        GuestAbi::LinuxRv64 => write_words(&[
            0x0010_0513, // addi a0, x0, 1
            0x0390_0893, // addi a7, x0, 57 (close)
            0x0000_0073, // ecall
            0x0070_0513, // addi a0, x0, 7
            0x05d0_0893, // addi a7, x0, 93 (exit)
            0x0000_0073, // ecall
        ]),
        GuestAbi::LinuxAarch64 => write_words(&[
            0xd280_0020, // movz x0, #1
            0xd280_0728, // movz x8, #57 (close)
            0xd400_0001, // svc #0
            0xd280_00e0, // movz x0, #7
            0xd280_0ba8, // movz x8, #93 (exit)
            0xd400_0001, // svc #0
        ]),
        GuestAbi::DarwinAarch64 => write_words(&[
            0xd280_0020, // movz x0, #1
            0xd280_00d0, // movz x16, #6 (close)
            0xd400_0001, // svc #0
            0xd280_0120, // movz x0, #9
            0xd280_0030, // movz x16, #1 (exit)
            0xd400_0001, // svc #0
        ]),
    }
}

fn chrome_binary() -> PathBuf {
    if let Some(path) = env::var_os("VANE_CHROME_BIN") {
        return path.into();
    }
    #[cfg(target_os = "macos")]
    {
        return PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome");
    }
    #[cfg(not(target_os = "macos"))]
    {
        PathBuf::from("google-chrome")
    }
}

async fn run_in_chrome(abi: GuestAbi, exit_code: u32) {
    let (result_sender, mut result_receiver) = mpsc::unbounded_channel();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let state = TestState {
        abi,
        calls: calls.clone(),
        result: result_sender,
        websocket_url: format!("ws://{address}/ws"),
    };
    let app = Router::new()
        .route("/", get(page))
        .route("/ws", get(websocket))
        .with_state(state);
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let profile = env::temp_dir().join(format!(
        "vane-wsdom-e2e-{}-{}",
        std::process::id(),
        exit_code
    ));
    std::fs::create_dir_all(&profile).unwrap();
    let mut chrome = Command::new(chrome_binary())
        .args([
            "--headless=new",
            "--disable-gpu",
            "--no-first-run",
            "--no-default-browser-check",
            &format!("--user-data-dir={}", profile.display()),
            &format!("http://{address}/"),
        ])
        .spawn()
        .expect("launch headless Chrome; set VANE_CHROME_BIN if needed");

    let result = timeout(Duration::from_secs(20), result_receiver.recv()).await;
    let _ = chrome.kill();
    let _ = chrome.wait();
    server.abort();
    std::fs::remove_dir_all(&profile).unwrap();

    assert_eq!(
        result.expect("Chrome did not finish the WSDOM run in time"),
        Some(Ok(RunExit::Exit(exit_code)))
    );
    assert_eq!(
        *calls.lock().unwrap(),
        [Call::Close(1), Call::Exit(exit_code)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_chrome_services_rv64_and_aarch64_unix_syscalls() {
    run_in_chrome(GuestAbi::LinuxRv64, 7).await;
    run_in_chrome(GuestAbi::LinuxAarch64, 7).await;
    run_in_chrome(GuestAbi::DarwinAarch64, 9).await;
}
