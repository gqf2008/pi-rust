#![forbid(unsafe_code)]

use rpi_plugin_sdk::handles::ToolHandles;
use rpi_plugin_sdk::{
    FreeStringFn, StableToolSchema, StbString, StbStringRef, StepHandle, StepResult, ToolPartialCb,
};
use serde_json::{json, Value};
use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::time::Duration;
use url::Url;
static HANDLES: ToolHandles<Drive> = ToolHandles::new();

struct Drive {
    params: Value,
    done: bool,
}
fn search(p: &Value) -> Result<String, String> {
    let query = p
        .get("query")
        .and_then(Value::as_str)
        .ok_or("query is required")?
        .trim();
    if query.is_empty() {
        return Err("query must not be empty".into());
    }
    let limit = p
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .clamp(1, 20) as usize;
    let url = Url::parse_with_params(
        "https://api.duckduckgo.com/",
        &[
            ("q", query),
            ("format", "json"),
            ("no_html", "1"),
            ("skip_disambig", "1"),
        ],
    )
    .map_err(|e| e.to_string())?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("rpi-websearch/0.1")
        .build()
        .map_err(|e| e.to_string())?;
    let value: Value = client
        .get(url)
        .send()
        .map_err(|e| format!("web search failed: {e}"))?
        .json()
        .map_err(|e| format!("invalid search response: {e}"))?;
    let mut hits = Vec::new();
    let mut failed = Vec::new();
    if let Some(text) = value
        .get("AbstractText")
        .and_then(Value::as_str)
        .filter(|x| !x.is_empty())
    {
        hits.push(json!({"title":value.get("Heading").and_then(Value::as_str).unwrap_or(query),"url":value.get("AbstractURL").and_then(Value::as_str).unwrap_or(""),"snippet":text,"source":"duckduckgo"}));
    }
    fn collect(v: &Value, hits: &mut Vec<Value>, limit: usize) {
        if hits.len() >= limit {
            return;
        }
        if let Some(arr) = v.as_array() {
            for x in arr {
                if hits.len() >= limit {
                    return;
                }
                if let Some(t) = x.get("Text").and_then(Value::as_str) {
                    hits.push(json!({"title":t,"url":x.get("FirstURL").and_then(Value::as_str).unwrap_or(""),"snippet":t,"source":"duckduckgo"}));
                }
                collect(x.get("Topics").unwrap_or(&Value::Null), hits, limit);
            }
        }
    }
    collect(
        value.get("RelatedTopics").unwrap_or(&Value::Null),
        &mut hits,
        limit,
    );
    // DDG's Instant Answer endpoint is intentionally narrow. Wikipedia gives
    // useful coverage for ordinary research queries when DDG returns no hits.
    if hits.len() < limit {
        let wiki = Url::parse_with_params(
            "https://en.wikipedia.org/w/api.php",
            &[
                ("action", "query"),
                ("list", "search"),
                ("srsearch", query),
                ("srlimit", "8"),
                ("format", "json"),
                ("origin", "*"),
            ],
        );
        let wiki_response = wiki.ok().and_then(|u| client.get(u).send().ok());
        match wiki_response {
            Some(response) if response.status().is_success() => {
                if let Ok(wiki_value) = response.json::<Value>() {
                    if let Some(items) = wiki_value
                        .get("query")
                        .and_then(|q| q.get("search"))
                        .and_then(Value::as_array)
                    {
                        for item in items {
                            if hits.len() >= limit {
                                break;
                            }
                            let title = item.get("title").and_then(Value::as_str).unwrap_or(query);
                            let pageid = item.get("pageid").and_then(Value::as_u64).unwrap_or(0);
                            let snippet = item
                                .get("snippet")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .replace("<span class=\"searchmatch\">", "")
                                .replace("</span>", "");
                            hits.push(json!({"title":title,"url":format!("https://en.wikipedia.org/?curid={pageid}"),"snippet":snippet,"source":"wikipedia"}));
                        }
                    }
                }
            }
            _ => failed.push("wikipedia"),
        }
    }
    Ok(json!({"query":query,"count":hits.len(),"results":hits,"failed":failed}).to_string())
}
extern "C" fn execute(
    _: StbStringRef,
    params: StbString,
    free: Option<FreeStringFn>,
) -> StepHandle {
    let t = params.to_string_lossy();
    params.free_with(free);
    HANDLES.insert(Drive {
        params: serde_json::from_str(&t).unwrap_or(Value::Null),
        done: false,
    })
}
extern "C" fn poll(h: StepHandle, _: Option<ToolPartialCb>, _: *mut c_void) -> StepResult {
    if h.is_null() {
        return StepResult::err(StbString::from_string("null websearch handle".into()));
    }
    let Some(entry) = HANDLES.get(h) else {
        return StepResult::err(StbString::from_string(
            "invalid or destroyed tool handle".into(),
        ));
    };
    let mut d = entry.lock();
    if entry.cancelled.load(Ordering::SeqCst) {
        return StepResult::err(StbString::from_string("websearch cancelled".into()));
    }
    if d.done {
        return StepResult::err(StbString::from_string(
            "websearch polled after completion".into(),
        ));
    }
    d.done = true;
    match search(&d.params) {
        Ok(t) => {
            let value: Value = serde_json::from_str(&t).unwrap_or_else(|_| json!({"results":[]}));
            let query = value.get("query").and_then(Value::as_str).unwrap_or("");
            let results = value
                .get("results")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut text = format!(
                "Found {} web reference(s) for \"{}\":\n",
                results.len(),
                query
            );
            for hit in &results {
                text.push_str(&format!(
                    "\n- {}\n  {}\n  {}\n",
                    hit.get("title").and_then(Value::as_str).unwrap_or(""),
                    hit.get("url").and_then(Value::as_str).unwrap_or(""),
                    hit.get("snippet").and_then(Value::as_str).unwrap_or("")
                ));
            }
            StepResult::done(StbString::from_string(
                json!({"content":[{"type":"text","text":text}],"details":value}).to_string(),
            ))
        }
        Err(e) => StepResult::err(StbString::from_string(e)),
    }
}
extern "C" fn cancel(h: StepHandle) {
    HANDLES.cancel(h);
}
extern "C" fn destroy(h: StepHandle) {
    drop(HANDLES.remove(h));
}
use rpi_plugin_sdk::callbacks::free_string;
rpi_plugin_sdk::export_plugin!(|api| {
    let Some(register) = api.register_tool else {
        return 1;
    };
    let schema=Box::new(StableToolSchema{name:StbString::from_string("websearch".into()),description:StbString::from_string("Search public web references with DuckDuckGo.".into()),parameters:StbString::from_string(r#"{"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":20}},"required":["query"]}"#.into())});
    let rc = register(&*schema, execute, poll, cancel, destroy, free_string);
    drop(schema);
    rc
});
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_empty() {
        assert!(search(&json!({"query":""})).is_err());
    }
}
