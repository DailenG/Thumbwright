//! `--mcp`: a Model Context Protocol server over stdio (newline-delimited JSON-RPC 2.0).
//!
//! Every tool is a CLI command: a call builds an argument vector, parses it with the same
//! parser as the command line, and runs it through the same `run`, so the tools cannot
//! drift from the CLI. Tools that need a dialog, the clipboard, the network or system
//! settings (upload, wallpaper, lock screen, pick color, settings, register) are not
//! exposed. `view` returns a decoded image as a PNG content block.

use std::io::{BufRead, Read, Write};

use serde_json::{Map, Value, json};

use crate::ops::{self, Result};

const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
/// Longest request line accepted (a base64 payload never travels this way; paths do).
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
/// Longest edge of an image returned by `view`.
const VIEW_MAX_EDGE: u32 = 4096;
const VIEW_DEFAULT_EDGE: u32 = 1024;

#[derive(Clone, Copy)]
enum Kind {
    /// One positional argument.
    Arg,
    /// A list of positional arguments.
    Args,
    /// `--name value`.
    Opt,
    /// `--name` when true.
    Flag,
}

struct Param {
    name: &'static str,
    kind: Kind,
    required: bool,
    doc: &'static str,
}

const fn p(name: &'static str, kind: Kind, required: bool, doc: &'static str) -> Param {
    Param {
        name,
        kind,
        required,
        doc,
    }
}

struct Tool {
    name: &'static str,
    doc: &'static str,
    command: &'static str,
    /// Options always passed (e.g. `--json`).
    fixed: &'static [&'static str],
    params: &'static [Param],
}

const FILES: &str = "Absolute paths of the input files";

