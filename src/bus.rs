//! ピア登録とメッセージの受け渡しを担う中核。
//!
//! HTTP や MCP の都合をここに持ち込まない。素のデータ構造として扱えるようにして、
//! テストからそのまま検証できる状態を保つ。
//!
//! # 設計方針
//!
//! - **poll 型**にする。Claude Code のようなエージェントはツール実行のタイミングでしか
//!   受信できないため、push しても即座には読まれない。受信は取りに来てもらう
//! - **メッセージは短命**。受け取ったら消える。履歴を貯める用途は持たせない
//! - 宛先が未登録でも**送信は失敗させない**。相手が後から登録して受け取る場合があるため

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// 1通のメッセージ。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Message {
    /// 送信順に振る通し番号。受信側が順序を判断するために使う
    pub id: u64,
    pub from: String,
    pub to: String,
    pub body: String,
}

/// ピアとその受信箱を保持する。
#[derive(Debug, Default)]
pub struct Bus {
    /// 宛先名 -> 未読メッセージ
    inboxes: HashMap<String, Vec<Message>>,
    /// 登録済みピア名
    peers: Vec<String>,
    next_id: AtomicU64,
}

impl Bus {
    pub fn new() -> Self {
        Self::default()
    }

    /// ピアを登録する。同じ名前を重ねて登録しても増やさない（再接続を許すため）。
    ///
    /// 戻り値は「新規登録だったか」。再登録なら `false`。
    pub fn register(&mut self, name: &str) -> bool {
        if self.peers.iter().any(|p| p == name) {
            return false;
        }
        self.peers.push(name.to_string());
        true
    }

    /// 登録済みのピア名を返す（登録順）。
    pub fn peers(&self) -> &[String] {
        &self.peers
    }

    /// メッセージを送る。
    ///
    /// 宛先が未登録でも受信箱を作って積む。相手が後から `register` して
    /// `poll` すれば受け取れる。起動順に依存させないための判断。
    pub fn send(&mut self, from: &str, to: &str, body: &str) -> Message {
        let msg = Message {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            from: from.to_string(),
            to: to.to_string(),
            body: body.to_string(),
        };
        self.inboxes
            .entry(to.to_string())
            .or_default()
            .push(msg.clone());
        msg
    }

    /// 自分宛の未読をすべて取り出す。取り出したものは受信箱から消える。
    ///
    /// 未読が無ければ空を返す。エラーにはしない（定期的に叩かれる想定のため）。
    pub fn poll(&mut self, name: &str) -> Vec<Message> {
        self.inboxes.remove(name).unwrap_or_default()
    }

    /// 受信箱に溜まっている総数。監視用。
    pub fn pending_count(&self) -> usize {
        self.inboxes.values().map(|v| v.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 登録したピアが一覧に出る() {
        let mut bus = Bus::new();
        assert!(bus.register("honten"));
        assert!(bus.register("shiten"));
        assert_eq!(bus.peers(), ["honten", "shiten"]);
    }

    #[test]
    fn 同じ名前の再登録は重複させない() {
        // セッションが再接続したときに名前が増殖しないこと
        let mut bus = Bus::new();
        assert!(bus.register("honten"));
        assert!(!bus.register("honten"), "2回目は新規ではない");
        assert_eq!(bus.peers(), ["honten"]);
    }

    #[test]
    fn 送ったメッセージを宛先が受け取れる() {
        let mut bus = Bus::new();
        bus.register("honten");
        bus.register("shiten");

        bus.send("honten", "shiten", "8/10のメトリクスを見て");

        let got = bus.poll("shiten");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].from, "honten");
        assert_eq!(got[0].to, "shiten");
        assert_eq!(got[0].body, "8/10のメトリクスを見て");
    }

    #[test]
    fn 受け取ったメッセージは消える() {
        // 同じものを二度処理しないための担保
        let mut bus = Bus::new();
        bus.send("honten", "shiten", "一度きり");

        assert_eq!(bus.poll("shiten").len(), 1);
        assert!(bus.poll("shiten").is_empty(), "2回目は空になる");
    }

    #[test]
    fn 宛先違いのメッセージは受け取らない() {
        let mut bus = Bus::new();
        bus.send("honten", "shiten", "支店宛");

        assert!(bus.poll("honten").is_empty(), "自分宛でないものは届かない");
        assert_eq!(bus.poll("shiten").len(), 1);
    }

    #[test]
    fn 未登録の宛先へ送っても失敗しない() {
        // 起動順に依存させない。相手が後から登録して受け取れること
        let mut bus = Bus::new();
        bus.send("honten", "mada-inai", "先に送っておく");

        bus.register("mada-inai");
        let got = bus.poll("mada-inai");
        assert_eq!(got.len(), 1, "後から登録しても受け取れる");
        assert_eq!(got[0].body, "先に送っておく");
    }

    #[test]
    fn 複数のメッセージは送信順に並ぶ() {
        let mut bus = Bus::new();
        bus.send("honten", "shiten", "1通目");
        bus.send("honten", "shiten", "2通目");
        bus.send("codex", "shiten", "3通目");

        let got = bus.poll("shiten");
        let bodies: Vec<&str> = got.iter().map(|m| m.body.as_str()).collect();
        assert_eq!(bodies, ["1通目", "2通目", "3通目"]);
        // id は単調増加する
        assert!(got[0].id < got[1].id && got[1].id < got[2].id);
    }

    #[test]
    fn 未読が無ければ空を返す() {
        // 定期的に叩かれる想定。空はエラーではない
        let mut bus = Bus::new();
        bus.register("honten");
        assert!(bus.poll("honten").is_empty());
    }

    #[test]
    fn 溜まっている総数を数えられる() {
        let mut bus = Bus::new();
        assert_eq!(bus.pending_count(), 0);

        bus.send("a", "b", "1");
        bus.send("a", "c", "2");
        assert_eq!(bus.pending_count(), 2);

        bus.poll("b");
        assert_eq!(bus.pending_count(), 1);
    }
}
