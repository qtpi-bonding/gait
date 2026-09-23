//! In-process GGUF generation through llama.cpp.
//!
//! `LlamaContext` is not `Send`; one named worker owns the model/context and
//! accepts jobs over a standard channel. Dropping a response stream cancels a
//! running job at its next token send, without leaving a daemon behind.

use async_trait::async_trait;
use gait_local::{GenerationError, GenerationRequest, GenerationStream, TextGenerator};
use std::path::PathBuf;

#[cfg(any(feature = "native", test))]
const QWEN_TOOL_OPEN: &str = "<tool_call>";
#[cfg(any(feature = "native", test))]
const QWEN_TOOL_CLOSE: &str = "</tool_call>";

#[cfg(any(feature = "native", test))]
#[derive(Debug, PartialEq)]
enum ScanOut {
    Text(String),
    Call(String),
}

/// Incrementally separates Qwen's `<tool_call>` spans from ordinary text.
/// Markers can straddle decoded-token boundaries, so it withholds only the
/// possible marker suffix and otherwise streams text immediately.
#[cfg(any(feature = "native", test))]
#[derive(Default)]
struct ToolCallScanner {
    pending: String,
    call: Option<String>,
}

#[cfg(any(feature = "native", test))]
impl ToolCallScanner {
    fn push(&mut self, text: &str) -> Vec<ScanOut> {
        self.pending.push_str(text);
        let mut out = Vec::new();
        loop {
            if let Some(mut call) = self.call.take() {
                if let Some(index) = self.pending.find(QWEN_TOOL_CLOSE) {
                    call.push_str(&self.pending[..index]);
                    self.pending.drain(..index + QWEN_TOOL_CLOSE.len());
                    out.push(ScanOut::Call(call));
                    continue;
                }
                let split = held_back_boundary(&self.pending, QWEN_TOOL_CLOSE.len());
                call.push_str(&self.pending[..split]);
                self.pending.drain(..split);
                self.call = Some(call);
                return out;
            }
            if let Some(index) = self.pending.find(QWEN_TOOL_OPEN) {
                if index > 0 {
                    out.push(ScanOut::Text(self.pending[..index].to_owned()));
                }
                self.pending.drain(..index + QWEN_TOOL_OPEN.len());
                self.call = Some(String::new());
                continue;
            }
            let split = held_back_boundary(&self.pending, QWEN_TOOL_OPEN.len());
            if split > 0 {
                out.push(ScanOut::Text(self.pending[..split].to_owned()));
                self.pending.drain(..split);
            }
            return out;
        }
    }

    /// A truncated tool span is ordinary text: never invent a tool call and
    /// never silently drop the model's words.
    fn finish(&mut self) -> Vec<ScanOut> {
        if let Some(call) = self.call.take() {
            let mut text = String::from(QWEN_TOOL_OPEN);
            text.push_str(&call);
            text.push_str(&std::mem::take(&mut self.pending));
            vec![ScanOut::Text(text)]
        } else if self.pending.is_empty() {
            vec![]
        } else {
            vec![ScanOut::Text(std::mem::take(&mut self.pending))]
        }
    }
}

