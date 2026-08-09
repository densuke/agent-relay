//! MCP（Model Context Protocol）の JSON-RPC を解釈し、bus の操作へ振り分ける。
//!
//! # 対応範囲
//!
//! Streamable HTTP transport（仕様 2025-06-18）のうち、**単一 JSON レスポンスで完結する
//! 部分だけ**を実装する。SSE ストリームは開かない。
//!
//! 仕様上 SSE は必須ではなく、「POST の body が request なら `application/json` を返す」
//! ことが許されている。本サーバは poll 型（受信側が取りに来る）なので、サーバから
//! 一方的に流す必要がなく、SSE を持たない判断をした。GET には 405 を返す。
//!
//! # 実装するメソッド
//!
//! - `initialize` — ハンドシェイク
//! - `notifications/initialized` — 通知（202 を返すだけ）
//! - `tools/list` — ツール一覧
//! - `tools/call` — ツール実行

use crate::bus::Bus;
use serde_json::{Value, json};

/// 本サーバが話すプロトコル版。
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// JSON-RPC のエラーコード（仕様準拠）。
mod code {
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
}

/// 1件の JSON-RPC 入力を処理した結果。
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// レスポンスを返す（HTTP 200 + JSON）
    Reply(String),
    /// 返すものが無い（HTTP 202 Accepted・body 無し）。notification / response が該当
    Accepted,
}

/// JSON-RPC メッセージを1件処理する。
pub fn handle(bus: &mut Bus, raw: &str) -> Outcome {
    let Ok(req): Result<Value, _> = serde_json::from_str(raw) else {
        return Outcome::Reply(error_response(
            Value::Null,
            code::INVALID_REQUEST,
            "JSON として解釈できません",
        ));
    };

    let method = req.get("method").and_then(Value::as_str);
    let id = req.get("id").cloned();

    // id を持たないものは notification。仕様上 202 を返して body は返さない
    let Some(id) = id else {
        return Outcome::Accepted;
    };

    let Some(method) = method else {
        // id はあるが method が無い = レスポンスとみなす。こちらから要求は出さないので受け流す
        return Outcome::Accepted;
    };

    let params = req.get("params").cloned().unwrap_or(json!({}));

    match method {
        "initialize" => Outcome::Reply(result_response(id, initialize_result())),
        "tools/list" => Outcome::Reply(result_response(id, tools_list())),
        "tools/call" => match call_tool(bus, &params) {
            Ok(v) => Outcome::Reply(result_response(id, v)),
            Err(msg) => Outcome::Reply(error_response(id, code::INVALID_PARAMS, &msg)),
        },
        // ping は疎通確認に使われる。空の結果を返すのが仕様
        "ping" => Outcome::Reply(result_response(id, json!({}))),
        _ => Outcome::Reply(error_response(id, code::METHOD_NOT_FOUND, method)),
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "agent-relay", "version": env!("CARGO_PKG_VERSION") }
    })
}

fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "register",
                "description": "このセッションの名前をリレーへ登録する。最初に一度だけ呼ぶ。",
                "inputSchema": {
                    "type": "object",
                    "properties": { "name": { "type": "string", "description": "自分の名前（例: honten / shiten）" } },
                    "required": ["name"]
                }
            },
            {
                "name": "send_message",
                "description": "指定した相手へメッセージを送る。相手が未登録でも送れる（後から受け取れる）。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "from": { "type": "string", "description": "自分の名前" },
                        "to": { "type": "string", "description": "宛先の名前" },
                        "body": { "type": "string", "description": "本文" }
                    },
                    "required": ["from", "to", "body"]
                }
            },
            {
                "name": "poll_messages",
                "description": "自分宛の未読を取り出す。取り出したものは消える。未読が無ければ空。",
                "inputSchema": {
                    "type": "object",
                    "properties": { "name": { "type": "string", "description": "自分の名前" } },
                    "required": ["name"]
                }
            },
            {
                "name": "list_peers",
                "description": "登録済みのピア一覧を返す。",
                "inputSchema": { "type": "object", "properties": {} }
            }
        ]
    })
}

fn call_tool(bus: &mut Bus, params: &Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or("tools/call に name がありません")?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let text = match name {
        "register" => {
            let who = str_arg(&args, "name")?;
            let is_new = bus.register(&who);
            if is_new {
                format!("{who} を登録しました")
            } else {
                format!("{who} は登録済みです")
            }
        }
        "send_message" => {
            let from = str_arg(&args, "from")?;
            let to = str_arg(&args, "to")?;
            let body = str_arg(&args, "body")?;
            let msg = bus.send(&from, &to, &body);
            format!("送信しました (id={})", msg.id)
        }
        "poll_messages" => {
            let who = str_arg(&args, "name")?;
            let msgs = bus.poll(&who);
            if msgs.is_empty() {
                "未読はありません".to_string()
            } else {
                serde_json::to_string_pretty(&msgs).map_err(|e| e.to_string())?
            }
        }
        "list_peers" => {
            let peers = bus.peers();
            if peers.is_empty() {
                "登録されているピアはいません".to_string()
            } else {
                peers.join(", ")
            }
        }
        other => return Err(format!("未知のツール: {other}")),
    };

    // MCP のツール結果は content 配列で返す
    Ok(json!({ "content": [ { "type": "text", "text": text } ] }))
}

