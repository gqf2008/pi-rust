//! Shared reader/writer for the session-scoped schedule files.
//!
//! `.ki/cron.json` and `.ki/watch.json` both accept either a bare JSON array or
//! an object with a single collection key (`jobs` / `watches`). The agent-facing
//! tools add and remove single entries; they must not re-encode the file through
//! the typed parser, because that would drop unknown top-level keys and turn a
//! one-entry edit into a whole-file rewrite. So this keeps the decoded
//! [`serde_json::Value`] and edits only the entry it was asked to edit.
//!
//! The scheduler threads reload on mtime, so a successful save is visible to a
//! running session without any further wiring.

use std::fs;
use std::io;
use std::path::Path;

use serde_json::Value;

/// The shape the file had when it was read; [`save`] preserves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    Array,
    Object,
}

/// One decoded schedule file. Entries are kept as raw values so untouched
/// entries round-trip unchanged.
#[derive(Debug)]
pub(crate) struct ScheduleFile {
    root: Value,
    shape: Shape,
    key: &'static str,
}

impl ScheduleFile {
    /// The collection, empty when the file did not exist yet.
    pub(crate) fn entries(&self) -> &[Value] {
        match (&self.root, self.shape) {
            (Value::Array(entries), Shape::Array) => entries,
            (Value::Object(map), Shape::Object) => map
                .get(self.key)
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            _ => &[],
        }
    }

    fn entries_mut(&mut self) -> &mut Vec<Value> {
        let key = self.key;
        match (&mut self.root, self.shape) {
            (Value::Array(entries), Shape::Array) => entries,
            (Value::Object(map), Shape::Object) => map
                .entry(key.to_string())
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .expect("the collection key is created as an array"),
            _ => unreachable!("shape always matches the root"),
        }
    }

    /// Replace the collection. The file is written back in its original shape.
    pub(crate) fn set_entries(&mut self, entries: Vec<Value>) {
        *self.entries_mut() = entries;
    }
}

/// Read `path`, treating a missing or empty file as "no entries yet".
pub(crate) fn load(path: &Path, key: &'static str) -> Result<ScheduleFile, String> {
    let missing = || ScheduleFile {
        root: Value::Object(serde_json::Map::new()),
        shape: Shape::Object,
        key,
    };
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(missing()),
        Err(err) => return Err(format!("读取 {} 失败：{err}", path.display())),
    };
    if raw.trim().is_empty() {
        return Ok(missing());
    }
    let root: Value = serde_json::from_str(&raw)
        .map_err(|err| format!("{} 不是合法 JSON：{err}", path.display()))?;
    let shape = match &root {
        Value::Array(_) => Shape::Array,
        Value::Object(map) => {
            if let Some(other) = map.get(key) {
                if !other.is_array() {
                    return Err(format!(
                        "{} 的 `{key}` 必须是数组，实际是{}",
                        path.display(),
                        type_name(other)
                    ));
                }
            }
            Shape::Object
        }
        other => {
            return Err(format!(
                "{} 顶层必须是对象或数组，实际是{}",
                path.display(),
                type_name(other)
            ))
        }
    };
    Ok(ScheduleFile { root, shape, key })
}

/// Write the file back, creating `.ki/` when needed.
///
/// Write-then-rename: a crash mid-write must not leave a truncated schedule that
/// the scheduler would report as a parse error.
pub(crate) fn save(path: &Path, file: &ScheduleFile) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("创建 {} 失败：{err}", parent.display()))?;
    }
    let mut text = serde_json::to_string_pretty(&file.root)
        .map_err(|err| format!("序列化 {} 失败：{err}", path.display()))?;
    text.push('\n');
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text).map_err(|err| format!("写入 {} 失败：{err}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|err| {
        let _ = fs::remove_file(&tmp);
        format!("替换 {} 失败：{err}", path.display())
    })
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "布尔值",
        Value::Number(_) => "数字",
        Value::String(_) => "字符串",
        Value::Array(_) => "数组",
        Value::Object(_) => "对象",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ki-schedule-file-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("schedule.json")
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let path = scratch("missing");
        let file = load(&path, "jobs").unwrap();
        assert!(file.entries().is_empty());
    }

    #[test]
    fn array_shape_round_trips() {
        let path = scratch("array");
        fs::write(&path, r#"[{"name":"a"}]"#).unwrap();
        let mut file = load(&path, "jobs").unwrap();
        assert_eq!(file.entries().len(), 1);
        file.set_entries(vec![serde_json::json!({"name": "b"})]);
        save(&path, &file).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.trim_start().starts_with('['),
            "shape must stay an array"
        );
        assert!(text.contains("\"b\""));
    }

    #[test]
    fn object_shape_keeps_unknown_top_level_keys() {
        let path = scratch("object");
        fs::write(&path, r#"{"jobs":[{"name":"a"}],"keep":true}"#).unwrap();
        let mut file = load(&path, "jobs").unwrap();
        file.set_entries(vec![serde_json::json!({"name": "b"})]);
        save(&path, &file).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["keep"], serde_json::json!(true));
        assert_eq!(value["jobs"][0]["name"], serde_json::json!("b"));
    }

    #[test]
    fn wrong_collection_type_is_reported() {
        let path = scratch("wrong");
        fs::write(&path, r#"{"jobs":{}}"#).unwrap();
        let error = load(&path, "jobs").unwrap_err();
        assert!(error.contains("必须是数组"), "{error}");
    }
}
