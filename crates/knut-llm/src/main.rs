//! `knut-llm`: a local language model for the simple page of `knut web` (questions in plain
//! language). It serves Qwen3-1.7B (Apache 2.0) through the OpenAI-compatible chat API that
//! `knut web` speaks, computed with candle (Metal on macOS, else the CPU). The model is
//! downloaded from Hugging Face on the first start and kept in `~/.cache/knut/models`.
//!
//! `knut web` starts `knut-llm serve` on its own when it finds this program and no `--llm`
//! is given.
//!
//! | Request | Answer |
//! | --- | --- |
//! | `GET /health` | `{status: loading\|ready\|failed, message}` |
//! | `POST /v1/chat/completions` `{messages, max_tokens?, temperature?}` | `{choices: [{message}]}`; 503 while loading |

use candle_core::quantized::gguf_file;
use candle_core::{Device, Tensor};
use candle_transformers::generation::{LogitsProcessor, Sampling};
use candle_transformers::models::quantized_qwen3::ModelWeights;
use serde_json::{Value as J, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Method, Response, Server};

const MODEL_URL: &str = "https://huggingface.co/Qwen/Qwen3-1.7B-GGUF/resolve/main/Qwen3-1.7B-Q8_0.gguf";
const MODEL_FILE: &str = "Qwen3-1.7B-Q8_0.gguf";
const TOKENIZER_URL: &str = "https://huggingface.co/Qwen/Qwen3-1.7B/resolve/main/tokenizer.json";
const TOKENIZER_FILE: &str = "Qwen3-1.7B-tokenizer.json";

/// Loading state shown by `/health` and in the replies while loading.
#[derive(Clone)]
enum Status {
    Loading(String),
    Ready,
    Failed(String),
}

struct Llm {
    model: ModelWeights,
    tokenizer: tokenizers::Tokenizer,
    device: Device,
    stop: Vec<u32>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let dir = flag("--dir").map(PathBuf::from).unwrap_or_else(cache_dir);
    let model = flag("--model").map(PathBuf::from).unwrap_or_else(|| dir.join(MODEL_FILE));
    let tokenizer = flag("--tokenizer").map(PathBuf::from).unwrap_or_else(|| dir.join(TOKENIZER_FILE));
    match args.first().map(String::as_str) {
        Some("serve") => {
            let port: u16 = flag("--port").and_then(|p| p.parse().ok()).unwrap_or(8780);
            let host = flag("--host").unwrap_or_else(|| "127.0.0.1".into());
            if let Err(e) = serve(&host, port, model, tokenizer) {
                eprintln!("knut-llm: {e}");
                std::process::exit(1);
            }
        }
        Some("download") => {
            let status = Arc::new(Mutex::new(Status::Loading(String::new())));
            if let Err(e) = fetch(TOKENIZER_URL, &tokenizer, &status).and_then(|_| fetch(MODEL_URL, &model, &status)) {
                eprintln!("knut-llm: {e}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!(
                "knut-llm: a local language model for knut web\n\n  knut-llm serve [--port 8780] [--host 127.0.0.1] [--model FILE] [--tokenizer FILE] [--dir DIR]\n  knut-llm download [--dir DIR]\n\nThe model (Qwen3-1.7B, {MODEL_FILE}, about 1.8 GB) is downloaded on the first start into {}.",
                cache_dir().display()
            );
            std::process::exit(2);
        }
    }
}

fn cache_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".cache").join("knut").join("models")
}

/// Starts the server at once; the model is downloaded (if needed) and loaded in the
/// background, requests meanwhile get 503 with the progress.
fn serve(host: &str, port: u16, model_path: PathBuf, tokenizer_path: PathBuf) -> Result<(), String> {
    let server = Server::http((host, port)).map_err(|e| format!("cannot listen on {host}:{port}: {e}"))?;
    eprintln!("knut-llm: http://{host}:{port}/");
    let status = Arc::new(Mutex::new(Status::Loading("starting".into())));
    let llm: Arc<Mutex<Option<Llm>>> = Arc::new(Mutex::new(None));
    {
        let (status, llm) = (status.clone(), llm.clone());
        std::thread::spawn(move || {
            let r = fetch(TOKENIZER_URL, &tokenizer_path, &status).and_then(|_| fetch(MODEL_URL, &model_path, &status)).and_then(|_| {
                set(&status, Status::Loading("loading the model".into()));
                load(&model_path, &tokenizer_path)
            });
            match r {
                Ok(m) => {
                    eprintln!("knut-llm: ready ({})", if m.device.is_metal() { "Metal" } else { "CPU" });
                    *llm.lock().unwrap() = Some(m);
                    set(&status, Status::Ready);
                }
                Err(e) => {
                    eprintln!("knut-llm: {e}");
                    set(&status, Status::Failed(e));
                }
            }
        });
    }
    for mut req in server.incoming_requests() {
        let (status, llm) = (status.clone(), llm.clone());
        std::thread::spawn(move || {
            let reply = |code: u16, v: J| Response::from_string(v.to_string()).with_status_code(code).with_header(Header::from_bytes("Content-Type", "application/json").unwrap());
            let url = req.url().split('?').next().unwrap_or("/").to_string();
            let s = status.lock().unwrap().clone();
            let r = match (req.method(), url.as_str()) {
                (Method::Get, "/health") => match s {
                    Status::Ready => reply(200, json!({ "status": "ready" })),
                    Status::Loading(m) => reply(200, json!({ "status": "loading", "message": m })),
                    Status::Failed(m) => reply(200, json!({ "status": "failed", "message": m })),
                },
                (Method::Post, "/v1/chat/completions") => match s {
                    Status::Loading(m) => reply(503, json!({ "error": { "message": format!("the language model is still loading ({m})") } })),
                    Status::Failed(m) => reply(500, json!({ "error": { "message": format!("the language model could not be loaded: {m}") } })),
                    Status::Ready => {
                        let mut body = String::new();
                        let _ = req.as_reader().take(4 << 20).read_to_string(&mut body);
                        let v: J = serde_json::from_str(&body).unwrap_or(J::Null);
                        let mut guard = llm.lock().unwrap();
                        match guard.as_mut().map(|m| m.chat(&v)) {
                            Some(Ok(text)) => reply(200, json!({ "choices": [{ "index": 0, "message": { "role": "assistant", "content": text }, "finish_reason": "stop" }] })),
                            Some(Err(e)) => reply(500, json!({ "error": { "message": e } })),
                            None => reply(503, json!({ "error": { "message": "the language model is not loaded" } })),
                        }
                    }
                },
                _ => reply(404, json!({ "error": { "message": "not found" } })),
            };
            let _ = req.respond(r);
        });
    }
    Ok(())
}

