#![allow(unused_crate_dependencies)]

use serde_json::{Value, json};

use std::{
    collections::BTreeMap,
    fs,
    io::{self, BufRead, BufReader, Write},
    net::TcpListener,
    path::PathBuf,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|argument| argument == "--version") {
        println!("solar-lsp-bench-fake 1");
        return Ok(());
    }
    if std::env::var_os("LSP_BENCH_EXPECT_TOOLCHAIN").is_some() {
        let solc = std::env::var_os("SOLC_PATH").ok_or("SOLC_PATH is missing")?;
        let solc = std::path::PathBuf::from(solc);
        if !solc.is_file()
            || !matches!(solc.file_name().and_then(|name| name.to_str()), Some("solc" | "solc.exe"))
        {
            return Err(format!("invalid pinned solc alias `{}`", solc.display()).into());
        }
        if std::env::var("LSP_BENCH_OFFLINE").as_deref() != Ok("1")
            || std::env::var("CARGO_NET_OFFLINE").as_deref() != Ok("true")
            || std::env::var("npm_config_offline").as_deref() != Ok("true")
        {
            return Err("offline environment is incomplete".into());
        }
        let first_path = std::env::split_paths(&std::env::var_os("PATH").ok_or("PATH is missing")?)
            .next()
            .ok_or("PATH is empty")?;
        if solc.parent() != Some(first_path.as_path()) {
            return Err("pinned tool directory is not first in PATH".into());
        }
    }
    let behavior = std::env::var("LSP_BENCH_FAKE_BEHAVIOR").unwrap_or_default();

    let cache_marker = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .ok_or("XDG_CACHE_HOME is missing")?
        .join("fake-lsp-index");
    let cache_reused = cache_marker.is_file();
    fs::write(&cache_marker, "ready")?;

    let args = std::env::args().collect::<Vec<_>>();
    let (mut reader, mut writer): (Box<dyn BufRead>, Box<dyn Write>) =
        if let Some(index) = args.iter().position(|argument| argument == "--tcp") {
            let address = args.get(index + 1).ok_or("--tcp requires an address")?;
            let listener = TcpListener::bind(address)?;
            let (stream, _) = listener.accept()?;
            (Box::new(BufReader::new(stream.try_clone()?)), Box::new(stream))
        } else {
            (Box::new(BufReader::new(io::stdin())), Box::new(io::stdout()))
        };
    let mut indexing = false;
    let mut documents = BTreeMap::<String, String>::new();
    while let Some(message) = read_message(&mut reader)? {
        let method = message.get("method").and_then(Value::as_str);
        match method {
            Some("initialize") => {
                write_message(
                    &mut writer,
                    &json!({"jsonrpc":"2.0","id":"progress-create","method":"window/workDoneProgress/create","params":{"token":"index"}}),
                )?;
                write_message(
                    &mut writer,
                    &json!({"jsonrpc":"2.0","id":"config","method":"workspace/configuration","params":{"items":[{"section":"solidity"}]}}),
                )?;
                write_message(&mut writer, &json!({"jsonrpc":"2.0","id":999,"result":"early"}))?;
                write_message(
                    &mut writer,
                    &json!({
                        "jsonrpc":"2.0",
                        "id":message["id"],
                        "result":{"capabilities":{
                            "positionEncoding":"utf-8",
                            "textDocumentSync":{"openClose":true,"change":1,"save":{"includeText":true}},
                            "hoverProvider":true,
                            "completionProvider":false,
                            "documentSymbolProvider":true,
                            "renameProvider":true
                        }}
                    }),
                )?;
            }
            Some("initialized") => {
                write_message(
                    &mut writer,
                    &json!({"jsonrpc":"2.0","id":"register","method":"client/registerCapability","params":{"registrations":[{"id":"completion","method":"textDocument/completion","registerOptions":{"triggerCharacters":["."]}}]}}),
                )?;
            }
            Some("textDocument/didOpen") => {
                let uri = message["params"]["textDocument"]["uri"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let document = message["params"]["textDocument"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                documents.insert(uri, document);
                indexing = true;
                write_message(
                    &mut writer,
                    &json!({"jsonrpc":"2.0","method":"$/progress","params":{"token":"index","value":{"kind":"begin","title":"index"}}}),
                )?;
                write_message(
                    &mut writer,
                    &json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":message["params"]["textDocument"]["uri"],"version":1,"diagnostics":[]}}),
                )?;
            }
            Some("textDocument/didChange") => {
                let uri = message["params"]["textDocument"]["uri"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let document = message["params"]["contentChanges"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                documents.insert(uri, document);
            }
            Some("textDocument/didSave") => {
                if let (Some(uri), Some(document)) = (
                    message["params"]["textDocument"]["uri"].as_str(),
                    message["params"]["text"].as_str(),
                ) {
                    documents.insert(uri.to_owned(), document.to_owned());
                }
            }
            Some("textDocument/hover") => {
                if behavior == "timeout-hover" {
                    continue;
                }
                write_message(
                    &mut writer,
                    &json!({"jsonrpc":"2.0","id":"apply","method":"workspace/applyEdit","params":{"label":"fake edit","edit":{"changes":{}}}}),
                )?;
                write_message(
                    &mut writer,
                    &json!({
                        "jsonrpc":"2.0",
                        "id":message["id"],
                        "result":{"contents":{"kind":"markdown","value":
                            if behavior == "incorrect-hover" {
                                "function wrong(uint256)"
                            } else if cache_reused {
                                "function add(uint256) cache-reused"
                            } else {
                                "function add(uint256)"
                            }
                        }}
                    }),
                )?;
                if indexing {
                    std::thread::sleep(std::time::Duration::from_millis(75));
                    write_message(
                        &mut writer,
                        &json!({"jsonrpc":"2.0","method":"$/progress","params":{"token":"index","value":{"kind":"end"}}}),
                    )?;
                    indexing = false;
                }
            }
            Some("textDocument/completion") => {
                let label = if message["params"]["context"]["triggerKind"] == 2
                    && message["params"]["context"]["triggerCharacter"] == "."
                {
                    "add"
                } else {
                    "wrong"
                };
                write_message(
                    &mut writer,
                    &json!({"jsonrpc":"2.0","id":message["id"],"result":{"isIncomplete":false,"items":[{"label":label,"kind":3}]}}),
                )?;
            }
            Some("textDocument/rename") => {
                let uri = message["params"]["textDocument"]["uri"]
                    .as_str()
                    .ok_or("rename URI is missing")?;
                let document = documents.get(uri).ok_or("rename document is not open")?;
                let character = message["params"]["position"]["character"]
                    .as_u64()
                    .ok_or("rename position is missing")? as usize;
                let (start, end) = identifier_range(document, character)
                    .ok_or("rename position does not identify a symbol")?;
                let new_name =
                    message["params"]["newName"].as_str().ok_or("rename name is missing")?;
                let mut changes = serde_json::Map::new();
                changes.insert(
                    uri.to_owned(),
                    json!([{
                        "range": {
                            "start": {"line": 0, "character": start},
                            "end": {"line": 0, "character": end}
                        },
                        "newText": new_name
                    }]),
                );
                write_message(
                    &mut writer,
                    &json!({"jsonrpc":"2.0","id":message["id"],"result":{"changes":changes}}),
                )?;
            }
            Some("workspace/didRenameFiles") => {
                for file in message["params"]["files"].as_array().into_iter().flatten() {
                    if let (Some(old_uri), Some(new_uri)) =
                        (file["oldUri"].as_str(), file["newUri"].as_str())
                        && let Some(document) = documents.remove(old_uri)
                    {
                        documents.insert(new_uri.to_owned(), document);
                    }
                }
            }
            Some("workspace/didDeleteFiles") => {
                for file in message["params"]["files"].as_array().into_iter().flatten() {
                    if let Some(uri) = file["uri"].as_str() {
                        documents.remove(uri);
                    }
                }
            }
            Some("textDocument/documentSymbol") => {
                let uri = message["params"]["textDocument"]["uri"].as_str().unwrap_or_default();
                let name = documents
                    .get(uri)
                    .and_then(|document| contract_name(document))
                    .unwrap_or("Unknown");
                write_message(
                    &mut writer,
                    &json!({"jsonrpc":"2.0","id":message["id"],"result":[{"name":name,"kind":5,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"selectionRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}]}),
                )?;
                if indexing {
                    std::thread::sleep(std::time::Duration::from_millis(75));
                    write_message(
                        &mut writer,
                        &json!({"jsonrpc":"2.0","method":"$/progress","params":{"token":"index","value":{"kind":"end"}}}),
                    )?;
                    indexing = false;
                }
            }
            Some("shutdown") => {
                if behavior == "strict-shutdown" && message.get("params").is_some() {
                    write_message(
                        &mut writer,
                        &json!({"jsonrpc":"2.0","id":message["id"],"error":{"code":-32602,"message":"shutdown does not accept params"}}),
                    )?;
                } else {
                    write_message(
                        &mut writer,
                        &json!({"jsonrpc":"2.0","id":message["id"],"result":null}),
                    )?;
                }
            }
            Some("exit") => break,
            _ => {}
        }
    }
    Ok(())
}

fn identifier_range(document: &str, character: usize) -> Option<(usize, usize)> {
    if character >= document.len() {
        return None;
    }
    let bytes = document.as_bytes();
    let mut start = character;
    while start > 0 && is_identifier_byte(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = character;
    while end < bytes.len() && is_identifier_byte(bytes[end]) {
        end += 1;
    }
    (start < end).then_some((start, end))
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn contract_name(document: &str) -> Option<&str> {
    document
        .split_once("contract ")?
        .1
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .next()
}

fn write_message(writer: &mut impl Write, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

fn read_message(reader: &mut impl BufRead) -> Result<Option<Value>, Box<dyn std::error::Error>> {
    let mut length = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let mut body = vec![0; length.ok_or("missing Content-Length")?];
    reader.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}