const TOOLS: &[Tool] = &[
    Tool {
        name: "thumbnail",
        doc: "Render a PNG thumbnail of any supported file.",
        command: "thumbnail",
        fixed: &[],
        params: &[
            p("input", Kind::Arg, true, "Input file"),
            p(
                "output",
                Kind::Arg,
                true,
                "Output .png path (must differ from the input)",
            ),
            p(
                "size",
                Kind::Opt,
                false,
                "Longest edge in pixels (default 256)",
            ),
        ],
    },
    Tool {
        name: "convert",
        doc: "Convert one image to the format of the output extension.",
        command: "convert",
        fixed: &[],
        params: &[
            p("input", Kind::Arg, true, "Input file"),
            p(
                "output",
                Kind::Arg,
                true,
                "Output path; its extension picks the format",
            ),
            p("quality", Kind::Opt, false, "JPEG quality 1-100"),
            p("resize", Kind::Opt, false, "WxH (fit inside) or N%"),
        ],
    },
    Tool {
        name: "convert_to",
        doc: "Convert files to a new format next to each source (<stem>.<ext>).",
        command: "convert-to",
        fixed: &[],
        params: &[
            p(
                "ext",
                Kind::Arg,
                true,
                "Target extension, e.g. png, jpg, webp, bmp, gif, tiff, ico",
            ),
            p("files", Kind::Args, true, FILES),
            p("quality", Kind::Opt, false, "JPEG quality 1-100"),
        ],
    },
    Tool {
        name: "resize",
        doc: "Resize images; writes '<stem> (resized).<ext>' next to each source.",
        command: "resize",
        fixed: &[],
        params: &[
            p(
                "to",
                Kind::Opt,
                true,
                "WxH (fit inside, never enlarges) or N%",
            ),
            p("files", Kind::Args, true, FILES),
        ],
    },
    Tool {
        name: "rotate",
        doc: "Rotate or flip images; writes '<stem> (edited).<ext>'.",
        command: "rotate",
        fixed: &[],
        params: &[
            p("by", Kind::Opt, true, "right | left | 180 | fliph | flipv"),
            p("files", Kind::Args, true, FILES),
        ],
    },
    Tool {
        name: "shrink_for_email",
        doc: "Shrink images to a long edge; writes '<stem> (email).jpg'.",
        command: "email",
        fixed: &[],
        params: &[
            p(
                "edge",
                Kind::Opt,
                true,
                "Long edge in pixels, e.g. 640, 1024, 1600",
            ),
            p("files", Kind::Args, true, FILES),
        ],
    },
    Tool {
        name: "compress",
        doc: "Compress images to a JPEG under a byte target; writes '<stem> (compressed).jpg'.",
        command: "compress",
        fixed: &[],
        params: &[
            p(
                "max_size",
                Kind::Opt,
                true,
                "Target such as 1MB, 500KB or a byte count",
            ),
            p("files", Kind::Args, true, FILES),
        ],
    },
    Tool {
        name: "strip_metadata",
        doc: "Remove EXIF/GPS metadata in place, losslessly.",
        command: "strip",
        fixed: &[],
        params: &[p("files", Kind::Args, true, FILES)],
    },
    Tool {
        name: "info",
        doc: "Dimensions, bit depth, format and size of an image (JSON).",
        command: "info",
        fixed: &["--json"],
        params: &[p("input", Kind::Arg, true, "Input file")],
    },
    Tool {
        name: "formats",
        doc: "List every supported file type (JSON).",
        command: "formats",
        fixed: &["--json"],
        params: &[],
    },
    Tool {
        name: "pdf",
        doc: "Combine images into a PDF (one page each). Returns the JSON result.",
        command: "pdf",
        fixed: &["--json"],
        params: &[
            p("output", Kind::Arg, true, "Output .pdf path"),
            p("inputs", Kind::Args, true, "Input images in page order"),
            p(
                "strict",
                Kind::Flag,
                false,
                "Fail instead of omitting undecodable inputs",
            ),
            p(
                "searchable",
                Kind::Flag,
                false,
                "Add an invisible OCR text layer",
            ),
        ],
    },
    Tool {
        name: "cbz",
        doc: "Combine images into a CBZ comic archive. Returns the JSON result.",
        command: "cbz",
        fixed: &["--json"],
        params: &[
            p("output", Kind::Arg, true, "Output .cbz path"),
            p("inputs", Kind::Args, true, "Input images in page order"),
            p(
                "strict",
                Kind::Flag,
                false,
                "Fail instead of omitting undecodable inputs",
            ),
        ],
    },
    Tool {
        name: "ocr",
        doc: "Recognise text in an image with the Windows OCR engine.",
        command: "ocr",
        fixed: &[],
        params: &[p("input", Kind::Arg, true, "Input image")],
    },
    Tool {
        name: "batch",
        doc: "Run thumbnail, convert or info over files/folders; returns the JSON report. Pass retry_from to rerun only the failures of an earlier report.",
        command: "batch",
        fixed: &["--json"],
        params: &[
            p(
                "operation",
                Kind::Arg,
                false,
                "thumbnail | convert | info (omit with retry_from)",
            ),
            p("inputs", Kind::Args, false, "Files or folders"),
            p("recurse", Kind::Flag, false, "Descend into subfolders"),
            p("out", Kind::Opt, false, "Output folder"),
            p("size", Kind::Opt, false, "Thumbnail size"),
            p("to", Kind::Opt, false, "Target extension for convert"),
            p("quality", Kind::Opt, false, "JPEG quality"),
            p("resize", Kind::Opt, false, "WxH or N%"),
            p(
                "retry_from",
                Kind::Opt,
                false,
                "Path of a previous report.json",
            ),
        ],
    },
    Tool {
        name: "prebuild",
        doc: "Warm Explorer's thumbnail cache for files or a folder.",
        command: "prebuild",
        fixed: &[],
        params: &[
            p("paths", Kind::Args, true, "Folders or files"),
            p("recurse", Kind::Flag, false, "Descend into subfolders"),
            p(
                "size",
                Kind::Opt,
                false,
                "Comma-separated sizes (default 96,256,768)",
            ),
            p(
                "rebuild_all",
                Kind::Flag,
                false,
                "Re-extract even when cached",
            ),
            p("jobs", Kind::Opt, false, "Parallel jobs"),
        ],
    },
    Tool {
        name: "rename",
        doc: "Rename files by EXIF date, camera + date, audio tags or a pattern.",
        command: "rename",
        fixed: &[],
        params: &[
            p(
                "by",
                Kind::Opt,
                true,
                "date | camera-date | artist-title | track-title | pattern",
            ),
            p(
                "pattern",
                Kind::Opt,
                false,
                "Pattern for by=pattern, e.g. {date} {name}",
            ),
            p("files", Kind::Args, true, FILES),
        ],
    },
    Tool {
        name: "sort",
        doc: "Move files into folders by image size, EXIF date or an audio tag.",
        command: "sort",
        fixed: &[],
        params: &[
            p("by", Kind::Opt, true, "size | date | tag"),
            p(
                "tag",
                Kind::Opt,
                false,
                "artist | album | genre | year (with by=tag)",
            ),
            p("files", Kind::Args, true, FILES),
        ],
    },
    Tool {
        name: "doctor",
        doc: "Self-check report of the installation, optionally for one file.",
        command: "doctor",
        fixed: &[],
        params: &[p("file", Kind::Arg, false, "A file to diagnose")],
    },
    Tool {
        name: "folder_icon",
        doc: "Use an image as the icon of its containing folder.",
        command: "folder-icon",
        fixed: &[],
        params: &[p("input", Kind::Arg, true, "Input image")],
    },
];

