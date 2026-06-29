//! Trim baseline harness — offline replay/measure tool.
//!
//! Generates realistic Claude Code session fixtures, runs each through
//! `llmtrim_core::rewrite_request` with the current production configs,
//! and reports per-turn token savings.
//!
//! # Usage
//!
//! ```bash
//! cargo test --package mhd-llm-proxy --test trim_harness -- --nocapture
//! ```
//!
//! The fixtures are written to `fixtures/session-01/` for inspection.
//! Output is a CSV-compatible table and aggregate summary.

use llmtrim_core::compress_with_config;
use llmtrim_core::config::DenseConfig;
use llmtrim_core::ir::ProviderKind;
use serde_json::Value;

// ── Fixture generation ──────────────────────────────────────────────────────

/// Per-fixture metadata returned by the generator.
struct Fixture {
    turn: usize,
    description: String,
    body: Value,
}

/// Generate a realistic Claude Code session with N turns.
///
/// Each turn appends the previous conversation history, simulating a real
/// agent loop. Turn 1 has only the initial user request; later turns have
/// growing tool-result history.
fn generate_session(n_turns: usize) -> Vec<Fixture> {
    let mut fixtures = Vec::with_capacity(n_turns);

    // Common system prompt (Claude Code style, ~3k tokens of instructions)
    let system_text = build_system_prompt();
    let tools = build_tool_definitions();
    let system_block = serde_json::json!({
        "type": "text",
        "text": system_text,
        "cache_control": {"type": "ephemeral"}
    });

    let mut history: Vec<(Value, Value)> = Vec::new();

    for turn in 1..=n_turns {
        let (user_msg, assistant_msg) = turn_content(turn, &history);
        history.push((user_msg.clone(), assistant_msg.clone()));

        let mut messages: Vec<Value> = Vec::new();
        for (u, a) in &history {
            messages.push(serde_json::json!({"role": "user", "content": u}));
            messages.push(serde_json::json!({"role": "assistant", "content": a}));
        }

        let body = serde_json::json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 8192,
            "system": [system_block],
            "tools": tools,
            "messages": messages
        });

        let desc = match turn {
            1 => "initial request".into(),
            t if t <= 4 => format!("read/grep turn {}", t),
            t if t <= 6 => format!("bash/build turn {}", t),
            _ => format!("edit/code turn {}", turn),
        };

        fixtures.push(Fixture {
            turn,
            description: desc,
            body,
        });
    }

    fixtures
}

/// Build a realistic system prompt (~2-3k tokens).
fn build_system_prompt() -> String {
    let mut text = String::with_capacity(3000);
    text.push_str(
        "You are Claude, an AI assistant powered by Anthropic's Claude 3.5 Sonnet model.
You are a highly skilled AI pair programmer with access to a set of tools that help you
understand and modify codebases, run commands, and complete software engineering tasks.

When the user asks a question or describes a task, use the available tools to:
1. Explore and understand the codebase
2. Make changes when requested
3. Verify your work

## Key guidelines

- Read files before editing them to understand their content
- Use grep to find relevant code patterns and definitions
- Run build/test commands to verify changes
- Write clear commit messages when asked to commit
- If a tool returns an error, diagnose the issue and fix it
- When exploring, start broad then narrow down

## Code style preferences

The project follows standard Rust conventions with some specific patterns:
- Use anyhow for error handling
- Prefer async/await with tokio
- Use RwLock for shared state, Mutex only when necessary
- Document public APIs with doc comments
- Use serde for serialization with rename_all = \"camelCase\"

## Response format

You should respond with a mix of:
1. Your reasoning and plan (in text blocks)
2. Tool calls to execute actions
3. Summary of what you found or changed

Be concise but thorough. Explain your reasoning before making significant changes.
If you're unsure about something, ask clarifying questions rather than guessing.
",
    );
    text.push_str("\n\n## Available tools\n\n");
    text.push_str("You have access to tools for reading files, writing files, running commands, searching, and more.
Each tool has specific parameters documented in its schema. Use the most appropriate tool
for each task. When reading files, prefer reading specific line ranges for large files.
When searching, use specific patterns and narrow results with flags.
When running commands, prefer non-interactive commands with clear output.
");
    text.push_str("\n\n## Session context\n\n");
    text.push_str(
        "This is an ongoing software engineering session. The user will provide tasks,
and you should work through them systematically. Keep track of what you've
learned about the codebase and use that knowledge efficiently in later turns.
Avoid repeating the same exploration. If you already know the answer from
a previous tool call, reference it rather than re-reading.
",
    );
    text
}

/// Build ~30 tool definitions matching Claude Code's actual toolset.
fn build_tool_definitions() -> Vec<Value> {
    let mut tools: Vec<Value> = Vec::with_capacity(32);

    let mut add_tool = |name: &str, description: &str, properties: Value, required: Vec<&str>| {
        tools.push(serde_json::json!({
            "name": name,
            "description": description,
            "input_schema": {
                "type": "object",
                "properties": properties,
                "required": required
            }
        }));
    };

    add_tool(
        "Read",
        "Read the contents of a file from the local filesystem. Reads the entire file or a specific line range. Use for understanding existing code.",
        serde_json::json!({
            "file_path": {"type": "string", "description": "The absolute or relative path to the file to read."},
            "offset": {"type": "integer", "description": "Optional starting line number (0-indexed)."},
            "limit": {"type": "integer", "description": "Optional maximum number of lines to read."}
        }),
        vec!["file_path"],
    );

    add_tool(
        "Write",
        "Write content to a file, creating it or overwriting existing content. Use for making changes or creating new files.",
        serde_json::json!({
            "file_path": {"type": "string", "description": "The absolute or relative path of the file to write."},
            "content": {"type": "string", "description": "The full content to write to the file."}
        }),
        vec!["file_path", "content"],
    );

    add_tool(
        "Edit",
        "Apply a structured diff or search-and-replace to an existing file. Less disruptive than Write when making targeted changes.",
        serde_json::json!({
            "file_path": {"type": "string", "description": "The absolute or relative path of the file to edit."},
            "old_string": {"type": "string", "description": "The exact text to find and replace."},
            "new_string": {"type": "string", "description": "The replacement text."}
        }),
        vec!["file_path", "old_string", "new_string"],
    );

    add_tool(
        "Bash",
        "Run a shell command on the local system. The command runs in the project root directory. Use for builds, tests, git operations, and other terminal tasks.",
        serde_json::json!({
            "command": {"type": "string", "description": "The shell command to execute."},
            "timeout": {"type": "integer", "description": "Optional timeout in milliseconds."},
            "description": {"type": "string", "description": "Optional human-readable description of what this command does."}
        }),
        vec!["command"],
    );

    add_tool(
        "Grep",
        "Search for a pattern across files in the project. Supports regex patterns and file globs. Use for finding definitions, usages, and references.",
        serde_json::json!({
            "pattern": {"type": "string", "description": "The regex pattern to search for."},
            "include": {"type": "string", "description": "Optional glob pattern to filter files (e.g. '*.rs')."},
            "context": {"type": "integer", "description": "Optional number of context lines before/after each match."}
        }),
        vec!["pattern"],
    );

    add_tool(
        "Glob",
        "Find files matching a glob pattern in the project. Use for locating files by name or extension.",
        serde_json::json!({
            "pattern": {"type": "string", "description": "The glob pattern to match (e.g. '**/*.rs')."}
        }),
        vec!["pattern"],
    );

    add_tool(
        "Dispatch",
        "Delegate a subtask to another model for parallel processing. Use for independent sub-tasks that can run concurrently.",
        serde_json::json!({
            "task": {"type": "string", "description": "The task description to delegate."},
            "context": {"type": "string", "description": "Context information needed for the subtask."}
        }),
        vec!["task"],
    );

    add_tool(
        "WebFetch",
        "Fetch the content of a URL and return its text. Use for reading documentation, API specs, or web pages.",
        serde_json::json!({
            "url": {"type": "string", "description": "The URL to fetch."}
        }),
        vec!["url"],
    );

    add_tool(
        "SearchReplace",
        "Apply a series of search-and-replace operations across multiple files. More efficient than individual Edit calls for large-scale changes.",
        serde_json::json!({
            "operations": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "file_path": {"type": "string", "description": "Path to the file."},
                        "old_string": {"type": "string", "description": "Text to find."},
                        "new_string": {"type": "string", "description": "Replacement text."}
                    }
                }
            }
        }),
        vec!["operations"],
    );

    add_tool(
        "Execute",
        "Run a compiled binary or script with arguments. More targeted than Bash for running specific programs.",
        serde_json::json!({
            "command": {"type": "string", "description": "The executable path or name."},
            "args": {"type": "array", "items": {"type": "string"}, "description": "Arguments to pass."},
            "working_dir": {"type": "string", "description": "Optional working directory."}
        }),
        vec!["command"],
    );

    add_tool(
        "FileSearch",
        "Search for files by their content or metadata within the project directory tree. Supports fuzzy matching for filenames.",
        serde_json::json!({
            "query": {"type": "string", "description": "The filename or path fragment to search for."},
            "max_results": {"type": "integer", "description": "Maximum number of results to return."}
        }),
        vec!["query"],
    );

    add_tool(
        "DirectoryTree",
        "List the contents of a directory with nesting depth control. Use for understanding the project structure.",
        serde_json::json!({
            "path": {"type": "string", "description": "The directory path to list."},
            "max_depth": {"type": "integer", "description": "Optional maximum depth of subdirectories."}
        }),
        vec!["path"],
    );

    add_tool(
        "FileCreate",
        "Create a new file with specified content. Fails if file already exists. Use for adding new files to the project.",
        serde_json::json!({
            "file_path": {"type": "string", "description": "Path for the new file."},
            "content": {"type": "string", "description": "Initial content of the file."}
        }),
        vec!["file_path", "content"],
    );

    // Pad with generic tools to reach ~30 total
    for name in [
        "LintTool",
        "FormatTool",
        "DocGenTool",
        "RefactorTool",
        "AnalyzeTool",
        "BenchmarkTool",
        "TestRunner",
        "CoverageTool",
        "DependencyTool",
        "ConfigTool",
        "DeployTool",
        "LogViewer",
        "MetricTool",
        "AlertTool",
        "DashboardTool",
        "SchemaTool",
        "MigrationTool",
        "SeedTool",
        "QueryTool",
        "ReportTool",
    ] {
        add_tool(
            name,
            &format!(
                "{} for software engineering tasks. Supports various operations and configurations relevant to its domain.",
                name
            ),
            serde_json::json!({
                "action": {"type": "string", "description": format!("The specific {} action to perform", name)},
                "params": {"type": "object", "description": "Optional parameters for the action."}
            }),
            vec!["action"],
        );
    }

    tools
}

