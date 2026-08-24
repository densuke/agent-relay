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
///
/// `now` は UNIX 秒。ピアの生存判定に使う。呼び出し側から渡すことで、
/// テストが実時計に依存しない。
pub fn handle(bus: &mut Bus, raw: &str, now: u64) -> Outcome {
    let Ok(req): Result<Value, _> = serde_json::from_str(raw) else {
        return Outcome::Reply(error_response(
            Value::Null,
            code::INVALID_REQUEST,
            "JSON として解釈できません",
        ));
    };

    // 失効したピアを先に落とす。これをしないと list_peers が実体のないピアを
    // 返し、send_message の到達性チェックも幽霊を「生存」と誤判定する。
    bus.sweep(now);

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
        "tools/call" => match call_tool(bus, &params, now) {
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
                "description": "このセッションの名前を登録し、以降の操作に使うトークンを受け取る。最初に一度だけ呼ぶ。返ってきた token は他のツールで必要になるので覚えておくこと。サーバが合言葉を要求する設定なら secret も渡す。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "自分の名前（例: honten / shiten）" },
                        "secret": { "type": "string", "description": "参加の合言葉（サーバが要求する場合のみ）" }
                    },
                    "required": ["name"]
                }
            },
            {
                "name": "send_message",
                "description": "指定した相手へメッセージを送る。送信者は token から判別されるので名乗る必要はない。相手が未登録でも送れる（後から受け取れる）。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "token": { "type": "string", "description": "register で受け取ったトークン" },
                        "to": { "type": "string", "description": "宛先の名前" },
                        "body": { "type": "string", "description": "本文" }
                    },
                    "required": ["token", "to", "body"]
                }
            },
            {
                "name": "poll_messages",
                "description": "自分宛の未読を取り出す。取り出したものは消える。未読が無ければ空。",
                "inputSchema": {
                    "type": "object",
                    "properties": { "token": { "type": "string", "description": "register で受け取ったトークン" } },
                    "required": ["token"]
                }
            },
            {
                "name": "list_peers",
                "description": "登録済みのピア一覧を返す。",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "unregister",
                "description": "登録を解除する。名前が空くので同じ名前で登録し直せる。未読は破棄される。",
                "inputSchema": {
                    "type": "object",
                    "properties": { "token": { "type": "string", "description": "register で受け取ったトークン" } },
                    "required": ["token"]
                }
            }
        ]
    })
}

