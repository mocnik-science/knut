//! Questions in plain language (the simple page): a language model turns a question into a
//! knut query or asks back when something essential is missing; the query is checked, and
//! the model corrects it from the messages (a few times at most); then it runs, and the model
//! answers the question from the results.
//!
//! The model is reached through [`Model`]: a server with an OpenAI-compatible API
//! (`knut web --llm URL`: llama.cpp, Ollama, vLLM, …), or `knut-llm`, the local model
//! that `knut web` starts on its own when it finds the program.

use serde_json::{Value as J, json};

/// Corrections the model may make after the check of its query.
const MAX_ATTEMPTS: usize = 3;
/// Characters of the results the model reads per output, and in all.
const RESULT_CHARS: usize = 2_000;
const RESULTS_CHARS: usize = 6_000;

/// A chat model: messages (role, content) in, the reply out.
pub trait Model: Send + Sync {
    fn complete(&self, messages: &[(String, String)], max_tokens: usize) -> Result<String, String>;
}

/// A server with an OpenAI-compatible chat API (`…/v1/chat/completions`).
pub struct HttpModel {
    pub url: String,
    pub model: String,
}

impl HttpModel {
    fn endpoint(&self) -> String {
        let base = self.url.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else if base.ends_with("/v1") {
            format!("{base}/chat/completions")
        } else {
            format!("{base}/v1/chat/completions")
        }
    }
}

impl Model for HttpModel {
    fn complete(&self, messages: &[(String, String)], max_tokens: usize) -> Result<String, String> {
        let endpoint = self.endpoint();
        let body = json!({
            "model": self.model,
            "messages": messages.iter().map(|(r, c)| json!({ "role": r, "content": c })).collect::<Vec<_>>(),
            "temperature": 0.2,
            "max_tokens": max_tokens,
            "stream": false,
        });
        let fail = |e: &dyn std::fmt::Display| format!("the language model at {endpoint} did not answer: {e}");
        // replies with an error status are read too: they say what is wrong (still loading, …)
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(std::time::Duration::from_secs(600))).build().into();
        let mut resp = agent.post(&endpoint).header("Content-Type", "application/json").send(body.to_string()).map_err(|e| fail(&e))?;
        let ok = resp.status().is_success();
        let text = resp.body_mut().read_to_string().map_err(|e| fail(&e))?;
        let v: J = serde_json::from_str(&text).map_err(|e| fail(&e))?;
        if !ok {
            return Err(v["error"]["message"].as_str().or(v["error"].as_str()).map(str::to_string).unwrap_or_else(|| fail(&text)));
        }
        let content = v["choices"][0]["message"]["content"].as_str().ok_or_else(|| fail(&"no message in the reply"))?;
        Ok(strip_thinking(content))
    }
}

/// The reply without a `<think>…</think>` part (models that reason before answering).
fn strip_thinking(s: &str) -> String {
    match (s.find("<think>"), s.find("</think>")) {
        (Some(a), Some(b)) if b > a => format!("{}{}", &s[..a], &s[b + "</think>".len()..]).trim().to_string(),
        (None, Some(b)) => s[b + "</think>".len()..].trim().to_string(),
        _ => s.trim().to_string(),
    }
}

/// What the model proposes: a query, or a question back.
#[derive(Debug, PartialEq)]
pub enum Reply {
    Query(String),
    Ask(String),
}

/// Reads `QUERY:` (followed by the script) or `ASK:` (followed by a question); a reply
/// without either is taken as a query. Code fences are removed.
pub fn parse_reply(s: &str) -> Reply {
    let t = s.trim();
    let upper = t.to_uppercase();
    if let Some(i) = upper.find("ASK:")
        && upper.find("QUERY:").is_none_or(|q| i < q)
    {
        // the question is one line (small models tend to repeat themselves)
        return Reply::Ask(t[i + 4..].trim().lines().next().unwrap_or("").trim().to_string());
    }
    let code = match upper.find("QUERY:") {
        Some(i) => &t[i + 6..],
        None => t,
    };
    // a second QUERY: or ASK: ends the query
    let up = code.to_uppercase();
    let end = [up.find("QUERY:"), up.find("ASK:")].into_iter().flatten().min().unwrap_or(code.len());
    Reply::Query(strip_fences(&code[..end]))
}

fn strip_fences(s: &str) -> String {
    let lines: Vec<&str> = s.trim().lines().filter(|l| !l.trim_start().starts_with("```")).collect();
    lines.join("\n").trim().to_string()
}

/// The outcome of turning a question into a query.
#[derive(Debug, Default)]
pub struct Plan {
    /// A question back to the user (then no query).
    pub ask: Option<String>,
    pub query: Option<String>,
    /// Messages of the check that remained after the last correction (empty: the query is
    /// valid).
    pub problems: Vec<String>,
    /// Replies of the model (1 + corrections).
    pub attempts: usize,
}