/// Build a unique multi-paragraph prose reflection for a given turn, seeded
/// by the turn number and a prefix phrase so that each turn produces genuinely
/// *different* text (cross-turn dedup cannot collapse identical content).
fn build_reflection(turn: usize, phrase: &str, aspect: &str) -> String {
    format!(
        "In turn {turn}, focusing on {aspect}, I need to consider several things. \
        {phrase} The main concern is that the existing implementation handles \
        the common case but not the edge cases that emerge under production load. \
        For example, when the system processes multiple concurrent requests, the \
        lock contention on the shared state becomes a bottleneck. The fix should \
        minimize the critical section and prefer read-optimised data structures \
        where the workload is read-heavy.\n\n\
        Looking deeper at turn {turn}'s specific context, the interaction between \
        the routing layer and the upstream selection is more coupled than it \
        appears. The current `classify` function makes a single pass over headers \
        but doesn't account for the fallback ordering when multiple targets match. \
        A cleaner design would separate the classification concern from the target \
        resolution, making it possible to unit-test each step independently.\n\n\
        Furthermore, the error handling in the {aspect} path needs attention. The \
        current code uses `unwrap()` in several hot-path locations that would cause \
        panics under unexpected upstream behaviour. Replacing these with proper \
        Result propagation and structured error types would make the proxy more \
        resilient. The deployment that introduced this code predated our structured \
        error framework, so the migration was never completed for this module.\n\n\
        I'll start by examining the relevant source files to confirm these \
        observations, then apply targeted fixes in order of impact. The most \
        important change is reducing the lock contention, which alone should \
        improve throughput by 15-20% under the workloads we see in production.\n\n\
        This analysis was prompted by the specific conditions in turn {turn}. \
        The production metrics show that most latency outliers cluster around \
        the {aspect} path under mixed workloads. Looking at the recent traces, \
        the 99th percentile latency spiked to 8 seconds on the route that \
        triggers this code path, while the median stayed under 200 ms. This \
        bimodal distribution is a clear sign of lock contention or resource \
        exhaustion on the shared state, not a general throughput problem.\n\n\
        A targeted fix for the {aspect} path would involve: (a) profiling the \
        hot path to identify the exact contention point, (b) restructuring the \
        lock to cover only the minimum critical section, and (c) adding a \
        read-through cache for the common lookup pattern so that repeated \
        lookups within the same request don't re-acquire the lock. The third \
        change alone could eliminate 60% of the lock acquisitions on the hot \
        path, based on the observation that most requests look up the same \
        target for all messages in a conversation.",
    )
}