fn set(status: &Mutex<Status>, s: Status) {
    *status.lock().unwrap() = s;
}

/// Downloads `url` to `path` unless it is there (through a `.part` file).
fn fetch(url: &str, path: &Path, status: &Mutex<Status>) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    eprintln!("knut-llm: downloading {name} from {url}");
    let mut resp = ureq::get(url).call().map_err(|e| format!("download of {name} failed: {e}"))?;
    let total: Option<u64> = resp.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
    let part = path.with_extension("part");
    let mut out = std::fs::File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?;
    let mut reader = resp.body_mut().with_config().limit(u64::MAX).reader();
    let mut buf = vec![0u8; 1 << 20];
    let (mut done, mut shown) = (0u64, 0u64);
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("download of {name} failed: {e}"))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| format!("{}: {e}", part.display()))?;
        done += n as u64;
        let text = match total {
            Some(t) if t > 0 => format!("downloading {name}: {:.0} %", done as f64 * 100.0 / t as f64),
            _ => format!("downloading {name}: {:.0} MB", done as f64 / 1e6),
        };
        set(status, Status::Loading(text.clone()));
        if done - shown >= 50 << 20 {
            eprintln!("knut-llm: {text}");
            shown = done;
        }
    }
    out.flush().map_err(|e| e.to_string())?;
    drop(out);
    std::fs::rename(&part, path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}

fn load(model_path: &Path, tokenizer_path: &Path) -> Result<Llm, String> {
    let device = if cfg!(target_os = "macos") { Device::new_metal(0).unwrap_or(Device::Cpu) } else { Device::Cpu };
    let mut file = std::fs::File::open(model_path).map_err(|e| format!("{}: {e}", model_path.display()))?;
    let content = gguf_file::Content::read(&mut file).map_err(|e| format!("{}: {e}", model_path.display()))?;
    let model = ModelWeights::from_gguf(content, &mut file, &device).map_err(|e| format!("loading the model: {e}"))?;
    let tokenizer = tokenizers::Tokenizer::from_file(tokenizer_path).map_err(|e| format!("{}: {e}", tokenizer_path.display()))?;
    let stop: Vec<u32> = ["<|im_end|>", "<|endoftext|>"].iter().filter_map(|t| tokenizer.token_to_id(t)).collect();
    Ok(Llm { model, tokenizer, device, stop })
}

impl Llm {
    /// A reply to OpenAI-style chat messages (Qwen3 chat format, without thinking).
    fn chat(&mut self, req: &J) -> Result<String, String> {
        let mut prompt = String::new();
        for m in req["messages"].as_array().into_iter().flatten() {
            let role = m["role"].as_str().unwrap_or("user");
            let content = m["content"].as_str().unwrap_or("");
            prompt.push_str(&format!("<|im_start|>{role}\n{content}<|im_end|>\n"));
        }
        // an empty thinking block: the model answers directly
        prompt.push_str("<|im_start|>assistant\n<think>\n\n</think>\n\n");
        let max_tokens = req["max_tokens"].as_u64().unwrap_or(512).min(4096) as usize;
        let temperature = req["temperature"].as_f64().unwrap_or(0.0);
        let tokens = self.tokenizer.encode(prompt, false).map_err(|e| e.to_string())?.get_ids().to_vec();
        let mut sampler =
            LogitsProcessor::from_sampling(42, if temperature <= 0.0 { Sampling::ArgMax } else { Sampling::TopP { p: 0.9, temperature } });
        let err = |e: candle_core::Error| e.to_string();
        self.model.clear_kv_cache();
        let input = Tensor::new(tokens.as_slice(), &self.device).map_err(err)?.unsqueeze(0).map_err(err)?;
        let mut logits = self.model.forward(&input, 0).map_err(err)?;
        let mut out: Vec<u32> = Vec::new();
        for i in 0..max_tokens {
            // a penalty on the last tokens keeps small models from repeating themselves
            let logits_1 = logits.squeeze(0).map_err(err)?;
            let recent = &out[out.len().saturating_sub(64)..];
            let logits_1 = if recent.is_empty() { logits_1 } else { candle_transformers::utils::apply_repeat_penalty(&logits_1, 1.1, recent).map_err(err)? };
            let next = sampler.sample(&logits_1).map_err(err)?;
            if self.stop.contains(&next) {
                break;
            }
            out.push(next);
            let input = Tensor::new(&[next], &self.device).map_err(err)?.unsqueeze(0).map_err(err)?;
            logits = self.model.forward(&input, tokens.len() + i).map_err(err)?;
        }
        self.tokenizer.decode(&out, true).map_err(|e| e.to_string())
    }
}