/// Asks the model for a query (or a question back) and lets it correct the query from the
/// messages of `check` (errors only), a few times at most. `history` alternates user and
/// assistant turns and ends with the user's question.
pub fn plan(model: &dyn Model, system: &str, history: &[(String, String)], check: &dyn Fn(&str) -> Vec<String>, step: &dyn Fn(&str)) -> Result<Plan, String> {
    let mut messages: Vec<(String, String)> = vec![("system".into(), system.to_string())];
    messages.extend(history.iter().cloned());
    let mut out = Plan::default();
    for attempt in 0..MAX_ATTEMPTS {
        step(if attempt == 0 { "understanding the question" } else { "correcting the query" });
        let reply = model.complete(&messages, 800)?;
        out.attempts += 1;
        match parse_reply(&reply) {
            Reply::Ask(q) => {
                out.ask = Some(q);
                out.query = None;
                out.problems.clear();
                return Ok(out);
            }
            Reply::Query(code) => {
                step("checking the query");
                let problems = check(&code);
                out.query = Some(code);
                if problems.is_empty() {
                    out.problems.clear();
                    return Ok(out);
                }
                messages.push(("assistant".into(), reply));
                messages.push((
                    "user".into(),
                    format!("Checking this query gives these messages:\n\n{}\n\nCorrect the query. Reply with QUERY: or ASK: again.", problems.join("\n\n")),
                ));
                out.problems = problems;
            }
        }
    }
    Ok(out)
}

/// The results of a run as the model reads them: per output its expression and its text,
/// shortened.
pub fn results_text(outputs: &[(String, String)]) -> String {
    let mut s = String::new();
    for (expr, text) in outputs {
        if s.len() >= RESULTS_CHARS {
            s.push_str("\n(more results left out)\n");
            break;
        }
        let mut t: String = text.chars().take(RESULT_CHARS).collect();
        if t.len() < text.len() {
            t.push_str("\n(shortened)");
        }
        s.push_str(&format!("── {expr}\n{t}\n\n"));
    }
    if s.is_empty() { "(no results)".into() } else { s }
}

/// The answer to the question from the query and its results.
pub fn answer(model: &dyn Model, question: &str, query: &str, results: &str) -> Result<String, String> {
    let system = "You answer questions about OpenStreetMap data. You get the question, the knut query that was run for it, \
        and its results. Answer briefly, in the language of the question, using only the results. \
        Give numbers as they are in the results. If the results are empty or shortened, say so. Do not show the query.";
    let user = format!("Question: {question}\n\nQuery:\n{query}\n\nResults:\n{results}");
    model.complete(&[("system".into(), system.into()), ("user".into(), user)], 600)
}

/// A compact reference of the language for the model.
pub const CHEAT_SHEET: &str = "\
Selections: nodes n with COND | ways w with COND | areas a with COND | relations r with COND | * x with COND (all kinds).
  count of SELECTION | pick SELECTION (one element) | pick 5 SELECTION ordered by -x.area (largest first).
Conditions: x:key=value (no spaces), x:key (tag present), x:key == none (tag missing), x:key in [a, b],
  and, or, not, brackets. Numbers from tags: (x:capacity ?? 0) > 20. Units: x:height > 30 m, x:maxspeed > 50 km/h.
Fields: x.id x.version x.time x.user x.length (metres) x.area (m2) x.lon x.lat; text: x:name.length.
Space: ... within PLACE (a boundary in the database, e.g. within Vegesack) | within 8.78...8.84, 53.06...53.09
  (longitude range, latitude range) | x near some _ in S closer than 50 m | x near no _ in S closer than 50 m |
  nearest of S to x.
Routing: path P from a to b by foot|bicycle|car (p.length, p.duration) | isochrone I from a closer than 10 min by foot.
  a and b are elements, e.g. s = pick node n with n:railway=station and n:name='Bremen Hauptbahnhof'.
Time: without a time, the state at the end of the data. ... on 2015-01-01 (state then) |
  ... on each year from 2010 to 2024 (one value per year) | ... from 2023 to 2024 (edits in that period) per year.
Grouping: count of SEL per x:key | per year | per (areas d with d:boundary=administrative and d:admin_level='9') |
  having count > 20. Aggregates: count sum average distribution share of SEL, or of (SEL).map(x => EXPR).
Variables: name = EXPR on its own line; later lines use the name. Several outputs: separate lines.
Tags: OpenStreetMap tags in English, e.g. amenity=bench|cafe|restaurant|school|library|bicycle_parking|waste_basket,
  shop=bakery|supermarket|butcher, leisure=park|playground, highway=residential|cycleway|bus_stop, railway=station,
  tourism=museum|hotel, building, natural=water. Translate words of other languages to these tags
  (Bäckerei: shop=bakery, Spielplatz: leisure=playground, Bank zum Sitzen: amenity=bench).";

/// Examples always shown to the model (titles in examples.json).
const CORE_EXAMPLES: &[&str] = &["Count benches", "Within a place", "Walking route", "Ten minutes on foot", "Count per value", "State every year"];