/// Generate the user message and assistant response for a given turn index.
///
/// Each turn's user message and assistant reply carry *unique* prose paragraphs
/// that vary with the turn number, reducing cross-turn text dedup so that
/// savings approach the ~25% seen in production rather than the pathologically
/// high 50%+ caused by identical repeated patterns.
fn turn_content(turn: usize, _history: &[(Value, Value)]) -> (Value, Value) {
    // Per-turn unique reflections — never the same text across turns.
    let user_note = build_reflection(
        turn,
        "The user is asking about the proxy architecture.",
        "classification",
    );
    let asst_note = build_reflection(
        turn + 10,
        "The assistant is analysing the routing setup.",
        "routing",
    );
    let summary_note = build_reflection(
        turn + 20,
        "After completing the analysis, the key findings are summarised.",
        "summary",
    );
    let (user, mut assistant) = match turn {
        1 => {
            let user = serde_json::json!([
                {"type": "text", "text": format!("I'm working on a Rust project called 'mhd'. Can you help me understand the codebase structure, the main components, and how the proxy routing works? Start by exploring the project tree and key files.\n\n{}", user_note)}
            ]);
            let assistant = serde_json::json!([
                {"type": "text", "text": format!("{}\n\nI'll explore the project structure first to understand the codebase layout.", asst_note)},
                {"type": "tool_use", "id": "tu_a1", "name": "DirectoryTree", "input": {"path": ".", "max_depth": 3}}
            ]);
            (user, assistant)
        }
        2 => {
            let tree_output = build_directory_tree_output();
            let user = serde_json::json!([
                {"type": "tool_result", "tool_use_id": "tu_a1", "content": tree_output},
                {"type": "text", "text": format!("Great, that shows the structure. Now let me understand the main entry points. Read the key files in the proxy module.\n\n{}", user_note)}
            ]);
            let assistant = serde_json::json!([
                {"type": "text", "text": format!("{}\n\nLet me look at the main source files to understand the proxy architecture. I need to check how routes are dispatched and whether the tier classification matches what we configure at startup.", asst_note)},
                {"type": "tool_use", "id": "tu_a2", "name": "Read", "input": {"file_path": "llm-proxy/src/lib.rs"}},
                {"type": "tool_use", "id": "tu_a3", "name": "Read", "input": {"file_path": "llm-proxy/src/handlers.rs"}}
            ]);
            (user, assistant)
        }
        3 => {
            let lib_rs = build_code_file(300);
            let handlers_rs = build_code_file(450);
            let user = serde_json::json!([
                {"type": "tool_result", "tool_use_id": "tu_a2", "content": lib_rs},
                {"type": "tool_result", "tool_use_id": "tu_a3", "content": handlers_rs},
                {"type": "text", "text": format!("Now I need to understand how routing decisions are made. Search for the tier classification logic and how targets are resolved.\n\n{}", user_note)}
            ]);
            let assistant = serde_json::json!([
                {"type": "text", "text": format!("{}\n\nLet me search for the routing and tier logic. I'm looking for how `target_for` maps tier names to upstream targets, and whether the `classify_request` function handles model aliases correctly.", asst_note)},
                {"type": "tool_use", "id": "tu_a4", "name": "Grep", "input": {"pattern": "fn target_for|Tier::", "include": "*.rs"}},
                {"type": "tool_use", "id": "tu_a5", "name": "Read", "input": {"file_path": "llm-proxy/src/state.rs"}}
            ]);
            (user, assistant)
        }
        4 => {
            let grep_out = build_grep_output(60);
            let state_rs = build_code_file(280);
            let user = serde_json::json!([
                {"type": "tool_result", "tool_use_id": "tu_a4", "content": grep_out},
                {"type": "tool_result", "tool_use_id": "tu_a5", "content": state_rs},
                {"type": "text", "text": format!("I see the Tier enum and the target resolution. Now let me understand how the upstream provider works. Read the upstream module and the anthropic passthrough provider.\n\n{}", user_note)}
            ]);
            let assistant = serde_json::json!([
                {"type": "text", "text": format!("{}\n\nLet me examine the provider implementations to understand the full picture, especially how the upstream gateway response is parsed and how usage data flows back to caller.", asst_note)},
                {"type": "tool_use", "id": "tu_a6", "name": "Glob", "input": {"pattern": "llm-proxy/src/providers/**/*.rs"}},
                {"type": "tool_use", "id": "tu_a7", "name": "Read", "input": {"file_path": "llm-proxy/src/providers/upstream.rs", "limit": 100}}
            ]);
            (user, assistant)
        }
        5 => {
            let glob_out = "llm-proxy/src/providers/mod.rs\nllm-proxy/src/providers/anthropic.rs\nllm-proxy/src/providers/upstream.rs";
            let upstream_rs = build_code_file(200);
            let user = serde_json::json!([
                {"type": "tool_result", "tool_use_id": "tu_a6", "content": glob_out},
                {"type": "tool_result", "tool_use_id": "tu_a7", "content": upstream_rs},
                {"type": "text", "text": format!("Now let me try building the project to see if there are any compilation issues.\n\n{}", user_note)}
            ]);
            let assistant = serde_json::json!([
                {"type": "text", "text": format!("{}\n\nLet me run the build to check for compilation errors. I want to verify that the latest edits compile before moving on to fixes.", asst_note)},
                {"type": "tool_use", "id": "tu_a8", "name": "Bash", "input": {"command": "cargo check 2>&1"}}
            ]);
            (user, assistant)
        }
        6 => {
            let build_log = build_build_log(400);
            let user = serde_json::json!([
                {"type": "tool_result", "tool_use_id": "tu_a8", "content": build_log},
                {"type": "text", "text": format!("There are some warnings and a few errors. Let me fix the compilation errors first. The issue seems to be in the upstream provider module.\n\n{}", user_note)}
            ]);
            let assistant = serde_json::json!([
                {"type": "text", "text": format!("{}\n\nI see the compilation errors. Let me read the relevant source files and check what is causing the type mismatch, then apply targeted fixes.", asst_note)},
                {"type": "tool_use", "id": "tu_a9", "name": "Read", "input": {"file_path": "llm-proxy/src/providers/upstream.rs", "offset": 120, "limit": 80}},
                {"type": "tool_use", "id": "tu_a10", "name": "Grep", "input": {"pattern": "unused_import|dead_code|deprecated", "include": "*.rs"}}
            ]);
            (user, assistant)
        }
        7 => {
            let upstream_snippet = build_code_file(60);
            let grep_warnings = "src/providers/upstream.rs:15: warning: unused import `std::collections::HashMap`\n\
                                src/providers/upstream.rs:89: warning: unused variable `config`\n\
                                src/providers/upstream.rs:145: warning: deprecated method `send`\n\
                                src/state.rs:200: warning: unused field `last_seen`\n\
                                src/state.rs:310: warning: variable does not need to be mutable\n\
                                src/handlers.rs:50: warning: unused import `Bytes`\n\
                                src/handlers.rs:180: warning: unused variable `response`\n\
                                src/config.rs:25: warning: field `debug_mode` is never read\n\
                                src/trim.rs:12: warning: unused import `std::time::Instant`\n\
                                src/trim.rs:45: warning: unused function `log_compression_stats`";
            let user = serde_json::json!([
                {"type": "tool_result", "tool_use_id": "tu_a9", "content": upstream_snippet},
                {"type": "tool_result", "tool_use_id": "tu_a10", "content": grep_warnings},
                {"type": "text", "text": format!("I see - there are unused imports and deprecated methods. Let me fix these issues. Edit the upstream.rs file to fix the deprecated method call and remove unused imports.\n\n{}", user_note)}
            ]);
            let assistant = serde_json::json!([
                {"type": "text", "text": format!("{}\n\nI'll fix each issue systematically. Starting with the deprecated method and unused imports, then I'll verify the build after each change.", asst_note)},
                {"type": "tool_use", "id": "tu_a11", "name": "Edit", "input": {
                    "file_path": "llm-proxy/src/providers/upstream.rs",
                    "old_string": "use std::collections::HashMap;\n",
                    "new_string": ""
                }},
                {"type": "tool_use", "id": "tu_a12", "name": "Edit", "input": {
                    "file_path": "llm-proxy/src/providers/upstream.rs",
                    "old_string": ".send()",
                    "new_string": ".send().await"
                }}
            ]);
            (user, assistant)
        }
        8 => {
            let edit_result_1 = "Applied edit: removed unused import";
            let edit_result_2 = "Error: the method `.send()` does not exist on `RequestBuilder`.";
            let user = serde_json::json!([
                {"type": "tool_result", "tool_use_id": "tu_a11", "content": edit_result_1},
                {"type": "tool_result", "tool_use_id": "tu_a12", "content": edit_result_2},
                {"type": "text", "text": format!("The first edit worked, but the second one needs a different approach. Let me check the reqwest API and fix this properly.\n\n{}", user_note)}
            ]);
            let assistant = serde_json::json!([
                {"type": "text", "text": format!("{}\n\nLet me check how the client is set up in the upstream module to understand the correct calling convention. The error suggests we're calling the method on the request builder instead of passing it to the client.", asst_note)},
                {"type": "tool_use", "id": "tu_a13", "name": "Read", "input": {"file_path": "llm-proxy/src/providers/upstream.rs", "offset": 1, "limit": 30}},
                {"type": "tool_use", "id": "tu_a14", "name": "Grep", "input": {"pattern": "reqwest::Client|\\.send\\(", "include": "*.rs"}}
            ]);
            (user, assistant)
        }
        9 => {
            let client_setup = "use reqwest::Client;\nuse std::sync::Arc;\n\npub struct UpstreamProvider {\n    client: Client,\n    base_url: String,\n}\n\nimpl UpstreamProvider {\n    pub fn new(base_url: &str) -> Self {\n        Self {\n            client: Client::new(),\n            base_url: base_url.to_string(),\n        }\n    }\n}";
            let grep_send = "src/providers/upstream.rs:45:    let response = client.send(request).await?;\nsrc/providers/anthropic.rs:30:    let response = client.send(request).await?;\nsrc/providers/mod.rs:15:    let response = client.post(url).send().await?;\nsrc/providers/upstream.rs:145:    let response = request.send().await?;";
            let user = serde_json::json!([
                {"type": "tool_result", "tool_use_id": "tu_a13", "content": client_setup},
                {"type": "tool_result", "tool_use_id": "tu_a14", "content": grep_send},
                {"type": "text", "text": format!("Now I understand. The pattern is `client.send(request)` not `request.send()`. Let me apply the correct fix.\n\n{}", user_note)}
            ]);
            let assistant = serde_json::json!([
                {"type": "text", "text": format!("{}\n\nI see - the correct pattern uses the Client directly. The reqwest crate expects `client.send(request)` rather than `request.send()`. Let me apply the proper fix and verify it compiles.", asst_note)},
                {"type": "tool_use", "id": "tu_a15", "name": "Edit", "input": {
                    "file_path": "llm-proxy/src/providers/upstream.rs",
                    "old_string": "request.send().await?",
                    "new_string": "client.send(request).await?"
                }},
                {"type": "tool_use", "id": "tu_a16", "name": "Bash", "input": {"command": "cargo check 2>&1 | tail -50"}}
            ]);
            (user, assistant)
        }
        _ => {
            // Turn 10: final build check
            let final_build = build_build_log(80);
            let user = serde_json::json!([
                {"type": "tool_result", "tool_use_id": "tu_a15", "content": "Applied edit: fixed method call pattern"},
                {"type": "tool_result", "tool_use_id": "tu_a16", "content": final_build},
                {"type": "text", "text": format!("The build passes now. Let me also run the tests to make sure everything works.\n\n{}", user_note)}
            ]);
            let assistant = serde_json::json!([
                {"type": "text", "text": format!("{}\n\nGreat, the build succeeds. Let me run the tests to verify everything works correctly before declaring this done.", asst_note)},
                {"type": "tool_use", "id": "tu_a17", "name": "Bash", "input": {"command": "cargo test 2>&1 | tail -100"}}
            ]);
            (user, assistant)
        }
    };
    // Add a 3rd unique reflection block to every assistant response to
    // further increase the non-compressible surface area per turn.
    if let Some(arr) = assistant.as_array_mut() {
        arr.push(serde_json::json!({"type": "text", "text": summary_note}));
    }
    (user, assistant)
}

/// Generate a realistic directory tree output.
fn build_directory_tree_output() -> String {
    r#"📁 mhd/
├── Cargo.toml
├── Cargo.lock
├── README.md
├── llm-proxy/
│   ├── Cargo.toml
│   ├── src/
│   │   ├── lib.rs
│   │   ├── main.rs
│   │   ├── handlers.rs
│   │   ├── state.rs
│   │   ├── config.rs
│   │   ├── trim.rs
│   │   ├── transform.rs
│   │   ├── db_log.rs
│   │   ├── providers/
│   │   │   ├── mod.rs
│   │   │   ├── anthropic.rs
│   │   │   └── upstream.rs
│   │   └── overlays/
│   │       ├── mod.rs
│   │       └── proxy_trace.rs
├── mhd-daemon/
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs
│       ├── config/
│       │   └── mod.rs
│       ├── core/
│       │   └── llm_proxy.rs
│       └── overlays/
│           ├── mod.rs
│           ├── tray.rs
│           └── proxy_trace.rs
├── vendor/
│   └── llmtrim/
│       └── src/
│           └── lib.rs
"#
    .to_string()
}

