//! Minimal ureq 3 helpers (form/json).

use std::time::Duration;

use serde_json::Value;

pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn agent(timeout_secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(timeout_secs)))
        .build()
        .new_agent()
}

pub fn get_json(url: &str, headers: &[(&str, &str)], timeout_secs: u64) -> Result<Value, String> {
    let mut req = agent(timeout_secs).get(url);
    for &(k, v) in headers {
        req = req.header(k, v);
    }
    let resp = req.call().map_err(describe_err)?;
    read_json(resp)
}

pub fn post_form_json(
    url: &str,
    fields: &[(&str, &str)],
    headers: &[(&str, &str)],
    timeout_secs: u64,
) -> Result<Value, String> {
    let _ = urlencode; // reserved; send_form handles encoding
    let mut req = agent(timeout_secs).post(url);
    for &(k, v) in headers {
        req = req.header(k, v);
    }
    let resp = req
        .send_form(fields.iter().copied())
        .map_err(describe_err)?;
    read_json(resp)
}

pub fn post_json(
    url: &str,
    body: &serde_json::Value,
    headers: &[(&str, &str)],
    timeout_secs: u64,
) -> Result<Value, String> {
    let mut req = agent(timeout_secs).post(url);
    for &(k, v) in headers {
        req = req.header(k, v);
    }
    let resp = req
        .header("content-type", "application/json")
        .send_json(body)
        .map_err(describe_err)?;
    read_json(resp)
}

fn describe_err(e: ureq::Error) -> String {
    match e {
        ureq::Error::StatusCode(code) => format!("HTTP {code}"),
        other => other.to_string(),
    }
}

fn read_json(resp: ureq::http::Response<ureq::Body>) -> Result<Value, String> {
    let mut body = resp.into_body();
    let text = body.read_to_string().map_err(|e| format!("read: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("bad json: {e}"))
}