fn schema(tool: &Tool) -> Value {
    let mut props = Map::new();
    let mut required = Vec::new();
    for prm in tool.params {
        let ty = match prm.kind {
            Kind::Arg | Kind::Opt => json!({"type": "string", "description": prm.doc}),
            Kind::Args => {
                json!({"type": "array", "items": {"type": "string"}, "description": prm.doc})
            }
            Kind::Flag => json!({"type": "boolean", "description": prm.doc}),
        };
        props.insert(prm.name.to_owned(), ty);
        if prm.required {
            required.push(prm.name);
        }
    }
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

fn view_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "input": {"type": "string", "description": "Any supported file"},
            "max_edge": {"type": "integer", "description": format!("Longest edge, 16-{VIEW_MAX_EDGE} (default {VIEW_DEFAULT_EDGE})")},
        },
        "required": ["input"],
        "additionalProperties": false,
    })
}

fn tool_list() -> Value {
    let mut tools: Vec<Value> = TOOLS
        .iter()
        .map(|t| json!({"name": t.name, "description": t.doc, "inputSchema": schema(t)}))
        .collect();
    tools.push(json!({
        "name": "view",
        "description": "Decode any supported file and return it as a PNG image.",
        "inputSchema": view_schema(),
    }));
    json!({ "tools": tools })
}

/// The CLI argument vector for a tool call.
fn argv(tool: &Tool, args: &Map<String, Value>) -> Result<Vec<String>> {
    if let Some(unknown) = args
        .keys()
        .find(|k| !tool.params.iter().any(|p| p.name == k.as_str()))
    {
        return Err(format!("unknown argument {unknown:?}"));
    }
    let (mut opts, mut positional) = (Vec::new(), Vec::new());
    for param in tool.params {
        let value = args.get(param.name).filter(|v| !v.is_null());
        let Some(value) = value else {
            if param.required {
                return Err(format!("missing required argument {:?}", param.name));
            }
            continue;
        };
        let text = |v: &Value| match v {
            Value::String(s) => Ok(s.clone()),
            Value::Number(n) => Ok(n.to_string()),
            _ => Err(format!("argument {:?} must be a string", param.name)),
        };
        match param.kind {
            Kind::Arg => positional.push(text(value)?),
            Kind::Args => {
                let list = value.as_array().ok_or_else(|| {
                    format!("argument {:?} must be an array of strings", param.name)
                })?;
                if param.required && list.is_empty() {
                    return Err(format!("argument {:?} must not be empty", param.name));
                }
                for v in list {
                    positional.push(text(v)?);
                }
            }
            Kind::Opt => {
                opts.push(format!("--{}", param.name.replace('_', "-")));
                opts.push(text(value)?);
            }
            Kind::Flag => {
                if value
                    .as_bool()
                    .ok_or_else(|| format!("argument {:?} must be a boolean", param.name))?
                {
                    opts.push(format!("--{}", param.name.replace('_', "-")));
                }
            }
        }
    }
    let mut out = vec![tool.command.to_owned()];
    out.extend(tool.fixed.iter().map(|s| (*s).to_owned()));
    out.extend(opts);
    out.push("--".into());
    out.extend(positional);
    Ok(out)
}