/// Build a code file with realistic Rust content.
///
/// Uses a large pool of *distinct*, unique lines from a real codebase so that
/// the trim algorithm sees varied content rather than cycling through a small
/// fixed set of identical patterns. This prevents the fixtures from being
/// pathologically compressible compared to real traffic.
fn build_code_file(line_count: usize) -> String {
    let mut content = String::with_capacity(line_count * 80);
    content.push_str("// Copyright 2025 — MHD proxy fixture\n\n");

    // Large pool of unique Rust code lines drawn from different domains.
    // Each call uses a different starting offset so even the same `line_count`
    // produces a different output.
    let pool: &[&str] = &[
        "use std::sync::Arc;\n",
        "use std::collections::HashMap;\n",
        "use std::time::Duration;\n",
        "use tokio::sync::RwLock;\n",
        "use tokio::time::{sleep, timeout};\n",
        "use serde::{Deserialize, Serialize};\n",
        "use anyhow::{Context, Result, bail};\n",
        "use tracing::{debug, info, warn, error};\n",
        "/// Resolve the upstream target for a routing tier.\n",
        "pub fn resolve_target(tier: Tier, key: &ApiKey) -> Option<Target> {\n",
        "    let overrides = OVERRIDES.read().unwrap_or_else(|e| e.into_inner());\n",
        "    if let Some(t) = overrides.get(&(tier, key.id())) { return Some(t.clone()); }\n",
        "    DEFAULTS.get(&tier).cloned()\n",
        "}\n",
        "#[derive(Debug, Clone, PartialEq, Eq, Hash)]\n",
        "pub enum Tier { Opus, Sonnet, Haiku, Fable }\n",
        "impl Tier {\n",
        "    pub fn downgradeable(&self) -> bool { matches!(self, Self::Opus | Self::Sonnet) }\n",
        "    pub fn budget_multiplier(&self) -> f64 { match self { Self::Opus => 1.0, _ => 0.5 } }\n",
        "}\n",
        "pub struct UpstreamProvider {\n",
        "    client: reqwest::Client,\n",
        "    base_url: String,\n",
        "    pending: Arc<std::sync::atomic::AtomicU64>,\n",
        "}\n",
        "impl UpstreamProvider {\n",
        "    pub fn new(base_url: &str) -> Result<Self> {\n",
        "        let client = reqwest::Client::builder()\n",
        "            .timeout(Duration::from_secs(60))\n",
        "            .pool_max_idle_per_host(32)\n",
        "            .build()?;\n",
        "        Ok(Self { client, base_url: base_url.into(), pending: Arc::new(0.into()) })\n",
        "    }\n",
        "    pub async fn send(&self, body: Value) -> Result<Value> {\n",
        "        self.pending.fetch_add(1, std::sync::atomic::Ordering::SeqCst);\n",
        "        let resp = self.client.post(&self.base_url)\n",
        "            .header(\"Content-Type\", \"application/json\")\n",
        "            .json(&body).send().await?;\n",
        "        self.pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);\n",
        "        if !resp.status().is_success() {\n",
        "            bail!(\"upstream {}: {}\", resp.status(), resp.text().await.unwrap_or_default());\n",
        "        }\n",
        "        Ok(resp.json().await?)\n",
        "    }\n",
        "}\n",
        "pub fn backoff(attempt: u32) -> Duration {\n",
        "    Duration::from_millis((50u64 * 2u64.pow(attempt)).min(8000))\n",
        "}\n",
        "#[derive(Debug)]\n",
        "pub enum ProxyError { Rejected(String), Upstream(String), Timeout, Internal }\n",
        "impl std::fmt::Display for ProxyError {\n",
        "    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {\n",
        "        match self { Self::Rejected(r) => write!(f, \"rejected: {r}\"), _ => write!(f, \"{self:?}\") }\n",
        "    }\n",
        "}\n",
        "impl std::error::Error for ProxyError {}\n",
        "pub fn classify_request(headers: &[(&str, &str)], body: &Value) -> &'static str {\n",
        "    if let Some(route) = body.get(\"x-tier\").and_then(|v| v.as_str()) {\n",
        "        return route;\n",
        "    }\n",
        "    body.get(\"model\").and_then(|m| m.as_str()).unwrap_or(\"unknown\")\n",
        "}\n",
        "pub async fn forward_to_upstream(client: &reqwest::Client, url: &str, key: &str,\n",
        "    body: Value) -> Result<Value> {\n",
        "    let resp = client.post(url).header(\"Authorization\", format!(\"Bearer {key}\"))\n",
        "        .json(&body).send().await?;\n",
        "    let status = resp.status();\n",
        "    let text = resp.text().await?;\n",
        "    if status.is_success() {\n",
        "        serde_json::from_str(&text).context(\"parse upstream response\")\n",
        "    } else { bail!(\"upstream {status}: {text}\") }\n",
        "}\n",
        "pub fn estimate_tokens(body: &Value) -> u64 {\n",
        "    serde_json::to_string(body).unwrap_or_default().len() as u64 / 4\n",
        "}\n",
        "pub fn now_ms() -> u64 {\n",
        "    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)\n",
        "        .unwrap_or_default().as_millis() as u64\n",
        "}\n",
        "pub const MAX_INFLIGHT: usize = 64;\n",
        "pub const SLOW_THRESHOLD_MS: u64 = 2000;\n",
        "pub const TRIM_MIN_BYTES: usize = 4096;\n",
        "pub const CACHE_CONTROL_HEADER: &str = \"x-cache-control\";\n",
        "#[cfg(test)]\n",
        "mod tests {\n",
        "    use super::*;\n",
        "    #[test]\n",
        "    fn test_backoff() { assert!(backoff(0) < backoff(5)); }\n",
        "    #[tokio::test]\n",
        "    async fn test_upstream() { }\n",
        "}\n",
    ];

    // Each fixture gets a deterministic offset so the same code lines cycled
    // produce a different unwinding per turn, reducing cross-turn dedup.
    let offset = (line_count * 7) % pool.len();
    for i in 0..line_count {
        let idx = (offset + i) % pool.len();
        content.push_str(pool[idx]);
    }
    content
}

/// Build a realistic grep output with many matches.
///
/// Each match uses a *different* function name and line range so the trim
/// algorithm sees varied content rather than the same line repeated N times.
fn build_grep_output(match_count: usize) -> String {
    let mut out = String::with_capacity(match_count * 80);

    let files = [
        "src/lib.rs",
        "src/handlers.rs",
        "src/state.rs",
        "src/config.rs",
        "src/trim.rs",
        "src/providers/mod.rs",
        "src/providers/anthropic.rs",
        "src/providers/upstream.rs",
        "src/db_log.rs",
    ];
    let funcs = [
        "resolve_target",
        "classify_request",
        "forward_to_upstream",
        "handle_message",
        "stream_response",
        "parse_usage",
        "build_cache_reason",
        "route_request",
        "apply_trim",
        "downgrade_tier",
        "update_trace",
    ];

    for i in 0..match_count {
        let file = files[i % files.len()];
        let line = 15 + (i * 13) % 350;
        let func = funcs[(i + match_count / 3) % funcs.len()];
        out.push_str(&format!("{}:{}:    fn {}() {{\n", file, line, func));
    }

    out
}

/// Build a realistic build log with varied warnings and errors across multiple
/// source files. Each log has unique file paths, line numbers, and message
/// text so the trim algorithm sees diverse content per turn.
fn build_build_log(line_count: usize) -> String {
    let mut log = String::with_capacity(line_count * 70);
    log.push_str("    Checking mhd-llm-proxy v0.6.0\n");
    log.push_str("    Checking mhd-daemon v0.6.0\n");

    let files = [
        "src/providers/upstream.rs",
        "src/handlers.rs",
        "src/state.rs",
        "src/trim.rs",
        "src/config.rs",
        "src/providers/mod.rs",
        "src/providers/anthropic.rs",
    ];
    let errors = [
        (
            "E0599",
            "no method named `send` found for struct `RequestBuilder`",
            "    let response = request.send().await?;\n",
            "    ^^^^ method not found",
            "items from traits can only be used if the trait is in scope",
        ),
        (
            "E0308",
            "mismatched types: expected `Result` but found `Option`",
            "    let val = map.get(key).unwrap();\n",
            "    ^^^^^^^^^^^^^^^^^ expected `Result`",
            "change return type or use `.ok_or()`",
        ),
        (
            "E0061",
            "this function takes 2 arguments but 1 argument was supplied",
            "    parse_usage(body);\n",
            "    ^^^^^^^^^^^^^ expected 2 arguments",
            "supply the missing `&str` argument",
        ),
        (
            "E0277",
            "the trait bound `Future` is not satisfied",
            "    tokio::spawn(handler());\n",
            "    ^^^^^^^^^^^ expected `Send` trait",
            "use `tokio::spawn` only with `Send + 'static` futures",
        ),
        (
            "E0382",
            "borrow of moved value: `response`",
            "    let text = response.text().await?;\n    let _ = status;\n",
            "    ^^^^^^^^ value borrowed after move",
            "clone `response` or restructure the borrows",
        ),
    ];

    for i in 0..line_count.min(120) {
        let file = files[(i * 7) % files.len()];
        let line = 20 + (i * 11) % 500;
        if i % 15 == 3 && i < 50 {
            let (code, msg, src, underline, help) = errors[(i / 15) % errors.len()];
            let col = (i % 40) + 1;
            log.push_str(&format!("error[{code}]: {msg}\n"));
            log.push_str(&format!("   --> {file}:{line}:{col}\n"));
            log.push_str("    |\n");
            log.push_str(&format!("{line} | {src}"));
            log.push_str(&format!("    |     {underline}\n"));
            log.push_str(&format!("    |\n    = help: {help}\n"));
        } else if i % 8 == 0 {
            log.push_str(&format!(
                "   Compiling mhd-llm-proxy ({}%)\n",
                (i * 100 / line_count).min(99)
            ));
        } else if i % 5 == 0 {
            log.push_str(&format!(
                "warning: unused variable `ctx_{0}` in {1}:{2}\n",
                i, file, line
            ));
        } else if i % 3 == 0 {
            log.push_str(&format!(
                "warning: field `detail_{0}` is never read in {1}:{2}\n",
                i, file, line
            ));
        } else {
            log.push_str(&format!(
                "    {} more files to compile...\n",
                line_count - i
            ));
        }
    }

    if line_count > 40 {
        log.push_str(&format!(
            "error: could not compile `mhd-llm-proxy` due to {} previous errors\n",
            line_count / 15
        ));
    } else {
        log.push_str("    Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.34s\n");
    }

    log
}

// ── Harness ─────────────────────────────────────────────────────────────────