#[cfg(any(feature = "native", test))]
fn held_back_boundary(text: &str, marker_len: usize) -> usize {
    let wanted = text.len().saturating_sub(marker_len.saturating_sub(1));
    let mut boundary = wanted;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

#[cfg(any(feature = "native", test))]
fn qwen_tool_prompt(request: &GenerationRequest) -> Result<String, GenerationError> {
    let mut prompt = String::new();
    prompt.push_str("<|im_start|>system\n");
    prompt.push_str(&request.system);
    prompt.push_str("\n\n# Tools\n\nYou may call one or more functions to assist with the user query.\n\n<tools>\n");
    for tool in &request.tools {
        let definition = serde_json::json!({
            "type": "function",
            "function": {
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
            }
        });
        prompt.push_str(&serde_json::to_string(&definition).map_err(|error| {
            GenerationError::InvalidRequest(format!("serialize tool '{}': {error}", tool.name))
        })?);
        prompt.push('\n');
    }
    prompt.push_str("</tools>\n\nFor each function call, return a JSON object inside <tool_call> and </tool_call> tags:\n<tool_call>\n{\"name\": \"function-name\", \"arguments\": {}}\n</tool_call><|im_end|>\n");
    for message in &request.messages {
        match message {
            gait_local::GenerationMessage::User { content } => {
                prompt.push_str("<|im_start|>user\n");
                prompt.push_str(content);
                prompt.push_str("<|im_end|>\n");
            }
            gait_local::GenerationMessage::Assistant {
                content,
                tool_calls,
            } => {
                prompt.push_str("<|im_start|>assistant\n");
                prompt.push_str(content);
                for call in tool_calls {
                    let arguments: serde_json::Value = serde_json::from_str(&call.arguments)
                        .map_err(|error| {
                            GenerationError::InvalidRequest(format!(
                                "historical tool call '{}' has invalid arguments: {error}",
                                call.name
                            ))
                        })?;
                    // Written by hand so the key order is always Qwen's own
                    // `name`-then-`arguments`, whether or not serde_json's
                    // `preserve_order` feature is unified into the build.
                    let call = format!(
                        "{{\"name\":{},\"arguments\":{}}}",
                        serde_json::to_string(&call.name).expect("strings serialize"),
                        serde_json::to_string(&arguments).expect("JSON values serialize")
                    );
                    prompt.push_str(QWEN_TOOL_OPEN);
                    prompt.push_str(&call);
                    prompt.push_str(QWEN_TOOL_CLOSE);
                }
                prompt.push_str("<|im_end|>\n");
            }
            gait_local::GenerationMessage::ToolResult { content, .. } => {
                // Qwen's published tool convention returns results as a user
                // message containing a `tool_response` payload.
                prompt.push_str("<|im_start|>user\n<tool_response>\n");
                prompt.push_str(content);
                prompt.push_str("\n</tool_response><|im_end|>\n");
            }
        }
    }
    prompt.push_str("<|im_start|>assistant\n");
    Ok(prompt)
}

#[derive(Debug, Clone)]
pub struct LlamaConfig {
    pub model_path: PathBuf,
    pub context_tokens: u32,
    /// `Some(0)` forces CPU. `None` uses llama.cpp's platform default.
    pub gpu_layers: Option<u32>,
    /// Prompt/tool convention expected by this GGUF profile.
    pub tool_protocol: ToolProtocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolProtocol {
    /// Qwen2.5 Coder's documented ChatML tool convention.
    Qwen25Coder,
    /// Plain chat only; requests with tools are rejected rather than hidden.
    Unsupported,
}

impl ToolProtocol {
    fn as_str(self) -> &'static str {
        match self {
            Self::Qwen25Coder => "qwen25-coder",
            Self::Unsupported => "unsupported",
        }
    }

    #[cfg(feature = "native")]
    fn parse(value: &str) -> Result<Self, GenerationError> {
        match value {
            "qwen25-coder" => Ok(Self::Qwen25Coder),
            "unsupported" => Ok(Self::Unsupported),
            _ => Err(GenerationError::InvalidRequest(format!(
                "unknown local tool protocol '{value}'"
            ))),
        }
    }
}

impl LlamaConfig {
    pub fn runner_key(&self) -> String {
        gait_local::runner_key(&format!(
            "model={};ctx={};gpu={:?};tool={}",
            self.model_path.display(),
            self.context_tokens,
            self.gpu_layers,
            self.tool_protocol.as_str()
        ))
    }
}

#[cfg(feature = "native")]
mod native {
    use super::*;
    use futures_util::stream;
    use gait_local::GenerationChunk;
    use llama_cpp_2::{
        context::params::LlamaContextParams,
        context::LlamaContext,
        llama_backend::LlamaBackend,
        llama_batch::LlamaBatch,
        model::params::LlamaModelParams,
        model::{AddBos, LlamaChatMessage, LlamaModel},
        sampling::LlamaSampler,
        token::LlamaToken,
    };
    use std::num::NonZeroU32;
    use uuid::Uuid;

    struct Job {
        request: GenerationRequest,
        output: tokio::sync::mpsc::Sender<Result<GenerationChunk, GenerationError>>,
    }

    pub struct NativeGenerator {
        jobs: std::sync::mpsc::Sender<Job>,
    }

    impl NativeGenerator {
        pub fn new(config: LlamaConfig) -> Result<Self, GenerationError> {
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (jobs, rx) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("gait-llama-cpp".into())
                .spawn(move || worker(config, ready_tx, rx))
                .map_err(|e| GenerationError::Engine(format!("spawn llama worker: {e}")))?;
            ready_rx.recv().map_err(|_| {
                GenerationError::Engine("llama worker stopped during load".into())
            })??;
            Ok(Self { jobs })
        }
    }

    #[async_trait]
    impl TextGenerator for NativeGenerator {
        async fn generate(
            &self,
            request: GenerationRequest,
        ) -> Result<GenerationStream, GenerationError> {
            let (tx, rx) = tokio::sync::mpsc::channel(32);
            self.jobs
                .send(Job {
                    request,
                    output: tx,
                })
                .map_err(|_| GenerationError::Engine("llama worker is not running".into()))?;
            Ok(Box::pin(stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|chunk| (chunk, rx))
            })))
        }
    }

    fn worker(
        config: LlamaConfig,
        ready: std::sync::mpsc::Sender<Result<(), GenerationError>>,
        jobs: std::sync::mpsc::Receiver<Job>,
    ) {
        let tool_protocol = config.tool_protocol;
        let loaded =
            (|| -> Result<(&'static LlamaModel, LlamaContext<'static>), GenerationError> {
                let backend = Box::leak(Box::new(LlamaBackend::init().map_err(|e| {
                    GenerationError::Engine(format!("initialize llama backend: {e}"))
                })?));
                let mut params = LlamaModelParams::default();
                if let Some(layers) = config.gpu_layers {
                    params = params.with_n_gpu_layers(layers);
                }
                let model = LlamaModel::load_from_file(backend, &config.model_path, &params)
                    .map_err(|e| {
                        GenerationError::Unavailable(format!(
                            "load {}: {e}",
                            config.model_path.display()
                        ))
                    })?;
                let model: &'static LlamaModel = Box::leak(Box::new(model));
                let n_ctx =
                    NonZeroU32::new(config.context_tokens.max(512)).expect("max ensures nonzero");
                let context = model
                    .new_context(
                        backend,
                        LlamaContextParams::default()
                            .with_n_ctx(Some(n_ctx))
                            .with_n_batch(config.context_tokens.max(512))
                            .with_n_ubatch(config.context_tokens.clamp(1, 512)),
                    )
                    .map_err(|e| GenerationError::Engine(format!("create llama context: {e}")))?;
                Ok((model, context))
            })();
        let (model, mut context) = match loaded {
            Ok(value) => {
                let _ = ready.send(Ok(()));
                value
            }
            Err(error) => {
                let _ = ready.send(Err(error));
                return;
            }
        };
        for job in jobs {
            run_job(model, &mut context, tool_protocol, job);
        }
    }

    fn prompt(
        model: &LlamaModel,
        tool_protocol: ToolProtocol,
        request: &GenerationRequest,
    ) -> Result<String, GenerationError> {
        if !request.tools.is_empty() {
            // `llama-cpp-2` cannot bind the GGUF template's `tools` variable.
            // The first shipped profile is Qwen2.5 Coder, whose documented
            // ChatML tool protocol we render explicitly instead.
            return match tool_protocol {
                ToolProtocol::Qwen25Coder => qwen_tool_prompt(request),
                ToolProtocol::Unsupported => Err(GenerationError::InvalidRequest(
                    "this local GGUF profile does not declare a Gait tool-call protocol".into(),
                )),
            };
        }
        let mut messages = Vec::with_capacity(request.messages.len() + 1);
        if !request.system.is_empty() {
            messages.push(
                LlamaChatMessage::new("system".into(), request.system.clone()).map_err(|e| {
                    GenerationError::InvalidRequest(format!("system contains NUL: {e}"))
                })?,
            );
        }
        for message in &request.messages {
            let (role, content) = match message {
                gait_local::GenerationMessage::User { content } => ("user", content.clone()),
                gait_local::GenerationMessage::Assistant { content, .. } => {
                    ("assistant", content.clone())
                }
                gait_local::GenerationMessage::ToolResult { content, .. } => {
                    ("tool", content.clone())
                }
            };
            messages.push(LlamaChatMessage::new(role.into(), content).map_err(|e| {
                GenerationError::InvalidRequest(format!("message contains NUL: {e}"))
            })?);
        }
        let template = model.chat_template(None).map_err(|e| {
            GenerationError::InvalidRequest(format!("GGUF has no usable chat template: {e}"))
        })?;
        model
            .apply_chat_template(&template, &messages, true)
            .map_err(|e| GenerationError::InvalidRequest(format!("render chat template: {e}")))
    }

    fn piece(model: &LlamaModel, token: LlamaToken) -> Result<Vec<u8>, GenerationError> {
        match model.token_to_piece_bytes(token, 16, true, None) {
            Ok(bytes) => Ok(bytes),
            Err(llama_cpp_2::TokenToStringError::InsufficientBufferSpace(needed)) => model
                .token_to_piece_bytes(token, usize::try_from(-needed).unwrap_or(128), true, None)
                .map_err(|e| GenerationError::Engine(format!("detokenize: {e}"))),
            Err(e) => Err(GenerationError::Engine(format!("detokenize: {e}"))),
        }
    }

    fn run_job(
        model: &'static LlamaModel,
        context: &mut LlamaContext<'static>,
        tool_protocol: ToolProtocol,
        job: Job,
    ) {
        if job.output.is_closed() {
            return;
        }
        context.clear_kv_cache();
        let prompt = match prompt(model, tool_protocol, &job.request) {
            Ok(prompt) => prompt,
            Err(error) => {
                let _ = job.output.blocking_send(Err(error));
                return;
            }
        };
        let tokens = match model.str_to_token(&prompt, AddBos::Never) {
            Ok(tokens) => tokens,
            Err(e) => {
                let _ = job
                    .output
                    .blocking_send(Err(GenerationError::Engine(format!("tokenize: {e}"))));
                return;
            }
        };
        let reserve = job.request.max_tokens.unwrap_or(1024).saturating_add(16);
        if tokens.len() as u32 >= context.n_ctx().saturating_sub(reserve) {
            let _ = job
                .output
                .blocking_send(Err(GenerationError::InvalidRequest(format!(
                    "prompt is {} tokens but context leaves no room in {}",
                    tokens.len(),
                    context.n_ctx()
                ))));
            return;
        }
        let mut batch = LlamaBatch::new(context.n_batch() as usize, 1);
        let mut position = 0i32;
        for chunk in tokens.chunks(context.n_batch() as usize) {
            batch.clear();
            for (i, token) in chunk.iter().enumerate() {
                let logits = position as usize + i == tokens.len() - 1;
                if let Err(e) = batch.add(*token, position + i as i32, &[0], logits) {
                    let _ = job
                        .output
                        .blocking_send(Err(GenerationError::Engine(format!("batch: {e}"))));
                    return;
                }
            }
            if let Err(e) = context.decode(&mut batch) {
                let _ = job
                    .output
                    .blocking_send(Err(GenerationError::Engine(format!("prefill: {e}"))));
                return;
            }
            position += chunk.len() as i32;
        }
        let temperature = job.request.temperature.unwrap_or(0.2);
        let mut sampler = if temperature == 0.0 {
            LlamaSampler::chain_simple([LlamaSampler::greedy()])
        } else {
            LlamaSampler::chain_simple([
                LlamaSampler::top_k(40),
                LlamaSampler::top_p(job.request.top_p.unwrap_or(0.95), 1),
                LlamaSampler::temp(temperature),
                LlamaSampler::dist(0),
            ])
        };
        let mut utf8 = Vec::new();
        let mut scanner = (tool_protocol == ToolProtocol::Qwen25Coder
            && !job.request.tools.is_empty())
        .then(ToolCallScanner::default);
        let mut output_tokens = 0u32;
        let max = job.request.max_tokens.unwrap_or(1024);
        for _ in 0..max {
            let token = sampler.sample(context, -1);
            sampler.accept(token);
            if model.is_eog_token(token) {
                break;
            }
            output_tokens = output_tokens.saturating_add(1);
            let bytes = match piece(model, token) {
                Ok(value) => value,
                Err(error) => {
                    let _ = job.output.blocking_send(Err(error));
                    return;
                }
            };
            utf8.extend(bytes);
            if let Ok(text) = std::str::from_utf8(&utf8) {
                if !emit_text(model, &job.output, scanner.as_mut(), text) {
                    return;
                }
                utf8.clear();
            }
            batch.clear();
            if batch.add(token, position, &[0], true).is_err()
                || context.decode(&mut batch).is_err()
            {
                break;
            }
            position += 1;
        }
        if !utf8.is_empty() {
            let text = String::from_utf8_lossy(&utf8).into_owned();
            if !emit_text(model, &job.output, scanner.as_mut(), &text) {
                return;
            }
        }
        if let Some(scanner) = scanner.as_mut() {
            if !emit_scans(model, &job.output, scanner.finish()) {
                return;
            }
        }
        if job
            .output
            .blocking_send(Ok(GenerationChunk::Usage {
                input_tokens: tokens.len().try_into().unwrap_or(u32::MAX),
                output_tokens,
            }))
            .is_err()
        {
            return;
        }
        let _ = job.output.blocking_send(Ok(GenerationChunk::Done));
    }

    fn emit_text(
        _model: &LlamaModel,
        output: &tokio::sync::mpsc::Sender<Result<GenerationChunk, GenerationError>>,
        scanner: Option<&mut ToolCallScanner>,
        text: &str,
    ) -> bool {
        let scans = match scanner {
            Some(scanner) => scanner.push(text),
            None => vec![ScanOut::Text(text.to_owned())],
        };
        emit_scans(_model, output, scans)
    }

    fn emit_scans(
        _model: &LlamaModel,
        output: &tokio::sync::mpsc::Sender<Result<GenerationChunk, GenerationError>>,
        scans: Vec<ScanOut>,
    ) -> bool {
        for scan in scans {
            let chunk = match scan {
                ScanOut::Text(text) => GenerationChunk::Text(text),
                ScanOut::Call(raw) => match parse_tool_call(&raw) {
                    Ok(call) => GenerationChunk::ToolCall(call),
                    // Invalid marked-up output remains visible text. Treating
                    // it as a tool call would fabricate an unsafe action.
                    Err(_) => {
                        GenerationChunk::Text(format!("{QWEN_TOOL_OPEN}{raw}{QWEN_TOOL_CLOSE}"))
                    }
                },
            };
            if output.blocking_send(Ok(chunk)).is_err() {
                return false;
            }
        }
        true
    }

    fn parse_tool_call(raw: &str) -> Result<gait_local::GenerationToolCall, GenerationError> {
        let value: serde_json::Value = serde_json::from_str(raw).map_err(|error| {
            GenerationError::InvalidRequest(format!("Qwen tool call is not JSON: {error}"))
        })?;
        let name = value
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| GenerationError::InvalidRequest("Qwen tool call has no name".into()))?;
        let arguments = value.get("arguments").ok_or_else(|| {
            GenerationError::InvalidRequest("Qwen tool call has no arguments".into())
        })?;
        if !arguments.is_object() {
            return Err(GenerationError::InvalidRequest(
                "Qwen tool call arguments must be an object".into(),
            ));
        }
        Ok(gait_local::GenerationToolCall {
            id: format!("local-{}", Uuid::new_v4()),
            name: name.to_owned(),
            arguments: arguments.to_string(),
        })
    }
}

