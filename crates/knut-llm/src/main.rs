//! `knut-llm`: a local language model for the text interface of `knut web` (questions in plain
//! language). It serves Qwen3-1.7B (Apache 2.0) through the OpenAI-compatible chat API that
//! `knut web` speaks, computed with candle (Metal on macOS, else the CPU). The model is
//! downloaded from Hugging Face on the first start and kept in `~/.cache/knut/models`.
//!
//! `knut web` starts `knut-llm serve` on its own when it finds this program and no `--llm`
//! is given.
//!
//! | Request | Answer |
//! | --- | --- |
//! | `GET /health` | `{status: loading\|ready\|failed, message, prompt}`; `prompt: "short"` for a model trained for knut |
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
/// The model trained for knut (knut-additional/training/train.sh), used instead when present.
const TUNED_FILE: &str = "knut-Qwen3-1.7B-Q8_0.gguf";
/// Metadata of a trained GGUF: the instructions it expects (`knut web` reads it from /health).
const PROMPT_KEY: &str = "knut.prompt";
const TOKENIZER_URL: &str = "https://huggingface.co/Qwen/Qwen3-1.7B/resolve/main/tokenizer.json";
const TOKENIZER_FILE: &str = "Qwen3-1.7B-tokenizer.json";

/// Loading state shown by `/health` and in the replies while loading.
#[derive(Clone)]
enum Status {
    Loading(String),
    /// With the `knut.prompt` of the model.
    Ready(Option<String>),
    Failed(String),
}

struct Llm {
    model: ModelWeights,
    /// `knut.prompt` of the GGUF: the instructions the model was trained with ("short").
    prompt: Option<String>,
    tokenizer: tokenizers::Tokenizer,
    device: Device,
    stop: Vec<u32>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let dir = flag("--dir").map(PathBuf::from).unwrap_or_else(cache_dir);
    let model = flag("--model").map(PathBuf::from).unwrap_or_else(|| if dir.join(TUNED_FILE).exists() { dir.join(TUNED_FILE) } else { dir.join(MODEL_FILE) });
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
            let base = flag("--model").map(PathBuf::from).unwrap_or_else(|| dir.join(MODEL_FILE));
            if let Err(e) = fetch(TOKENIZER_URL, &tokenizer, &status).and_then(|_| fetch(MODEL_URL, &base, &status)) {
                eprintln!("knut-llm: {e}");
                std::process::exit(1);
            }
        }
        Some("convert") => {
            let (Some(weights), Some(out)) = (flag("--weights"), flag("--out")) else {
                eprintln!("knut-llm convert --weights DIR --out FILE.gguf [--model BASE.gguf] [--prompt short]");
                std::process::exit(2);
            };
            // the base: the untrained model (also when a trained one is installed)
            let base = flag("--model").map(PathBuf::from).unwrap_or_else(|| dir.join(MODEL_FILE));
            let prompt = flag("--prompt").unwrap_or_else(|| "short".into());
            if let Err(e) = convert(&base, Path::new(&weights), Path::new(&out), &prompt) {
                eprintln!("knut-llm: {e}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!(
                "knut-llm: a local language model for knut web\n\n  knut-llm serve [--port 8780] [--host 127.0.0.1] [--model FILE] [--tokenizer FILE] [--dir DIR]\n  knut-llm download [--dir DIR]\n  knut-llm convert --weights DIR --out FILE.gguf [--model BASE.gguf] [--prompt short]\n      a trained model (safetensors in DIR, e.g. from mlx_lm.fuse) as GGUF, with the\n      metadata and the kinds of quantization of the base model\n\nThe model (Qwen3-1.7B, {MODEL_FILE}, about 1.8 GB) is downloaded on the first start into {}; a trained one ({TUNED_FILE}) there is used instead.",
                cache_dir().display()
            );
            std::process::exit(2);
        }
    }
}

/// The name of a tensor in Hugging Face safetensors for a name in GGUF (as llama.cpp names
/// them for Qwen3; no permutation of the attention weights for this architecture).
fn hf_name(gguf: &str) -> Option<String> {
    match gguf {
        "token_embd.weight" => return Some("model.embed_tokens.weight".into()),
        "output_norm.weight" => return Some("model.norm.weight".into()),
        "output.weight" => return Some("lm_head.weight".into()),
        _ => {}
    }
    let rest = gguf.strip_prefix("blk.")?;
    let (layer, part) = rest.split_once('.')?;
    let hf = match part {
        "attn_norm.weight" => "input_layernorm.weight",
        "ffn_norm.weight" => "post_attention_layernorm.weight",
        "attn_q.weight" => "self_attn.q_proj.weight",
        "attn_k.weight" => "self_attn.k_proj.weight",
        "attn_v.weight" => "self_attn.v_proj.weight",
        "attn_output.weight" => "self_attn.o_proj.weight",
        "attn_q_norm.weight" => "self_attn.q_norm.weight",
        "attn_k_norm.weight" => "self_attn.k_norm.weight",
        "ffn_gate.weight" => "mlp.gate_proj.weight",
        "ffn_up.weight" => "mlp.up_proj.weight",
        "ffn_down.weight" => "mlp.down_proj.weight",
        _ => return None,
    };
    Some(format!("model.layers.{layer}.{hf}"))
}