/// The instructions for turning a question into a query: the task, the database, the
/// compact reference and some examples (always the core ones, plus those sharing most words
/// with the question; small models do better with few).
pub fn system_prompt(database: &str, question: &str, examples: &[crate::Example]) -> String {
    let mut s = String::from(
        "You turn questions about OpenStreetMap data into queries in the knut query language.\n\
         Reply in exactly one of two forms and nothing else:\n\
         QUERY:\n<the knut query>\n\
         or\n\
         ASK: <one short question>\n\
         Ask back only when something essential is missing, for example the way of moving for a route. \
         Otherwise write the query; prefer plain, short queries.\n\n",
    );
    s.push_str(&format!("Database: {database}\n\nThe language:\n{CHEAT_SHEET}\n\nExamples:\n"));
    let words = |t: &str| -> Vec<String> { t.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 3).map(String::from).collect() };
    let q = words(question);
    let mut scored: Vec<(usize, &crate::Example)> = examples
        .iter()
        .filter(|e| !CORE_EXAMPLES.contains(&e.title.as_str()))
        .map(|e| (words(&format!("{} {} {}", e.title, e.description, e.code)).iter().filter(|w| q.contains(w)).count(), e))
        .filter(|x| x.0 > 0)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    let chosen = examples.iter().filter(|e| CORE_EXAMPLES.contains(&e.title.as_str())).chain(scored.into_iter().take(4).map(|x| x.1));
    for e in chosen {
        s.push_str(&format!("\n# {}\n{}\n", e.description, e.code));
    }
    s
}

/// The text of the documentation page, without markup (the reference for the model).
pub fn reference_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut tag = String::new();
    for c in html.chars() {
        match c {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                let closing = tag.starts_with('/');
                let name = tag.trim_start_matches('/').split_whitespace().next().unwrap_or("").to_lowercase();
                match name.as_str() {
                    "tr" | "p" | "h2" | "h3" | "li" | "pre" | "br" | "table" => out.push('\n'),
                    "td" | "th" if !closing => out.push_str(" | "),
                    _ => {}
                }
            }
            _ if in_tag => tag.push(c),
            _ => out.push(c),
        }
    }
    let out = out.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&").replace("&nbsp;", " ").replace("&quot;", "\"");
    let mut lines: Vec<String> = Vec::new();
    for l in out.lines() {
        let l = l.trim().trim_start_matches('|').trim().to_string();
        if !(l.is_empty() && lines.last().is_some_and(|p| p.is_empty())) {
            lines.push(l);
        }
    }
    lines.join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Replies in turn with the given texts.
    struct Scripted(Mutex<Vec<String>>);

    impl Model for Scripted {
        fn complete(&self, _: &[(String, String)], _: usize) -> Result<String, String> {
            let mut v = self.0.lock().unwrap();
            if v.is_empty() { Err("no more replies".into()) } else { Ok(v.remove(0)) }
        }
    }

    #[test]
    fn replies_are_read() {
        assert_eq!(parse_reply("QUERY:\n```knut\ncount of nodes n with n:amenity=bench\n```"), Reply::Query("count of nodes n with n:amenity=bench".into()));
        assert_eq!(parse_reply("ASK: By foot, bicycle or car?"), Reply::Ask("By foot, bicycle or car?".into()));
        assert_eq!(parse_reply("count of nodes n with n:shop"), Reply::Query("count of nodes n with n:shop".into()));
        assert_eq!(strip_thinking("<think>hm</think>\nQUERY: x"), "QUERY: x");
    }

    #[test]
    fn a_query_is_corrected_from_the_messages() {
        let model = Scripted(Mutex::new(vec!["QUERY:\ncount of nodes n with n:amenity=bench and".into(), "QUERY:\ncount of nodes n with n:amenity=bench".into()]));
        let check = |q: &str| if q.ends_with("and") { vec!["Error: the condition ends after `and`".to_string()] } else { Vec::new() };
        let p = plan(&model, "system", &[("user".into(), "How many benches?".into())], &check, &|_| {}).unwrap();
        assert_eq!(p.query.as_deref(), Some("count of nodes n with n:amenity=bench"));
        assert!(p.problems.is_empty());
        assert_eq!(p.attempts, 2);
    }

    #[test]
    fn the_model_may_ask_back() {
        let model = Scripted(Mutex::new(vec!["ASK: From where?".into()]));
        let p = plan(&model, "system", &[("user".into(), "How long to the station?".into())], &|_| Vec::new(), &|_| {}).unwrap();
        assert_eq!(p.ask.as_deref(), Some("From where?"));
        assert!(p.query.is_none());
    }

    #[test]
    fn corrections_are_limited() {
        let model = Scripted(Mutex::new(vec!["QUERY: a".into(), "QUERY: b".into(), "QUERY: c".into(), "QUERY: d".into()]));
        let p = plan(&model, "system", &[("user".into(), "?".into())], &|_| vec!["Error".to_string()], &|_| {}).unwrap();
        assert_eq!(p.attempts, MAX_ATTEMPTS);
        assert!(!p.problems.is_empty());
    }

    #[test]
    fn the_reference_is_plain_text() {
        let t = reference_text("<h2>Fields</h2><table><tr><td><code>.id</code></td><td>identity &amp; more</td></tr></table>");
        assert!(t.contains("Fields"));
        assert!(t.contains(".id | identity & more"));
        assert!(!t.contains('<'));
    }
}