pub struct LlamaGenerator {
    #[cfg(feature = "native")]
    inner: native::NativeGenerator,
    #[cfg(not(feature = "native"))]
    config: LlamaConfig,
}

/// Client for the deliberately narrow warm-model sidecar. It owns no model
/// itself: every Gait brain can die while the runner retains only GGUF weights.
#[cfg(feature = "native")]
pub struct RunnerGenerator {
    config: LlamaConfig,
    socket: PathBuf,
}

#[cfg(feature = "native")]
impl RunnerGenerator {
    pub fn new(config: LlamaConfig, gait_dir: PathBuf) -> Result<Self, GenerationError> {
        if !config.model_path.is_file() {
            return Err(GenerationError::Unavailable(format!(
                "GGUF model does not exist: {}",
                config.model_path.display()
            )));
        }
        Ok(Self {
            socket: gait_dir
                .join("local-runners")
                .join(format!("{}.sock", config.runner_key())),
            config,
        })
    }

    async fn connect_or_start(&self) -> Result<tokio::net::UnixStream, GenerationError> {
        if let Ok(stream) = tokio::net::UnixStream::connect(&self.socket).await {
            return Ok(stream);
        }
        let executable = std::env::current_exe()
            .map_err(|e| GenerationError::Engine(format!("find gait executable: {e}")))?;
        let parent = self
            .socket
            .parent()
            .expect("socket has local-runners parent");
        std::fs::create_dir_all(parent)
            .map_err(|e| GenerationError::Engine(format!("create runner directory: {e}")))?;
        let _ = std::process::Command::new(executable)
            .arg("__local-runner")
            .arg("--socket")
            .arg(&self.socket)
            .arg("--model-path")
            .arg(&self.config.model_path)
            .arg("--context-tokens")
            .arg(self.config.context_tokens.to_string())
            .arg("--tool-protocol")
            .arg(self.config.tool_protocol.as_str())
            .arg("--gpu-layers")
            .arg(self.config.gpu_layers.unwrap_or(0).to_string())
            .spawn()
            .map_err(|e| GenerationError::Engine(format!("start local runner: {e}")))?;
        for _ in 0..100 {
            if let Ok(stream) = tokio::net::UnixStream::connect(&self.socket).await {
                return Ok(stream);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Err(GenerationError::Unavailable(
            "local runner did not become ready within 5 seconds".into(),
        ))
    }
}

#[cfg(feature = "native")]
#[async_trait]
impl TextGenerator for RunnerGenerator {
    async fn generate(
        &self,
        request: GenerationRequest,
    ) -> Result<GenerationStream, GenerationError> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let mut stream = self.connect_or_start().await?;
        let request = gait_local::RunnerRequest::Generate { request };
        let encoded = serde_json::to_string(&request)
            .map_err(|e| GenerationError::Engine(format!("encode runner request: {e}")))?;
        stream
            .write_all(encoded.as_bytes())
            .await
            .map_err(|e| GenerationError::Engine(format!("write runner request: {e}")))?;
        stream
            .write_all(b"\n")
            .await
            .map_err(|e| GenerationError::Engine(format!("finish runner request: {e}")))?;
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<gait_local::RunnerResponse>(&line) {
                    Ok(gait_local::RunnerResponse::Ready { .. }) => {}
                    Ok(gait_local::RunnerResponse::Chunk(chunk)) => {
                        let done = matches!(chunk, Ok(gait_local::GenerationChunk::Done));
                        if tx.send(chunk).await.is_err() || done {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let _ = tx
                            .send(Err(GenerationError::Engine(format!(
                                "decode runner response: {error}"
                            ))))
                            .await;
                        break;
                    }
                }
            }
        });
        Ok(Box::pin(futures_util::stream::unfold(
            rx,
            |mut rx| async move { rx.recv().await.map(|item| (item, rx)) },
        )))
    }
}

