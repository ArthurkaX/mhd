//! dump_body — decompress and inspect captured request bodies from proxy.db.
//!
//! Usage:
//!   dump_body [--db <path>] [--run-id <id>] [--seq <n>] [--last] [--tools] [--raw]
//!
//! The proxy stores full request bodies (zstd) in `request_bodies` when
//! `corpus_capture = true`. This reads them back for offline inspection —
//! primarily to harvest an agent's tool-call stream (e.g. pi-lens
//! `ast_grep_replace` pattern→rewrite payloads) from a captured run.
//!
//! Modes:
//!   --list           list runs (run_id, rows, first/last ts, models) and exit
//!   --run-id N       restrict to one run (default: the most recent run)
//!   --seq M          one specific body; else --last picks the highest seq
//!   --tools          parse messages and print every tool_call (name + args),
//!                    handling both OpenAI (tool_calls[].function) and
//!                    Anthropic (content[].tool_use) shapes
//!   --raw            print the decompressed JSON body verbatim

use llm_proxy::config::config_dir;
use llm_proxy::db_log::decompress_body;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::path::PathBuf;

fn arg(flag: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == flag).and_then(|i| a.get(i + 1).cloned())
}
fn has(flag: &str) -> bool {
    std::env::args().any(|x| x == flag)
}

fn main() {
    let db_path = arg("--db")
        .map(PathBuf::from)
        .unwrap_or_else(|| config_dir().join("proxy.db"));
    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap_or_else(|_| panic!("cannot open {}", db_path.display()));

    if has("--list") {
        let mut stmt = conn
            .prepare(
                "SELECT run_id, COUNT(*), MIN(seq), MAX(seq), MIN(ts), MAX(ts), \
                 COALESCE(GROUP_CONCAT(DISTINCT model),'') \
                 FROM request_bodies GROUP BY run_id ORDER BY MAX(ts) DESC LIMIT 30",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })
            .unwrap();
        println!("run_id           rows  seq_lo..hi   first_ts             last_ts              models");
        for row in rows {
            let (run, n, lo, hi, t0, t1, models) = row.unwrap();
            println!(
                "{run:<15}  {n:>4}  {lo:>4}..{hi:<4}  {t0:.19}  {t1:.19}  {models}"
            );
        }
        return;
    }

    // Resolve run_id (default: most recent).
    let run_id: i64 = match arg("--run-id") {
        Some(s) => s.parse().expect("--run-id must be an integer"),
        None => conn
            .query_row(
                "SELECT run_id FROM request_bodies ORDER BY ts DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .expect("no request_bodies rows"),
    };

    // Resolve the body: explicit --seq, else the highest seq (fullest history).
    let (seq, body): (i64, Vec<u8>) = match arg("--seq") {
        Some(s) => {
            let seq: i64 = s.parse().expect("--seq must be an integer");
            let b = conn
                .query_row(
                    "SELECT body FROM request_bodies WHERE run_id=?1 AND seq=?2",
                    (run_id, seq),
                    |r| r.get(0),
                )
                .expect("no such (run_id, seq)");
            (seq, b)
        }
        None => conn
            .query_row(
                "SELECT seq, body FROM request_bodies WHERE run_id=?1 ORDER BY seq DESC LIMIT 1",
                (run_id,),
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("no bodies for run_id"),
    };

    let json = decompress_body(&body).expect("zstd decompress failed");
    eprintln!("# run_id={run_id} seq={seq} decompressed={} bytes", json.len());

    if has("--raw") {
        println!("{json}");
        return;
    }

    let v: Value = serde_json::from_str(&json).expect("body is not JSON");

    if has("--tools") {
        let calls = extract_tool_calls(&v);
        eprintln!("# {} tool-call(s) across message history", calls.len());
        for (i, (name, args)) in calls.iter().enumerate() {
            println!("--- [{i}] {name} ---");
            println!("{args}");
        }
        return;
    }

    // Default: a compact summary — model, message roles, tool schema names.
    let model = v.get("model").and_then(Value::as_str).unwrap_or("?");
    let msgs = v.get("messages").and_then(Value::as_array);
    let tools = v
        .get("tools")
        .and_then(Value::as_array)
        .map(|t| {
            t.iter()
                .filter_map(|d| {
                    d.get("name")
                        .or_else(|| d.get("function").and_then(|f| f.get("name")))
                        .and_then(Value::as_str)
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    println!("model: {model}");
    println!("messages: {}", msgs.map(|m| m.len()).unwrap_or(0));
    println!("tool schemas offered: [{tools}]");
    println!("tool-calls in history: {}", extract_tool_calls(&v).len());
    println!("(use --tools to dump them, --raw for the full body)");
}

/// Walk a chat-completions / messages request body and pull every tool call the
/// assistant made, as (tool_name, pretty_args). Handles both wire shapes:
///   OpenAI:    messages[].tool_calls[].function.{name, arguments(JSON string)}
///   Anthropic: messages[].content[] where {type:"tool_use", name, input}
fn extract_tool_calls(v: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(msgs) = v.get("messages").and_then(Value::as_array) else {
        return out;
    };
    for m in msgs {
        // OpenAI shape
        if let Some(tcs) = m.get("tool_calls").and_then(Value::as_array) {
            for tc in tcs {
                if let Some(f) = tc.get("function") {
                    let name = f.get("name").and_then(Value::as_str).unwrap_or("?").to_string();
                    let args = f.get("arguments").and_then(Value::as_str).unwrap_or("");
                    let pretty = serde_json::from_str::<Value>(args)
                        .ok()
                        .and_then(|p| serde_json::to_string_pretty(&p).ok())
                        .unwrap_or_else(|| args.to_string());
                    out.push((name, pretty));
                }
            }
        }
        // Anthropic shape
        if let Some(content) = m.get("content").and_then(Value::as_array) {
            for c in content {
                if c.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let name = c.get("name").and_then(Value::as_str).unwrap_or("?").to_string();
                    let input = c.get("input").cloned().unwrap_or(Value::Null);
                    let pretty =
                        serde_json::to_string_pretty(&input).unwrap_or_else(|_| input.to_string());
                    out.push((name, pretty));
                }
            }
        }
    }
    out
}