fn str_arg(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("引数 {key} がありません"))
}

fn result_response(id: Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error_response(id: Value, code: i32, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(bus: &mut Bus, raw: &str) -> Value {
        match handle(bus, raw) {
            Outcome::Reply(s) => serde_json::from_str(&s).expect("JSON として返るはず"),
            Outcome::Accepted => panic!("Reply を期待したが Accepted だった"),
        }
    }

    fn call(bus: &mut Bus, tool: &str, args: Value) -> String {
        let raw = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": tool, "arguments": args }
        })
        .to_string();
        let v = reply(bus, &raw);
        v["result"]["content"][0]["text"]
            .as_str()
            .expect("text が返るはず")
            .to_string()
    }

    #[test]
    fn initializeでプロトコル版とサーバ情報を返す() {
        let mut bus = Bus::new();
        let v = reply(
            &mut bus,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
        );
        assert_eq!(v["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(v["result"]["serverInfo"]["name"], "agent-relay");
        // tools を持つことを宣言する
        assert!(v["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn notificationには何も返さない() {
        // 仕様: id を持たない入力は 202 Accepted・body 無し
        let mut bus = Bus::new();
        let out = handle(
            &mut bus,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        );
        assert_eq!(out, Outcome::Accepted);
    }

    #[test]
    fn tools_listで4つのツールを返す() {
        let mut bus = Bus::new();
        let v = reply(
            &mut bus,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        );
        let tools = v["result"]["tools"].as_array().expect("配列");
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            ["register", "send_message", "poll_messages", "list_peers"]
        );
        // 各ツールが inputSchema を持つこと（クライアントが引数を組み立てられる）
        for t in tools {
            assert!(
                t["inputSchema"].is_object(),
                "{} に inputSchema が無い",
                t["name"]
            );
        }
    }

    #[test]
    fn registerからpollまで一連の流れが通る() {
        let mut bus = Bus::new();
        assert!(call(&mut bus, "register", json!({"name":"honten"})).contains("登録しました"));
        assert!(call(&mut bus, "register", json!({"name":"shiten"})).contains("登録しました"));

        let sent = call(
            &mut bus,
            "send_message",
            json!({"from":"honten","to":"shiten","body":"やあ"}),
        );
        assert!(sent.contains("送信しました"));

        let got = call(&mut bus, "poll_messages", json!({"name":"shiten"}));
        assert!(got.contains("やあ"), "本文が返るはず: {got}");
        assert!(got.contains("honten"), "送信者が分かるはず: {got}");
    }

    #[test]
    fn 未読が無いときは分かる文言を返す() {
        let mut bus = Bus::new();
        let got = call(&mut bus, "poll_messages", json!({"name":"honten"}));
        assert_eq!(got, "未読はありません");
    }

    #[test]
    fn list_peersで登録済みを列挙する() {
        let mut bus = Bus::new();
        assert_eq!(
            call(&mut bus, "list_peers", json!({})),
            "登録されているピアはいません"
        );
        call(&mut bus, "register", json!({"name":"honten"}));
        call(&mut bus, "register", json!({"name":"shiten"}));
        assert_eq!(call(&mut bus, "list_peers", json!({})), "honten, shiten");
    }

    #[test]
    fn 未知のメソッドはmethod_not_foundを返す() {
        let mut bus = Bus::new();
        let v = reply(&mut bus, r#"{"jsonrpc":"2.0","id":1,"method":"no/such"}"#);
        assert_eq!(v["error"]["code"], code::METHOD_NOT_FOUND);
    }

    #[test]
    fn 引数不足はinvalid_paramsを返す() {
        // body を落とした send_message
        let mut bus = Bus::new();
        let raw = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params": { "name":"send_message", "arguments": {"from":"a","to":"b"} }
        })
        .to_string();
        let v = reply(&mut bus, &raw);
        assert_eq!(v["error"]["code"], code::INVALID_PARAMS);
        assert!(
            v["error"]["message"].as_str().unwrap().contains("body"),
            "どの引数が足りないか分かること: {}",
            v["error"]["message"]
        );
    }

    #[test]
    fn 壊れたjsonはinvalid_requestを返す() {
        let mut bus = Bus::new();
        let v = reply(&mut bus, "{ this is not json");
        assert_eq!(v["error"]["code"], code::INVALID_REQUEST);
    }

    #[test]
    fn pingに空の結果を返す() {
        // 疎通確認。仕様上 result は空オブジェクト
        let mut bus = Bus::new();
        let v = reply(&mut bus, r#"{"jsonrpc":"2.0","id":9,"method":"ping"}"#);
        assert_eq!(v["result"], json!({}));
        assert_eq!(v["id"], 9);
    }

    #[test]
    fn 応答のidは要求のidと一致する() {
        let mut bus = Bus::new();
        let v = reply(
            &mut bus,
            r#"{"jsonrpc":"2.0","id":"abc","method":"tools/list"}"#,
        );
        assert_eq!(v["id"], "abc", "文字列の id もそのまま返す");
        assert_eq!(v["jsonrpc"], "2.0");
    }
}