/// Hidden role implementation. The process may retain only a model/context and
/// private socket; it receives no session id, worktree, tools, or log paths.
#[cfg(feature = "native")]
pub async fn run_local_runner(argv: &[String]) -> Result<(), GenerationError> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let value = |flag: &str| {
        argv.windows(2)
            .find(|pair| pair[0] == flag)
            .map(|pair| pair[1].clone())
    };
    let socket = PathBuf::from(
        value("--socket")
            .ok_or_else(|| GenerationError::InvalidRequest("runner missing --socket".into()))?,
    );
    let context_tokens = value("--context-tokens")
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| GenerationError::InvalidRequest("runner invalid --context-tokens".into()))?;
    let gpu_layers = value("--gpu-layers").and_then(|v| v.parse().ok());
    let config = LlamaConfig {
        model_path: PathBuf::from(value("--model-path").ok_or_else(|| {
            GenerationError::InvalidRequest("runner missing --model-path".into())
        })?),
        context_tokens,
        gpu_layers,
        tool_protocol: ToolProtocol::parse(
            &value("--tool-protocol").unwrap_or_else(|| "unsupported".into()),
        )?,
    };
    let key = config.runner_key();
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| GenerationError::Engine(format!("create runner directory: {e}")))?;
    }
    if socket.exists() {
        let _ = std::fs::remove_file(&socket);
    }
    let listener = tokio::net::UnixListener::bind(&socket)
        .map_err(|e| GenerationError::Engine(format!("bind runner socket: {e}")))?;
    let generator = LlamaGenerator::new(config)?;
    loop {
        let accepted =
            tokio::time::timeout(std::time::Duration::from_secs(20 * 60), listener.accept()).await;
        let Ok(Ok((stream, _))) = accepted else {
            break;
        };
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let Some(line) = lines
            .next_line()
            .await
            .map_err(|e| GenerationError::Engine(format!("read runner request: {e}")))?
        else {
            continue;
        };
        let request: gait_local::RunnerRequest = serde_json::from_str(&line)
            .map_err(|e| GenerationError::InvalidRequest(format!("decode runner request: {e}")))?;
        match request {
            gait_local::RunnerRequest::Status => {
                write_response(
                    &mut write,
                    gait_local::RunnerResponse::Status {
                        model_key: key.clone(),
                    },
                )
                .await?
            }
            gait_local::RunnerRequest::Stop => {
                write_response(&mut write, gait_local::RunnerResponse::Stopped).await?;
                break;
            }
            gait_local::RunnerRequest::Generate { request } => {
                if write_response(
                    &mut write,
                    gait_local::RunnerResponse::Ready {
                        protocol: gait_local::RUNNER_PROTOCOL,
                    },
                )
                .await
                .is_err()
                {
                    continue;
                }
                let mut chunks = generator.generate(request).await?;
                use futures_util::StreamExt;
                while let Some(chunk) = chunks.next().await {
                    let done = matches!(chunk, Ok(gait_local::GenerationChunk::Done));
                    // A disconnected brain cancels this generation only. It
                    // must not take the warm model process down with it.
                    if write_response(&mut write, gait_local::RunnerResponse::Chunk(chunk))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if done {
                        break;
                    }
                }
            }
        }
    }
    let _ = std::fs::remove_file(socket);
    Ok(())
}