fn view(args: &Map<String, Value>) -> Result<Value> {
    let input = args
        .get("input")
        .and_then(Value::as_str)
        .ok_or("missing required argument \"input\"")?;
    let edge = match args.get("max_edge") {
        None | Some(Value::Null) => VIEW_DEFAULT_EDGE,
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| (16..=VIEW_MAX_EDGE).contains(n))
            .ok_or_else(|| format!("max_edge must be 16-{VIEW_MAX_EDGE}"))?,
    };
    let settings = tw_core::config::Settings::load(&ops::exe_dir());
    let img = ops::decode_full(std::path::Path::new(input), settings.max_file_bytes())?;
    let img = ops::apply_resize(&img, ops::Resize::Fit(edge, edge));
    let png = ops::encode(&img, "png", None)?;
    Ok(json!({
        "content": [
            {"type": "image", "data": crate::base64(&png), "mimeType": "image/png"},
            {"type": "text", "text": format!("{}x{} px", img.width(), img.height())},
        ],
        "isError": false,
    }))
}

fn tool_result(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn call_tool(params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or("tools/call needs a tool name")?;
    let empty = Map::new();
    let args = match params.get("arguments") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(m)) => m,
        Some(_) => return Err("arguments must be an object".into()),
    };
    if name == "view" {
        return Ok(view(args).unwrap_or_else(|e| tool_result(e, true)));
    }
    let tool = TOOLS
        .iter()
        .find(|t| t.name == name)
        .ok_or_else(|| format!("unknown tool {name:?}"))?;
    let outcome = argv(tool, args).and_then(crate::parse).and_then(|cli| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::run(&cli)))
            .unwrap_or_else(|_| Err("the command panicked".into()))
    });
    let partial = crate::take_partial_failure();
    Ok(match outcome {
        Ok(text) => tool_result(text, partial),
        Err(e) => tool_result(e, true),
    })
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Handles one JSON-RPC message; `None` for notifications and responses.
pub fn handle(line: &str) -> Option<Value> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Some(error(&Value::Null, -32700, &format!("parse error: {e}"))),
    };
    let Some(obj) = msg.as_object() else {
        return Some(error(
            &Value::Null,
            -32600,
            "a request must be a JSON object",
        ));
    };
    let id = obj.get("id").cloned();
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        // A response to a request we never send, or garbage: ignore responses, flag the rest.
        return if obj.contains_key("result") || obj.contains_key("error") {
            None
        } else {
            Some(error(
                id.as_ref().unwrap_or(&Value::Null),
                -32600,
                "missing method",
            ))
        };
    };
    let Some(id) = id else {
        return None; // notification (notifications/initialized, notifications/cancelled, ...)
    };
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => {
            let asked = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("");
            let version = SUPPORTED_VERSIONS
                .iter()
                .find(|v| **v == asked)
                .unwrap_or(&SUPPORTED_VERSIONS[0]);
            Ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "twctl", "version": tw_core::ids::VERSION},
                "instructions": "Thumbwright image toolbox: convert, resize, rotate, compress, combine into PDF/CBZ, OCR, batch jobs, and view.",
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tool_list()),
        "tools/call" => call_tool(&params).map_err(|e| (-32602, e)),
        other => Err((-32601, format!("method not found: {other}"))),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, m)) => error(&id, code, &m),
    })
}

/// One newline-terminated line of at most `MAX_LINE_BYTES`; `Ok(None)` at end of input,
/// `Ok(Some(Err(())))` for an oversized line (the rest of which is discarded).
fn read_line(input: &mut impl BufRead) -> std::io::Result<Option<Result<String, ()>>> {
    let mut buf = Vec::new();
    let n = (&mut *input)
        .take(MAX_LINE_BYTES as u64 + 1)
        .read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.ends_with(b"\n") || buf.len() <= MAX_LINE_BYTES {
        return Ok(Some(Ok(String::from_utf8_lossy(&buf).trim().to_owned())));
    }
    loop {
        let mut sink = Vec::new();
        let n = (&mut *input).take(64 * 1024).read_until(b'\n', &mut sink)?;
        if n == 0 || sink.ends_with(b"\n") {
            break;
        }
    }
    Ok(Some(Err(())))
}