/// Run trim with the given config and return (before_tokens, after_tokens, applied).
fn run_trim(body: &Value, provider: Option<ProviderKind>, preset: &str) -> (u64, u64, bool) {
    let input = serde_json::to_string(body).unwrap_or_default();

    if input.len() < 4096 {
        return (0, 0, false);
    }

    match llmtrim_core::rewrite_request(&input, provider, Some(preset)) {
        Ok(res) if res.input_tokens_after.0 < res.input_tokens_before.0 => (
            res.input_tokens_before.0 as u64,
            res.input_tokens_after.0 as u64,
            true,
        ),
        _ => (0, 0, false),
    }
}

#[test]
fn baseline_agent_harness() {
    let session = generate_session(10);

    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixtures_dir = manifest_dir.join("fixtures").join("session-01");
    std::fs::create_dir_all(&fixtures_dir).expect("create fixtures dir");

    println!("\n{}", "=".repeat(90));
    println!("TRIM BASELINE HARNESS — agent preset");
    println!("{}", "=".repeat(90));
    println!(
        "{:<6} {:<25} {:>10} {:>10} {:>10} {:>12} {}",
        "Turn", "Description", "Before(tok)", "After(tok)", "Saved(tok)", "Saved(%)", "Applied"
    );
    println!("{}", "-".repeat(90));

    let mut total_before: u64 = 0;
    let mut total_after: u64 = 0;
    let mut applied_count: usize = 0;

    for fix in &session {
        let path = fixtures_dir.join(format!("turn-{:02}.json", fix.turn));
        let json_str = serde_json::to_string_pretty(&fix.body).expect("serialize");
        std::fs::write(&path, &json_str).expect("write fixture");

        let (before, after, applied) = run_trim(&fix.body, Some(ProviderKind::Anthropic), "agent");

        if applied {
            let saved = before.saturating_sub(after);
            let pct = if before > 0 {
                saved as f64 / before as f64 * 100.0
            } else {
                0.0
            };
            println!(
                "{:<6} {:<25} {:>10} {:>10} {:>10} {:>11.1}% ✅",
                fix.turn, fix.description, before, after, saved, pct
            );
            total_before += before;
            total_after += after;
            applied_count += 1;
        } else {
            println!(
                "{:<6} {:<25} {:>10} {:>10} {:>10} {:>12} ❌",
                fix.turn, fix.description, 0, 0, 0, "N/A"
            );
        }
    }

    println!("{}", "-".repeat(90));

    if applied_count > 0 && total_before > 0 {
        let overall_pct = (total_before - total_after) as f64 / total_before as f64 * 100.0;
        println!(
            "{:<6} {:<25} {:>10} {:>10} {:>10} {:>11.1}% ({} of {} fixtures applied)",
            "∑",
            "aggregate",
            total_before,
            total_after,
            total_before - total_after,
            overall_pct,
            applied_count,
            session.len()
        );
        println!("📊 Mean savings: {:.1}%", overall_pct);
    } else {
        println!("⚠️  No fixtures were compressed (all below threshold or failed)");
    }

    assert!(
        applied_count >= 5,
        "Expected at least 5 fixtures to be compressed, got {}",
        applied_count
    );

    let disk_size: u64 = std::fs::read_dir(&fixtures_dir)
        .expect("read fixtures dir")
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum();
    println!(
        "📁 Fixtures written to: {} ({} bytes total)",
        fixtures_dir.display(),
        disk_size
    );
}

#[test]
fn baseline_auto_harness() {
    let mut total_before: u64 = 0;
    let mut total_after: u64 = 0;
    let mut applied_count: usize = 0;

    println!("\n{}", "=".repeat(90));
    println!("TRIM BASELINE HARNESS — auto preset (OpenAI shape)");
    println!("{}", "=".repeat(90));

    let fixtures = generate_openai_fixtures(8);

    println!(
        "{:<6} {:<25} {:>10} {:>10} {:>10} {:>12} {}",
        "#", "Description", "Before(tok)", "After(tok)", "Saved(tok)", "Saved(%)", "Applied"
    );
    println!("{}", "-".repeat(90));

    for (i, body) in fixtures.iter().enumerate() {
        let (before, after, applied) = run_trim(body, Some(ProviderKind::OpenAi), "auto");

        if applied {
            let saved = before.saturating_sub(after);
            let pct = if before > 0 {
                saved as f64 / before as f64 * 100.0
            } else {
                0.0
            };
            println!(
                "{:<6} {:<25} {:>10} {:>10} {:>10} {:>11.1}% ✅",
                i + 1,
                "openai fixture",
                before,
                after,
                saved,
                pct
            );
            total_before += before;
            total_after += after;
            applied_count += 1;
        } else {
            println!(
                "{:<6} {:<25} {:>10} {:>10} {:>10} {:>12} ❌",
                i + 1,
                "openai fixture",
                0,
                0,
                0,
                "N/A"
            );
        }
    }

    if applied_count > 0 && total_before > 0 {
        let overall_pct = (total_before - total_after) as f64 / total_before as f64 * 100.0;
        println!("{}", "-".repeat(90));
        println!(
            "📊 Aggregate auto savings: {:.1}% ({} of {} applied)",
            overall_pct,
            applied_count,
            fixtures.len()
        );
    }
}

// ── Phase 2: Single-tuning benchmarks ──────────────────────────────────────

/// Run trim with an explicit DenseConfig. Returns (before_tokens, after_tokens, applied).
fn run_trim_with_config(
    body: &Value,
    provider: Option<ProviderKind>,
    config: &DenseConfig,
) -> (u64, u64, bool) {
    let input = serde_json::to_string(body).unwrap_or_default();
    if input.len() < 4096 {
        return (0, 0, false);
    }
    match compress_with_config(&input, provider, config) {
        Ok(res) if res.input_tokens_after.0 < res.input_tokens_before.0 => (
            res.input_tokens_before.0 as u64,
            res.input_tokens_after.0 as u64,
            true,
        ),
        _ => (0, 0, false),
    }
}

/// Apply the baseline agent preset to all fixtures and return per-turn results.
fn baseline_agent_results(session: &[Fixture]) -> Vec<(usize, u64, u64, bool)> {
    let cfg = DenseConfig::preset("agent").expect("agent preset exists");
    session
        .iter()
        .map(|fix| {
            let (b, a, ok) = run_trim_with_config(&fix.body, Some(ProviderKind::Anthropic), &cfg);
            (fix.turn, b, a, ok)
        })
        .collect()
}

/// Apply a custom config to all fixtures, returning per-turn results and saving
/// the delta table to `findings-phase2-<label>.md`.
fn phase2_bench(
    session: &[Fixture],
    label: &str,
    description: &str,
    baseline: &[(usize, u64, u64, bool)],
    custom_config: &DenseConfig,
) {
    let baseline_total_before: u64 = baseline.iter().map(|(_, b, _, _)| b).sum();
    let baseline_total_after: u64 = baseline.iter().map(|(_, _, a, _)| a).sum();
    let baseline_pct = if baseline_total_before > 0 {
        (baseline_total_before - baseline_total_after) as f64 / baseline_total_before as f64 * 100.0
    } else {
        0.0
    };

    println!("\n{}", "=".repeat(100));
    println!("PHASE 2 — {}: {}", label, description);
    println!("{}", "=".repeat(100));
    println!(
        "{:<6} {:<20} {:>10} {:>10} {:>10} {:>8} {:>10} {}",
        "Turn", "Config", "Before", "After", "Saved%", "Δpp", "Baseline%", "Applied"
    );
    println!("{}", "-".repeat(100));

    let mut total_before: u64 = 0;
    let mut total_after: u64 = 0;
    let mut applied_count: usize = 0;

    for (i, fix) in session.iter().enumerate() {
        let (bl_base, bl_after, bl_ok) = (baseline[i].1, baseline[i].2, baseline[i].3);
        let (before, after, ok) =
            run_trim_with_config(&fix.body, Some(ProviderKind::Anthropic), custom_config);

        if ok && bl_ok {
            let saved = before.saturating_sub(after);
            let _pct = before as f64 / before as f64 * 100.0;
            let saved_pct = if before > 0 {
                saved as f64 / before as f64 * 100.0
            } else {
                0.0
            };
            let bl_saved_pct = if bl_base > 0 {
                (bl_base - bl_after) as f64 / bl_base as f64 * 100.0
            } else {
                0.0
            };
            let delta = saved_pct - bl_saved_pct;
            println!(
                "{:<6} {:<20} {:>10} {:>10} {:>7.1}% {:>+7.1} {:>9.1}% ✅",
                fix.turn, label, before, after, saved_pct, delta, bl_saved_pct,
            );
            total_before += before;
            total_after += after;
            applied_count += 1;
        } else {
            println!(
                "{:<6} {:<20} {:>10} {:>10} {:>10} {:>10} {:>10} {}",
                fix.turn, label, 0, 0, "N/A", "N/A", "N/A", "❌"
            );
        }
    }

    if applied_count > 0 && total_before > 0 {
        let overall_pct = (total_before - total_after) as f64 / total_before as f64 * 100.0;
        let delta = overall_pct - baseline_pct;
        println!("{}", "-".repeat(100));
        println!(
            "📊 Aggregate {label}: {:.1}% (Δ {:.1}pp vs baseline {:.1}%) — {}/{} applied",
            overall_pct,
            delta,
            baseline_pct,
            applied_count,
            session.len()
        );
        println!(
            "   Δ input tokens: {} fewer than baseline ({:.1}%)",
            baseline_total_before
                .abs_diff(total_before)
                .max(baseline_total_before.abs_diff(total_before)),
            if baseline_total_before > total_before {
                (baseline_total_before - total_before) as f64 / baseline_total_before as f64
                    * 100.0
            } else {
                -((total_before - baseline_total_before) as f64 / baseline_total_before as f64
                    * 100.0)
            }
        );
    }
}