/// Writes the weights of a trained model as GGUF: the metadata (tokenizer, architecture) and
/// the quantization of each tensor come from the base model. Each new tensor is compared with
/// the base one: training changes them a little, a wrong mapping a lot.
fn convert(base: &Path, weights: &Path, out: &Path, prompt: &str) -> Result<(), String> {
    use candle_core::quantized::QTensor;
    let err = |e: &dyn std::fmt::Display| e.to_string();
    let cpu = Device::Cpu;
    let mut f = std::fs::File::open(base).map_err(|e| format!("{}: {e}", base.display()))?;
    let content = gguf_file::Content::read(&mut f).map_err(|e| err(&e))?;
    let mut hf = std::collections::HashMap::new();
    for entry in std::fs::read_dir(weights).map_err(|e| format!("{}: {e}", weights.display()))? {
        let p = entry.map_err(|e| err(&e))?.path();
        if p.extension().is_some_and(|x| x == "safetensors") {
            hf.extend(candle_core::safetensors::load(&p, &cpu).map_err(|e| format!("{}: {e}", p.display()))?);
        }
    }
    if hf.is_empty() {
        return Err(format!("no .safetensors in {}", weights.display()));
    }
    // the order of the base file
    let mut names: Vec<&String> = content.tensor_infos.keys().collect();
    names.sort_by_key(|n| content.tensor_infos[*n].offset);
    let mut tensors: Vec<(String, QTensor)> = Vec::new();
    let (mut replaced, mut worst) = (0, (0.0f64, String::new()));
    for name in names {
        let old = content.tensor(&mut f, name, &cpu).map_err(|e| err(&e))?;
        // tied embeddings: no lm_head, the output uses the embedding
        let new = hf_name(name).and_then(|h| hf.get(&h).or_else(|| if name == "output.weight" { hf.get("model.embed_tokens.weight") } else { None }));
        let Some(new) = new else {
            tensors.push((name.clone(), old));
            continue;
        };
        let new = new.to_dtype(candle_core::DType::F32).map_err(|e| err(&e))?;
        if new.dims() != old.shape().dims() {
            return Err(format!("{name}: shape {:?} in the weights, {:?} in the base model", new.dims(), old.shape().dims()));
        }
        let before = old.dequantize(&cpu).map_err(|e| err(&e))?;
        let diff = (&new - &before).and_then(|d| d.abs()).and_then(|d| d.sum_all()).and_then(|d| d.to_scalar::<f32>()).map_err(|e| err(&e))? as f64;
        let size = before.abs().and_then(|d| d.sum_all()).and_then(|d| d.to_scalar::<f32>()).map_err(|e| err(&e))? as f64;
        let rel = diff / size.max(1e-12);
        if rel > worst.0 {
            worst = (rel, name.clone());
        }
        let q = QTensor::quantize(&new, old.dtype()).map_err(|e| format!("{name}: {e}"))?;
        tensors.push((name.clone(), q));
        replaced += 1;
    }
    eprintln!("knut-llm: {replaced} of {} tensors replaced; largest change {:.1} % ({})", tensors.len(), 100.0 * worst.0, worst.1);
    if worst.0 > 0.25 {
        return Err("the weights differ too much from the base model: another model, or a wrong mapping of the names".into());
    }
    let mut meta = content.metadata.clone();
    meta.insert(PROMPT_KEY.to_string(), gguf_file::Value::String(prompt.to_string()));
    let mut metadata: Vec<(&String, &gguf_file::Value)> = meta.iter().collect();
    metadata.sort_by_key(|(k, _)| k.as_str());
    let metadata: Vec<(&str, &gguf_file::Value)> = metadata.into_iter().map(|(k, v)| (k.as_str(), v)).collect();
    let refs: Vec<(&str, &QTensor)> = tensors.iter().map(|(n, t)| (n.as_str(), t)).collect();
    let tmp = out.with_extension("part");
    let mut w = std::io::BufWriter::new(std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?);
    gguf_file::write(&mut w, &metadata, &refs).map_err(|e| err(&e))?;
    w.flush().map_err(|e| err(&e))?;
    drop(w);
    std::fs::rename(&tmp, out).map_err(|e| err(&e))?;
    eprintln!("knut-llm: written {}", out.display());
    Ok(())
}

fn cache_dir() -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()).map(PathBuf::from);
    let cache = cache.unwrap_or_else(|| std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join(".cache"));
    cache.join("knut").join("models")
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
                    let prompt = m.prompt.clone();
                    *llm.lock().unwrap() = Some(m);
                    set(&status, Status::Ready(prompt));
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
                    Status::Ready(prompt) => reply(200, json!({ "status": "ready", "prompt": prompt })),
                    Status::Loading(m) => reply(200, json!({ "status": "loading", "message": m })),
                    Status::Failed(m) => reply(200, json!({ "status": "failed", "message": m })),
                },
                (Method::Post, "/v1/chat/completions") => match s {
                    Status::Loading(m) => reply(503, json!({ "error": { "message": format!("the language model is still loading ({m})") } })),
                    Status::Failed(m) => reply(500, json!({ "error": { "message": format!("the language model could not be loaded: {m}") } })),
                    Status::Ready(_) => {
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
    let prompt = content.metadata.get(PROMPT_KEY).and_then(|v| v.to_string().ok()).cloned();
    let model = ModelWeights::from_gguf(content, &mut file, &device).map_err(|e| format!("loading the model: {e}"))?;
    let tokenizer = tokenizers::Tokenizer::from_file(tokenizer_path).map_err(|e| format!("{}: {e}", tokenizer_path.display()))?;
    let stop: Vec<u32> = ["<|im_end|>", "<|endoftext|>"].iter().filter_map(|t| tokenizer.token_to_id(t)).collect();
    Ok(Llm { model, prompt, tokenizer, device, stop })
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