/// Serves requests from `input` until it closes.
pub fn serve(mut input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
    while let Some(line) = read_line(&mut input)? {
        let reply = match line {
            Ok(l) if l.is_empty() => continue,
            Ok(l) => handle(&l),
            Err(()) => Some(error(&Value::Null, -32600, "request line too long")),
        };
        if let Some(r) = reply {
            writeln!(output, "{r}")?;
            output.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(line: &str) -> Value {
        handle(line).expect("a response")
    }

    #[test]
    fn initialize_negotiates_version_and_advertises_tools() {
        let r = call(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
        );
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
        assert!(r["result"]["capabilities"]["tools"].is_object());
        assert_eq!(r["result"]["serverInfo"]["name"], "twctl");
        let r = call(
            r#"{"jsonrpc":"2.0","id":"x","method":"initialize","params":{"protocolVersion":"1999"}}"#,
        );
        assert_eq!(r["id"], "x");
        assert_eq!(r["result"]["protocolVersion"], SUPPORTED_VERSIONS[0]);
    }

    #[test]
    fn tools_list_has_schemas_and_view() {
        let r = call(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
        let tools = r["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for want in [
            "convert", "pdf", "cbz", "ocr", "batch", "compress", "view", "doctor",
        ] {
            assert!(names.contains(&want), "{want} missing: {names:?}");
        }
        assert!(!names.contains(&"upload") && !names.contains(&"register"));
        let pdf = tools.iter().find(|t| t["name"] == "pdf").unwrap();
        assert_eq!(pdf["inputSchema"]["required"], json!(["output", "inputs"]));
        assert_eq!(pdf["inputSchema"]["properties"]["inputs"]["type"], "array");
    }

    #[test]
    fn protocol_errors_use_json_rpc_codes_and_notifications_are_silent() {
        assert!(handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
        assert!(handle(r#"{"jsonrpc":"2.0","id":3,"result":{}}"#).is_none());
        assert_eq!(call("{not json")["error"]["code"], -32700);
        assert_eq!(call("[1]")["error"]["code"], -32600);
        assert_eq!(
            call(r#"{"jsonrpc":"2.0","id":4,"method":"nope"}"#)["error"]["code"],
            -32601
        );
        let r = call(r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"nope"}}"#);
        assert_eq!(r["error"]["code"], -32602);
        assert_eq!(
            call(r#"{"jsonrpc":"2.0","id":6,"method":"ping"}"#)["result"],
            json!({})
        );
    }

    #[test]
    fn argument_mapping_validates_and_orders_options_before_files() {
        let tool = TOOLS.iter().find(|t| t.name == "pdf").unwrap();
        let args = json!({"output": "o.pdf", "inputs": ["a.png", "--b.png"], "strict": true, "searchable": false});
        let v = argv(tool, args.as_object().unwrap()).unwrap();
        assert_eq!(
            v,
            [
                "pdf", "--json", "--strict", "--", "o.pdf", "a.png", "--b.png"
            ]
        );
        let bad = json!({"output": "o.pdf"});
        assert!(
            argv(tool, bad.as_object().unwrap())
                .unwrap_err()
                .contains("inputs")
        );
        let extra = json!({"output": "o.pdf", "inputs": ["a"], "bogus": 1});
        assert!(
            argv(tool, extra.as_object().unwrap())
                .unwrap_err()
                .contains("bogus")
        );
        let batch = TOOLS.iter().find(|t| t.name == "batch").unwrap();
        let retry = json!({"retry_from": "r.json"});
        let v = argv(batch, retry.as_object().unwrap()).unwrap();
        assert_eq!(v, ["batch", "--json", "--retry-from", "r.json", "--"]);
    }

    #[test]
    fn serve_answers_a_session_over_a_stream() {
        let input = "\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n\
                     {\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n\
                     {\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n";
        let mut out = Vec::new();
        serve(std::io::Cursor::new(input), &mut out).unwrap();
        let lines: Vec<&str> = std::str::from_utf8(&out).unwrap().lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"protocolVersion\""));
        assert!(lines[1].contains("\"tools\""));
    }

    #[test]
    fn oversized_request_lines_are_rejected_without_ending_the_session() {
        let mut input = vec![b'x'; MAX_LINE_BYTES + 10];
        input.extend_from_slice(b"\n{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"ping\"}\n");
        let mut out = Vec::new();
        serve(std::io::Cursor::new(input), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].contains("too long"));
        assert!(lines[1].contains("\"id\":9"));
    }
}