// ── Phase 2 — tuning 1: normalize_unicode ──────────────────────────────────

/// Phase 2.4: normalize_unicode: true — the safest tuning.
/// Claimed +1-2%, realistically <<0.5% on ASCII-heavy content.
/// Purpose: calibrate estimate inflation before touching riskier knobs.
#[test]
fn phase2_normalize_unicode() {
    let session = generate_session(10);
    let baseline = baseline_agent_results(&session);

    let mut config = DenseConfig::preset("agent").expect("agent preset exists");
    config.normalize_unicode = true;

    phase2_bench(
        &session,
        "norm_unicode",
        "normalize_unicode: true (safest tuning — calibrates estimate inflation)",
        &baseline,
        &config,
    );

    println!("\n📋 Findings to save in findings-phase2-normalize_unicode.md");
}

// ── Phase 2 — tuning 2: toolout_max_lines ─────────────────────────────────

/// Phase 2.1: toolout_max_lines: 40 → 25 — the highest claimed single gain.
/// Directly trades against the recovery path (needs turn-count scrutiny).
#[test]
fn phase2_toolout_max_lines() {
    let session = generate_session(10);
    let baseline = baseline_agent_results(&session);

    let mut config = DenseConfig::preset("agent").expect("agent preset exists");
    config.toolout_max_lines = 25;

    phase2_bench(
        &session,
        "toolout_25",
        "toolout_max_lines: 40 → 25 (highest claimed gain — moderate risk)",
        &baseline,
        &config,
    );

    println!("\n📋 Findings to save in findings-phase2-toolout_max_lines.md");
}