fn call_tool(bus: &mut Bus, params: &Value, now: u64) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or("tools/call に name がありません")?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let text = match name {
        "register" => {
            let who = str_arg(&args, "name")?;
            // 合言葉を要求する設定のときだけ secret が要る
            let secret = args.get("secret").and_then(Value::as_str);
            let token = bus.register(&who, secret, now).map_err(|e| e.to_string())?;
            // トークンはこの応答でしか渡らない。失うと登録し直しになる旨を添える
            format!(
                "{who} として登録しました。\ntoken: {token}\n（以降の send_message / poll_messages でこのトークンを使ってください）"
            )
        }
        "send_message" => {
            let token = str_arg(&args, "token")?;
            let to = str_arg(&args, "to")?;
            let body = str_arg(&args, "body")?;
            let sent = bus
                .send(&token, &to, &body, now)
                .map_err(|e| e.to_string())?;
            let m = &sent.msg;
            let mut text = format!("{} -> {} へ送信しました (id={})", m.from, m.to, m.id);

            // 未登録の宛先へも送れる仕様（相手が後から登録して受け取れる）だが、
            // そのぶん「宛先を間違えた」ことに送信側が気づけない。実際に、中継網の
            // ピア名ではなく Remote Control の表示名を宛先にした5通が、誰も取りに
            // 来ない受信箱に滞留したまま数日気づかれなかった。
            // 送信は成功のままにし、警告だけ添える。
            if !bus.peers().iter().any(|p| p == &to) {
                let peers = bus.peers();
                let list = if peers.is_empty() {
                    "（現在ゼロ）".to_string()
                } else {
                    peers.join(", ")
                };
                text.push_str(&format!(
                    "\n\n注意: '{to}' はいま中継網に接続していません。宛先の綴りを確認してください。\n\
                     相手が後から接続すれば受け取れますが、名前が違っていた場合は誰にも読まれません。\n\
                     接続中: {list}"
                ));
            }

            if sent.dropped > 0 {
                // 黙って消さない。宛先が受け取りに来ていないことを送信側に知らせる
                text.push_str(&format!(
                    "\n（{} の受信箱が上限に達したため、古い {} 件を破棄しました）",
                    m.to, sent.dropped
                ));
            }
            text
        }
        "poll_messages" => {
            let token = str_arg(&args, "token")?;
            let msgs = bus.poll(&token, now).map_err(|e| e.to_string())?;
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
        "unregister" => {
            let token = str_arg(&args, "token")?;
            let who = bus.unregister(&token).map_err(|e| e.to_string())?;
            format!("{who} の登録を解除しました")
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
        match handle(bus, raw, 0) {
            Outcome::Reply(s) => serde_json::from_str(&s).expect("JSON として返るはず"),
            Outcome::Accepted => panic!("Reply を期待したが Accepted だった"),
        }
    }

    /// ツールを呼び、返ってきた本文（text）を取り出す。
    fn call(bus: &mut Bus, tool: &str, args: Value) -> String {
        let raw = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": tool, "arguments": args }
        })
        .to_string();
        let v = reply(bus, &raw);
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("text が返るはず: {v}"))
            .to_string()
    }

    /// ツールを呼び、JSON-RPC のエラーを取り出す。
    fn call_err(bus: &mut Bus, tool: &str, args: Value) -> Value {
        let raw = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": tool, "arguments": args }
        })
        .to_string();
        reply(bus, &raw)["error"].clone()
    }

    /// 登録してトークン部分だけを抜き出す。
    fn register(bus: &mut Bus, name: &str) -> String {
        let text = call(bus, "register", json!({ "name": name }));
        text.lines()
            .find_map(|l| l.strip_prefix("token: "))
            .unwrap_or_else(|| panic!("token 行があるはず: {text}"))
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
        assert!(v["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn notificationには何も返さない() {
        // 仕様: id を持たない入力は 202 Accepted・body 無し
        let mut bus = Bus::new();
        let out = handle(
            &mut bus,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            0,
        );
        assert_eq!(out, Outcome::Accepted);
    }

    #[test]
    fn tools_listで5つのツールを返す() {
        let mut bus = Bus::new();
        let v = reply(
            &mut bus,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        );
        let tools = v["result"]["tools"].as_array().expect("配列");
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "register",
                "send_message",
                "poll_messages",
                "list_peers",
                "unregister"
            ]
        );
        for t in tools {
            assert!(
                t["inputSchema"].is_object(),
                "{} に inputSchema が無い",
                t["name"]
            );
        }
    }

    #[test]
    fn registerでトークンが返る() {
        let mut bus = Bus::new();
        let text = call(&mut bus, "register", json!({"name":"honten"}));
        assert!(text.contains("honten として登録しました"));
        assert!(text.contains("token: "), "トークンを渡すこと: {text}");
    }

    #[test]
    fn registerからpollまで一連の流れが通る() {
        let mut bus = Bus::new();
        let honten = register(&mut bus, "honten");
        let shiten = register(&mut bus, "shiten");

        let sent = call(
            &mut bus,
            "send_message",
            json!({"token": honten, "to":"shiten", "body":"やあ"}),
        );
        assert!(
            sent.contains("honten -> shiten"),
            "送信者が分かること: {sent}"
        );

        let got = call(&mut bus, "poll_messages", json!({"token": shiten}));
        assert!(got.contains("やあ"), "本文が返るはず: {got}");
        assert!(got.contains("honten"), "送信者が入るはず: {got}");
    }

    #[test]
    fn 送信者を詐称できない() {
        // from を引数で受け取らない設計。トークンの持ち主が送信者になる
        let mut bus = Bus::new();
        let honten = register(&mut bus, "honten");
        let shiten = register(&mut bus, "shiten");

        // 引数に from を紛れ込ませても無視される
        call(
            &mut bus,
            "send_message",
            json!({"token": honten, "to":"shiten", "body":"x", "from":"shacho"}),
        );

        let got = call(&mut bus, "poll_messages", json!({"token": shiten}));
        assert!(got.contains("honten"), "実際の送信者になること: {got}");
        assert!(!got.contains("shacho"), "詐称した名前は入らない: {got}");
    }

    #[test]
    fn 不正なトークンでの送信は拒否される() {
        let mut bus = Bus::new();
        register(&mut bus, "shiten");
        let err = call_err(
            &mut bus,
            "send_message",
            json!({"token":"nisemono", "to":"shiten", "body":"なりすまし"}),
        );
        assert_eq!(err["code"], code::INVALID_PARAMS);
        assert!(
            err["message"].as_str().unwrap().contains("トークン"),
            "理由が分かること: {}",
            err["message"]
        );
    }

    #[test]
    fn 不正なトークンでの受信は拒否される() {
        let mut bus = Bus::new();
        let err = call_err(&mut bus, "poll_messages", json!({"token":"nisemono"}));
        assert_eq!(err["code"], code::INVALID_PARAMS);
    }

    #[test]
    fn 同じ名前の二重登録は拒否される() {
        let mut bus = Bus::new();
        register(&mut bus, "honten");
        let err = call_err(&mut bus, "register", json!({"name":"honten"}));
        assert_eq!(err["code"], code::INVALID_PARAMS);
        assert!(err["message"].as_str().unwrap().contains("既に使われ"));
    }

    #[test]
    fn 接続していない宛先へ送ると警告が付く() {
        // 宛先を間違えても送信は成功するため、送信側が気づけない。
        // 実際に Remote Call の表示名を宛先にした5通が数日滞留した
        let mut bus = Bus::new();
        let honten = register(&mut bus, "honten");
        register(&mut bus, "shiten");

        let out = call(
            &mut bus,
            "send_message",
            json!({ "token": honten, "to": "Agy(支店)", "body": "届かない" }),
        );

        assert!(out.contains("送信しました"), "送信自体は成功する");
        assert!(out.contains("接続していません"), "警告が出る");
        assert!(
            out.contains("shiten"),
            "接続中のピアを示して綴りを直せるようにする"
        );
    }

    #[test]
    fn 接続中の宛先へ送っても警告は出ない() {
        let mut bus = Bus::new();
        let honten = register(&mut bus, "honten");
        register(&mut bus, "shiten");

        let out = call(
            &mut bus,
            "send_message",
            json!({ "token": honten, "to": "shiten", "body": "届く" }),
        );

        assert!(out.contains("送信しました"));
        assert!(!out.contains("接続していません"), "正常時に警告を出さない");
    }

    #[test]
    fn 失効したピアは接続中とみなさない() {
        // sweep をかけずに peers() を見ると幽霊を生存と誤判定する
        let mut bus = Bus::new();
        // shiten は時刻 0 に登録して以降まったく接触しない = 失効する。
        // honten は送信時点で登録するので生きている（実運用でも送信側は
        // poll し続けているため失効しない）
        bus.register("shiten", None, 0).unwrap();
        let honten = bus
            .register("honten", None, crate::bus::PEER_TTL_SECS + 1)
            .unwrap();

        // TTL を跨いだ時刻で送る。shiten は既に失効しているはず
        let raw = serde_json::to_string(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "send_message",
                        "arguments": { "token": honten, "to": "shiten", "body": "留守宛" } }
        }))
        .unwrap();
        let Outcome::Reply(resp) = handle(&mut bus, &raw, crate::bus::PEER_TTL_SECS + 1) else {
            panic!("応答が返るはず");
        };
        let v: Value = serde_json::from_str(&resp).unwrap();
        let text = v["result"]["content"][0]["text"].as_str().unwrap();

        assert!(
            text.contains("接続していません"),
            "失効ピアは接続中扱いしない"
        );
    }

    #[test]
    fn 未読が無いときは分かる文言を返す() {
        let mut bus = Bus::new();
        let honten = register(&mut bus, "honten");
        assert_eq!(
            call(&mut bus, "poll_messages", json!({"token": honten})),
            "未読はありません"
        );
    }

    #[test]
    fn list_peersで登録済みを列挙する() {
        let mut bus = Bus::new();
        assert_eq!(
            call(&mut bus, "list_peers", json!({})),
            "登録されているピアはいません"
        );
        register(&mut bus, "honten");
        register(&mut bus, "shiten");
        assert_eq!(call(&mut bus, "list_peers", json!({})), "honten, shiten");
    }

    #[test]
    fn unregisterで登録を解除できる() {
        let mut bus = Bus::new();
        let honten = register(&mut bus, "honten");
        let text = call(&mut bus, "unregister", json!({"token": honten}));
        assert!(text.contains("honten の登録を解除"));
        assert_eq!(
            call(&mut bus, "list_peers", json!({})),
            "登録されているピアはいません"
        );
        // 解除後のトークンは使えない
        let err = call_err(&mut bus, "poll_messages", json!({"token": honten}));
        assert_eq!(err["code"], code::INVALID_PARAMS);
    }

    #[test]
    fn 未知のメソッドはmethod_not_foundを返す() {
        let mut bus = Bus::new();
        let v = reply(&mut bus, r#"{"jsonrpc":"2.0","id":1,"method":"no/such"}"#);
        assert_eq!(v["error"]["code"], code::METHOD_NOT_FOUND);
    }

    #[test]
    fn 引数不足はinvalid_paramsを返す() {
        let mut bus = Bus::new();
        let honten = register(&mut bus, "honten");
        let err = call_err(&mut bus, "send_message", json!({"token": honten, "to":"b"}));
        assert_eq!(err["code"], code::INVALID_PARAMS);
        assert!(
            err["message"].as_str().unwrap().contains("body"),
            "どの引数が足りないか分かること: {}",
            err["message"]
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