#[cfg(feature = "native")]
async fn write_response(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    response: gait_local::RunnerResponse,
) -> Result<(), GenerationError> {
    use tokio::io::AsyncWriteExt;
    let value = serde_json::to_string(&response)
        .map_err(|e| GenerationError::Engine(format!("encode runner response: {e}")))?;
    write
        .write_all(value.as_bytes())
        .await
        .map_err(|e| GenerationError::Engine(format!("write runner response: {e}")))?;
    write
        .write_all(b"\n")
        .await
        .map_err(|e| GenerationError::Engine(format!("finish runner response: {e}")))
}

impl LlamaGenerator {
    pub fn new(config: LlamaConfig) -> Result<Self, GenerationError> {
        if !config.model_path.is_file() {
            return Err(GenerationError::Unavailable(format!(
                "GGUF model does not exist: {}",
                config.model_path.display()
            )));
        }
        #[cfg(feature = "native")]
        return native::NativeGenerator::new(config).map(|inner| Self { inner });
        #[cfg(not(feature = "native"))]
        Ok(Self { config })
    }
}

#[async_trait]
impl TextGenerator for LlamaGenerator {
    async fn generate(
        &self,
        request: GenerationRequest,
    ) -> Result<GenerationStream, GenerationError> {
        #[cfg(feature = "native")]
        return self.inner.generate(request).await;
        #[cfg(not(feature = "native"))]
        {
            let _ = (&self.config, request);
            Err(GenerationError::Unavailable(
                "this gait build was compiled without the local llama.cpp engine".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gait_local::{GenerationMessage, GenerationTool};

    fn request() -> GenerationRequest {
        GenerationRequest {
            system: "Work carefully.".into(),
            messages: vec![
                GenerationMessage::User {
                    content: "read src/lib.rs".into(),
                },
                GenerationMessage::Assistant {
                    content: "I will inspect it.".into(),
                    tool_calls: vec![gait_local::GenerationToolCall {
                        id: "old-call".into(),
                        name: "read_file".into(),
                        arguments: r#"{"path":"src/lib.rs"}"#.into(),
                    }],
                },
                GenerationMessage::ToolResult {
                    call_id: "old-call".into(),
                    name: "read_file".into(),
                    content: "pub fn example() {}".into(),
                },
            ],
            tools: vec![GenerationTool {
                name: "read_file".into(),
                description: "Read a UTF-8 file.".into(),
                parameters: serde_json::json!({"type":"object","properties":{"path":{"type":"string"}}}),
            }],
            max_tokens: Some(100),
            temperature: Some(0.2),
            top_p: Some(0.9),
        }
    }

    #[test]
    fn qwen_tool_prompt_contains_schema_history_and_response() {
        let rendered = qwen_tool_prompt(&request()).unwrap();
        assert!(rendered.contains("<tools>"));
        assert!(rendered.contains(r#""name":"read_file""#));
        assert!(rendered.contains("<tool_call>{\"name\":\"read_file\",\"arguments\":{\"path\":\"src/lib.rs\"}}</tool_call>"));
        assert!(rendered.contains("<tool_response>\npub fn example() {}\n</tool_response>"));
        assert!(rendered.ends_with("<|im_start|>assistant\n"));
    }

    fn collect(scans: Vec<ScanOut>) -> (String, Vec<String>) {
        let mut text = String::new();
        let mut calls = Vec::new();
        for scan in scans {
            match scan {
                ScanOut::Text(value) => text.push_str(&value),
                ScanOut::Call(value) => calls.push(value),
            }
        }
        (text, calls)
    }

    #[test]
    fn scanner_handles_qwen_calls_across_every_marker_boundary() {
        let full = format!("before {QWEN_TOOL_OPEN}{{\"name\":\"read\",\"arguments\":{{}}}}{QWEN_TOOL_CLOSE} after");
        for split in 1..full.len() {
            if !full.is_char_boundary(split) {
                continue;
            }
            let mut scanner = ToolCallScanner::default();
            let mut scans = scanner.push(&full[..split]);
            scans.extend(scanner.push(&full[split..]));
            scans.extend(scanner.finish());
            let (text, calls) = collect(scans);
            assert_eq!(text, "before  after", "split at {split}");
            assert_eq!(
                calls,
                vec![r#"{"name":"read","arguments":{}}"#],
                "split at {split}"
            );
        }
    }

    #[test]
    fn incomplete_qwen_call_remains_text() {
        let mut scanner = ToolCallScanner::default();
        let mut scans = scanner.push("before <tool_call>{\"name\":\"read\"");
        scans.extend(scanner.finish());
        let (text, calls) = collect(scans);
        assert_eq!(text, "before <tool_call>{\"name\":\"read\"");
        assert!(calls.is_empty());
    }
}