/// Phase 2.1 semantic damage check: compare baseline vs tuned output on each fixture.
#[test]
fn phase2_toolout_max_lines_semantic() {
    let session = generate_session(10);
    let base_cfg = DenseConfig::preset("agent").expect("agent preset exists");
    let mut tuned_cfg = DenseConfig::preset("agent").expect("agent preset exists");
    tuned_cfg.toolout_max_lines = 25;

    println!("\n=== Semantic damage check: toolout_max_lines 40→25 ===");
    for fix in &session {
        let input = serde_json::to_string(&fix.body).unwrap_or_default();
        if input.len() < 4096 {
            continue;
        }
        let base_res =
            compress_with_config(&input, Some(ProviderKind::Anthropic), &base_cfg).unwrap();
        let tuned_res =
            compress_with_config(&input, Some(ProviderKind::Anthropic), &tuned_cfg).unwrap();
        let base_len = base_res.request_json.len();
        let tuned_len = tuned_res.request_json.len();

        // Check tool_result blocks for truncation
        let base_val: Value = serde_json::from_str(&base_res.request_json).unwrap();
        let tuned_val: Value = serde_json::from_str(&tuned_res.request_json).unwrap();

        let mut tool_result_truncations = 0;
        if let Some(base_msgs) = base_val["messages"].as_array() {
            if let Some(tuned_msgs) = tuned_val["messages"].as_array() {
                for i in 0..base_msgs.len().min(tuned_msgs.len()) {
                    if base_msgs[i]["role"] == "user" {
                        let base_content = base_msgs[i]["content"].as_array();
                        let tuned_content = tuned_msgs[i]["content"].as_array();
                        if let (Some(bc), Some(tc)) = (base_content, tuned_content) {
                            for j in 0..bc.len().min(tc.len()) {
                                if bc[j]["type"] == "tool_result" {
                                    let b_lines = bc[j]["content"]
                                        .as_str()
                                        .map(|s| s.lines().count())
                                        .unwrap_or(0);
                                    let t_lines = tc[j]["content"]
                                        .as_str()
                                        .map(|s| s.lines().count())
                                        .unwrap_or(0);
                                    if b_lines > t_lines {
                                        tool_result_truncations += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        println!(
            "Turn {:2} | output {}B→{}B (Δ {}B) | tool_result truncations: {}",
            fix.turn,
            base_len,
            tuned_len,
            base_len as i64 - tuned_len as i64,
            tool_result_truncations
        );
    }
    println!("=== End semantic check ===");
}

// ── Phase 2 — tuning 3: tool_max_desc_chars ──────────────────────────────

/// Phase 2.3: tool_max_desc_chars: 300 → 150 — only meaningful if caching is real.
/// Phase 0 confirmed trim does not bust prefix cache, so this is safe.
#[test]
fn phase2_tool_max_desc_chars() {
    let session = generate_session(10);
    let baseline = baseline_agent_results(&session);

    let mut config = DenseConfig::preset("agent").expect("agent preset exists");
    config.tool_max_desc_chars = 150;

    phase2_bench(
        &session,
        "desc_150",
        "tool_max_desc_chars: 300 → 150 (cache-safe description truncation)",
        &baseline,
        &config,
    );

    println!("\n📋 Findings to save in findings-phase2-tool_max_desc_chars.md");
}

/// Phase 2.3 semantic damage check: compare tool description lengths.
#[test]
fn phase2_tool_max_desc_chars_semantic() {
    let session = generate_session(10);
    let base_cfg = DenseConfig::preset("agent").expect("agent preset exists");
    let mut tuned_cfg = DenseConfig::preset("agent").expect("agent preset exists");
    tuned_cfg.tool_max_desc_chars = 150;

    println!("\n=== Semantic damage check: tool_max_desc_chars 300→150 ===");
    for fix in &session {
        let input = serde_json::to_string(&fix.body).unwrap_or_default();
        if input.len() < 4096 {
            continue;
        }

        let base_res =
            compress_with_config(&input, Some(ProviderKind::Anthropic), &base_cfg).unwrap();
        let tuned_res =
            compress_with_config(&input, Some(ProviderKind::Anthropic), &tuned_cfg).unwrap();

        let base_val: Value = serde_json::from_str(&base_res.request_json).unwrap();
        let tuned_val: Value = serde_json::from_str(&tuned_res.request_json).unwrap();

        let base_tools = base_val["tools"].as_array();
        let tuned_tools = tuned_val["tools"].as_array();

        let mut truncated = 0;
        let mut unchanged = 0;
        if let (Some(bt), Some(tt)) = (base_tools, tuned_tools) {
            for i in 0..bt.len().min(tt.len()) {
                let b_desc = bt[i]["description"].as_str().unwrap_or("").len();
                let t_desc = tt[i]["description"].as_str().unwrap_or("").len();
                if b_desc > t_desc && t_desc > 0 {
                    truncated += 1;
                } else if b_desc == t_desc {
                    unchanged += 1;
                }
            }
        }

        let output_shorter = tuned_res.request_json.len() < base_res.request_json.len();
        println!(
            "Turn {:2} | tools {}/{} truncated, {} unchanged | output {}B→{}B (Δ {}B) | shorter: {}",
            fix.turn,
            truncated,
            base_tools.map(|t| t.len()).unwrap_or(0),
            unchanged,
            base_res.request_json.len(),
            tuned_res.request_json.len(),
            base_res.request_json.len() as i64 - tuned_res.request_json.len() as i64,
            output_shorter,
        );
    }
    println!("=== End semantic check ===");
}

// ── Phase 2 — tuning 4: dedup_near ────────────────────────────────────────

/// Phase 2.2: dedup_near: true — the most semantically risky tuning.
/// Collapses near-duplicate lines that differ by line number / identifier.
/// Must be checked for false positives.
#[test]
fn phase2_dedup_near() {
    let session = generate_session(10);
    let baseline = baseline_agent_results(&session);

    let mut config = DenseConfig::preset("agent").expect("agent preset exists");
    config.dedup_near = true;

    phase2_bench(
        &session,
        "dedup_near",
        "dedup_near: true (most risky — collapses near-dup lines)",
        &baseline,
        &config,
    );

    println!("\n📋 Findings to save in findings-phase2-dedup_near.md");
}

/// Phase 2.2 semantic damage check: inspect what lines were collapsed.
/// Reports false-positive candidates where semantically distinct lines were merged.
#[test]
fn phase2_dedup_near_semantic() {
    let session = generate_session(10);
    let base_cfg = DenseConfig::preset("agent").expect("agent preset exists");
    let mut tuned_cfg = DenseConfig::preset("agent").expect("agent preset exists");
    tuned_cfg.dedup_near = true;

    println!("\n=== Semantic damage check: dedup_near true ===");

    let mut total_saved_bytes: i64 = 0;
    let mut total_turns_with_changes = 0;

    for fix in &session {
        let input = serde_json::to_string(&fix.body).unwrap_or_default();
        if input.len() < 4096 {
            continue;
        }

        let base_res =
            compress_with_config(&input, Some(ProviderKind::Anthropic), &base_cfg).unwrap();
        let tuned_res =
            compress_with_config(&input, Some(ProviderKind::Anthropic), &tuned_cfg).unwrap();

        let base_val: Value = serde_json::from_str(&base_res.request_json).unwrap();
        let tuned_val: Value = serde_json::from_str(&tuned_res.request_json).unwrap();

        let mut truncations = 0;
        if let Some(base_msgs) = base_val["messages"].as_array() {
            if let Some(tuned_msgs) = tuned_val["messages"].as_array() {
                for i in 0..base_msgs.len().min(tuned_msgs.len()) {
                    if base_msgs[i]["role"] == "user" {
                        let base_content = base_msgs[i]["content"].as_array();
                        let tuned_content = tuned_msgs[i]["content"].as_array();
                        if let (Some(bc), Some(tc)) = (base_content, tuned_content) {
                            for j in 0..bc.len().min(tc.len()) {
                                if bc[j]["type"] == "tool_result" {
                                    let b_txt = bc[j]["content"].as_str().unwrap_or("");
                                    let t_txt = tc[j]["content"].as_str().unwrap_or("");
                                    let b_lines: Vec<&str> = b_txt.lines().collect();
                                    let t_lines: Vec<&str> = t_txt.lines().collect();
                                    if b_lines.len() > t_lines.len() {
                                        truncations += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let saved = base_res.request_json.len() as i64 - tuned_res.request_json.len() as i64;
        if saved > 0 {
            total_turns_with_changes += 1;
            total_saved_bytes += saved;
        }

        println!(
            "Turn {:2} | tokens {}→{} | bytes {}→{} (Δ {}B) | tool_result truncations: {}",
            fix.turn,
            base_res.input_tokens_after.0,
            tuned_res.input_tokens_after.0,
            base_res.request_json.len(),
            tuned_res.request_json.len(),
            saved,
            truncations,
        );
    }

    println!("---");
    println!(
        "Turns with changes: {}/{}",
        total_turns_with_changes,
        session.len()
    );
    println!("Total byte reduction: {} B", total_saved_bytes);
    if total_turns_with_changes > 0 {
        println!("\n--- False-positive spot-check (first turn with changes) ---");
        for fix in &session {
            let input = serde_json::to_string(&fix.body).unwrap_or_default();
            if input.len() < 4096 {
                continue;
            }
            let base_res =
                compress_with_config(&input, Some(ProviderKind::Anthropic), &base_cfg).unwrap();
            let tuned_res =
                compress_with_config(&input, Some(ProviderKind::Anthropic), &tuned_cfg).unwrap();

            let saved = base_res.request_json.len() as i64 - tuned_res.request_json.len() as i64;
            if saved <= 0 {
                continue;
            }

            let base_val: Value = serde_json::from_str(&base_res.request_json).unwrap();
            let tuned_val: Value = serde_json::from_str(&tuned_res.request_json).unwrap();

            if let Some(base_msgs) = base_val["messages"].as_array() {
                if let Some(tuned_msgs) = tuned_val["messages"].as_array() {
                    for i in 0..base_msgs.len().min(tuned_msgs.len()) {
                        if base_msgs[i]["role"] != "user" {
                            continue;
                        }
                        let bc = base_msgs[i]["content"].as_array();
                        let tc = tuned_msgs[i]["content"].as_array();
                        if let (Some(bc), Some(tc)) = (bc, tc) {
                            for j in 0..bc.len().min(tc.len()) {
                                if bc[j]["type"] != "tool_result" {
                                    continue;
                                }
                                let b_txt = bc[j]["content"].as_str().unwrap_or("");
                                let t_txt = tc[j]["content"].as_str().unwrap_or("");
                                let b_lines: Vec<&str> = b_txt.lines().collect();
                                let t_lines: Vec<&str> = t_txt.lines().collect();
                                if b_lines.len() != t_lines.len() {
                                    println!(
                                        "\ntool_result [{}][{}]: {} baseline lines → {} tuned lines",
                                        i,
                                        j,
                                        b_lines.len(),
                                        t_lines.len()
                                    );
                                    for k in t_lines.len()..b_lines.len().min(t_lines.len() + 3) {
                                        println!(
                                            "  First removed line: {}",
                                            b_lines
                                                .get(k)
                                                .map(|s| {
                                                    let s = s.trim();
                                                    if s.len() > 100 { &s[..100] } else { s }
                                                })
                                                .unwrap_or("")
                                        );
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            break; // only first turn with changes
        }
    }
    println!("=== End semantic check ===");
}

// ── Phase 3 — combined config ─────────────────────────────────────────────

/// Phase 3.0: ALL three accepted tunings stacked: normalize_unicode:true,
/// toolout_max_lines:25, tool_max_desc_chars:150.
/// Reports the REAL combined gain (which will be less than sum of parts).
/// This is a measurement-only test — no production code is changed.
#[test]
fn phase3_combined() {
    let session = generate_session(10);
    let baseline = baseline_agent_results(&session);

    let mut config = DenseConfig::preset("agent").expect("agent preset exists");
    config.normalize_unicode = true;
    config.toolout_max_lines = 25;
    config.tool_max_desc_chars = 150;

    phase2_bench(
        &session,
        "combined",
        "normalize_unicode:true + toolout_max_lines:25 + tool_max_desc_chars:150",
        &baseline,
        &config,
    );

    println!("\n📋 Findings to save in findings-phase3-combined.md");
    println!("🛑 CHECKPOINT 3 — STOPPED. Awaiting human go/no-go for production apply.");
}

/// Generate OpenAI-shaped request fixtures. Each fixture includes a fenced
/// code excerpt with a prose review prompt, multi-turn assistant replies, and
/// a structured-data config block -- matching the kind of content real Zed /
/// Copilot chat completions carry. Target size ensures every fixture clears
/// the `TRIM_MIN_BYTES` (4096) threshold.
fn generate_openai_fixtures(count: usize) -> Vec<Value> {
    let mut fixtures = Vec::with_capacity(count);

    let system_prompt = "You are an experienced Rust and systems software engineer. \
                         Review code critically and provide specific, actionable feedback \
                         grounded in the actual code rather than generic advice.";

    for i in 0..count {
        let mut messages: Vec<Value> = vec![
            serde_json::json!({"role": "system", "content": system_prompt}),
            serde_json::json!({"role": "user", "content": build_openai_user_content(i)}),
        ];

        // Multi-turn: each fixture from 2+ has an assistant review + user follow-up
        let topics = [
            "the retry loop",
            "lock acquisition",
            "streaming path",
            "classification match",
            "shutdown sequence",
            "connection pooling",
            "config reload",
            "metrics emission",
        ];
        if i >= 2 {
            let reply = build_openai_assistant_reply(i);
            let follow_up = format!(
                "Thanks. Could you show the corrected version of {} and explain \
                 why the change is safe under {}?",
                topics[(i * 3) % topics.len()],
                [
                    "concurrent load",
                    "a 429 storm",
                    "client disconnects",
                    "a slow upstream"
                ][i % 4],
            );
            messages.push(serde_json::json!({"role": "assistant", "content": reply}));
            messages.push(serde_json::json!({"role": "user", "content": follow_up}));
        }

        fixtures.push(serde_json::json!({
            "model": "gpt-4o",
            "messages": messages,
            "max_tokens": 4096,
            "temperature": 0.7,
        }));
    }
    fixtures
}

/// Build a prose + code user message for an OpenAI fixture. Content is diverse
/// per index so cross-fixture dedup is minimal.
fn build_openai_user_content(index: usize) -> String {
    let tasks = [
        "Review this routing layer: are there any correctness bugs or performance \
         problems that would matter under load, or is it just messy but fine?",
        "We're seeing intermittent latency spikes in this async handler in prod. \
         Walk through the code's behaviour under contention and identify the most \
         likely stall source.",
        "This config-loading module grew organically. Give an honest line-level \
         review: what's fragile, what's actually buggy, and what's ugly but fine?",
        "Here is a streaming proxy path that occasionally leaks upstream \
         connections. Trace the lifecycle of a request through this code and \
         identify where a disconnected client could leave a connection dangling.",
        "This tier-classification code needs edits every time a model ships. \
         Suggest a structure that keeps the hot path fast but makes adding a new \
         tier a one-line change.",
        "Our retry logic was written in a hurry. Critique the backoff strategy \
         and tell me what a hostile upstream (slow headers, 429 storms, partial \
         responses) would do to it.",
        "This module mixes business logic with serialization. Help me see the \
         seams where I could split it, and flag any place where the current \
         coupling hides an actual bug.",
        "I inherited this connection-pool snapshot code. Review it for race \
         conditions and whether the metrics it emits are even meaningful.",
    ];

    let mut content = String::with_capacity(8000);
    content.push_str(tasks[index % tasks.len()]);
    content.push_str("\n\n");
    // Add a unique contextual explanation for each fixture so the content is
    // less repetitive across fixtures (reduces cross-request dedup).
    content.push_str(match index % 8 {
        0 => {
            "I'm particularly concerned about how the semaphore interacts with the \
              caller's timeout. The acquire() method wraps the semaphore wait in a \
              tokio::time::timeout, but the drop handler for PermitGuard releases \
              the permit asynchronously. Under high concurrency this could lead to \
              permit starvation because the release is delayed past the point where \
              the next waiter starts its timeout clock.\n\n"
        }
        1 => {
            "One thing that jumps out is the error handling around the config parser. \
              The unwrap() on from_str means any malformed config file will crash the \
              entire proxy, not just the reload task. For a production service this \
              should at minimum log the error and keep the last known good config in \
              place. The current behaviour makes the proxy unavailable during config \
              rollouts if even one field is misspelled.\n\n"
        }
        2 => {
            "The lock ordering is the main structural concern. The classify function \
              acquires the tier override map read lock, then the route resolve function \
              acquires the connection pool lock. If the connection-pool eviction path \
              ever needs to check the tier config (e.g., to decide which pool to drain), \
              we have a classic A-B / B-A deadlock. The fix is to document a consistent \
              lock ordering and enforce it at compile time with a lock hierarchy.\n\n"
        }
        3 => {
            "The parse_upstream_usage function does a lot of nested Option chaining \
              without logging which field was missing. When cached_tokens is zero (cache \
              miss), we can't tell whether the upstream doesn't support caching or \
              just didn't have a hit. Adding a debug-level log per missing field would \
              make debugging cache-efficacy issues much faster.\n\n"
        }
        4 => {
            "Looking at the build_cache_reason format string, the percentage is rounded \
              to 0 decimal places, which means small cache hits (under 0.5%) show as \
              0%. For observability that's misleading: the reader can't distinguish \
              'no cache support' from 'cold start with a tiny hit'. I'd fix by showing \
              one decimal place when the ratio is under 1%.\n\n"
        }
        5 => {
            "The spawn function pattern is unusual: it takes a PathBuf by value but \
              then clones it inside watch_loop. If the file path never changes, the \
              clone is wasted. An Arc<PathBuf> would let all references share the \
              same allocation. The same applies to the ConfigWatcher struct itself, \
              which stores a PathBuf but never mutates it.\n\n"
        }
        6 => {
            "The watch_loop function has an unconditional loop with 30-second sleep. \
              If the config file is deleted, it keeps reading an empty buffer and \
              panicking on from_str. A better design would be: read the file, if it \
              doesn't exist skip the update but keep the current config, and log a \
              warning. Only escalate after N consecutive failures.\n\n"
        }
        _ => {
            "The SemaphorePool's max_wait timeout should be configurable per-tier. \
              Currently it's a single Duration, but Opus requests may justify a longer \
              wait than Fable. I'd make the timeout a parameter of acquire() so the \
              caller can decide based on the request priority rather than using a \
              single value for all traffic.\n\n"
        }
    });
    content.push_str("Here's the code in question:\n\n```rust\n");

    let code_lines: &[&str] = &[
        "pub struct SemaphorePool {\n    permits: Arc<tokio::sync::Semaphore>,\n    max_wait: Duration,\n    active: Arc<AtomicU64>,\n    rejected: Arc<AtomicU64>,\n}\n",
        "impl SemaphorePool {\n    pub async fn acquire(&self) -> Result<PermitGuard, PoolError> {\n        match tokio::time::timeout(self.max_wait, self.permits.acquire()).await {\n            Ok(Ok(p)) => { self.active.fetch_add(1, Ordering::Release); Ok(PermitGuard(p, self.active.clone())) }\n            Ok(Err(_)) => Err(PoolError::Closed),\n            Err(_) => { self.rejected.fetch_add(1, Ordering::Release); Err(PoolError::Timeout) }\n        }\n    }\n}\n",
        "pub struct ConfigWatcher {\n    path: PathBuf,\n    current: Arc<RwLock<Option<AppConfig>>>,\n    notifier: tokio::sync::watch::Sender<Arc<AppConfig>>,\n}\n",
        "impl ConfigWatcher {\n    pub fn spawn(path: PathBuf) -> (Self, tokio::sync::watch::Receiver<Arc<AppConfig>>) {\n        let (tx, rx) = tokio::sync::watch::channel(Arc::new(AppConfig::default()));\n        let current = Arc::new(RwLock::new(None));\n        let watcher = Self { path, current, notifier: tx.clone() };\n        tokio::spawn(watch_loop(path, current, tx));\n        (watcher, rx)\n    }\n}\n",
        "async fn watch_loop(path: PathBuf, current: Arc<RwLock<Option<AppConfig>>>, tx: watch::Sender<Arc<AppConfig>>) {\n    let mut buffer = String::new();\n    loop {\n        buffer.clear();\n        let mut reader = tokio::fs::File::open(&path).await.unwrap();\n        reader.read_to_string(&mut buffer).await.unwrap();\n        let parsed: AppConfig = toml::from_str(&buffer).unwrap();\n        *current.write().await = Some(parsed.clone());\n        tx.send(Arc::new(parsed)).ok();\n        tokio::time::sleep(Duration::from_secs(30)).await;\n    }\n}\n",
        "pub fn classify(headers: &HeaderMap) -> &'static str {\n    let auth = headers.get(\"authorization\").and_then(|v| v.to_str().ok());\n    let hint = headers.get(\"x-routing-hint\").and_then(|v| v.to_str().ok());\n    match (auth, hint) {\n        (Some(_), Some(\"opus\")) => \"opus\",\n        (Some(_), Some(\"sonnet\")) => \"sonnet\",\n        (Some(_), Some(\"haiku\")) => \"haiku\",\n        (Some(_), None) => \"fallback\",\n        (None, _) => \"unauthenticated\",\n        _ => \"unknown\",\n    }\n}\n",
        "fn parse_upstream_usage(body: &Value) -> (u64, u64, u64) {\n    let usage = body.get(\"usage\")?;\n    let input = usage.get(\"prompt_tokens\").and_then(|v| v.as_u64()).unwrap_or(0);\n    let output = usage.get(\"completion_tokens\").and_then(|v| v.as_u64()).unwrap_or(0);\n    let cached = usage.get(\"prompt_tokens_details\").and_then(|d| d.get(\"cached_tokens\")).and_then(|v| v.as_u64()).unwrap_or(0);\n    (input, output, cached)\n}\n",
        "pub fn build_cache_reason(cached: u64, input: u64) -> String {\n    let ratio = if input > 0 { cached as f64 / input as f64 * 100.0 } else { 0.0 };\n    format!(\"cache: {:.0}% ({} / {})\", ratio, cached, input)\n}\n",
        "pub async fn timeout_handler(req: Request, timeout: Duration) -> Result<Response> {\n    tokio::select! {\n        resp = forward_request(req) => resp,\n        _ = tokio::time::sleep(timeout) => {\n            metrics.timeouts.inc();\n            Err(ProxyError::Timeout)\n        }\n    }\n}\n",
    ];

    let start = (index * 3) % code_lines.len();
    let line_count = 40 + (index * 5);
    for j in 0..line_count {
        let idx = (start + j) % code_lines.len();
        content.push_str(code_lines[idx]);
    }

    content.push_str("```\n\n");
    // Per-fixture diverse follow-up prose to reduce cross-request dedup
    content.push_str(match index % 4 {
        0 => "For reference, here are the current routing defaults from our config:\n",
        1 => "The upstream routing table at startup looks like this:\n",
        2 => "These are the relevant configuration entries for the tier mapping:\n",
        _ => "The relevant section of settings.json for routing:\n",
    });
    content.push_str(&format!(
        "- routing table: {:?}\n- timeout: {}ms\n- retries: {}\n- pool size: {}\n",
        vec![
            ("opus", "sva-opencode/glm-5.2", 0.9, 16),
            ("sonnet", "sva-opencode/deepseek-v4-flash", 0.7, 32),
        ],
        30000,
        4,
        64 + index * 8,
    ));
    content
}

/// Build a unique assistant review reply for each fixture index.
fn build_openai_assistant_reply(index: usize) -> String {
    let mut s = String::with_capacity(3000);
    match index % 5 {
        0 => s.push_str("I've traced through the code path carefully. The main concerns are: \
            the lock scope is wider than needed (the `send()` call inside the critical section \
            blocks all concurrent readers), the error variant `PoolError::Timeout` doesn't \
            propagate the upstream status which would help debugging, and the `unwrap()` on \
            line 42 will panic if the file is truncated mid-write. A safer approach would be \
            to restructure the lock to cover only the hash lookup."),
        1 => s.push_str("Looking at this from a systems perspective, the retry logic is \
            vulnerable to thundering-herd amplification: all concurrent requests retry on the \
            same cadence, which can cascade. I'd add jitter (`base * 2^attempt * random(0.5,1.5)`) \
            and a circuit-breaker that stops sending after N consecutive 503s within a window. \
            The current fixed backoff is also too aggressive for rate-limit (429) responses."),
        2 => s.push_str("The coupling between deserialization and routing is tighter than it \
            looks. The `classify` function reads headers and dispatches in one shot, but the \
            connection-pool key is computed from a separate field path. I'd split the \
            classification into a two-phase: first extract a `RouteKey` (headers + model), \
            then dispatch using a match on `RouteKey`. That makes both sides independently testable."),
        3 => s.push_str("The race condition is subtle but real. The `active` counter uses \
            `Release` ordering but the subsequent assertion reads it with `Relaxed`, so a \
            thread on another core could observe a stale decrement. Fix: use `AcqRel` on both \
            sides or an atomic increment that returns the new value. The connection-close path \
            also drops the permit guard without logging, which explains why we see 'lost' \
            permits under load."),
        _ => s.push_str("The config watcher has a TOCTOU problem: `read_to_string` reads the \
            file, then `from_str` parses it, but between those two calls another writer could \
            truncate or replace the file. The `unwrap()` on the parse step would then panic. \
            Fix: mmap the file or use `serde_path` to atomically read. Also, the 30-second \
            polling interval is fragile -- consider `inotify`/`ReadDirectoryChanges` for \
            immediate reload rather than busy-polling."),
    };
    s.push_str(
        "\n\nThe specific hot spot to address first is on line 85: the fallthrough \
        case in the match doesn't log which route was selected, making it impossible \
        to diagnose misrouting after the fact. Adding a tracing event there would have \
        caught the current bug in minutes.",
    );
    s
}
