//! 受信箱をディスクへ保存し、再起動をまたいで引き継ぐ。
//!
//! # 何を残し、何を残さないか
//!
//! 残すのは**受信箱だけ**。ピアの登録とトークンは残さない。
//!
//! トークンを復元しても、そのトークンを握っていたプロセスは再起動で消えている
//! ことが多い。shim は `UnknownToken` を受け取ると自分で登録し直すので、
//! 復元する実益がない。むしろ資格情報をディスクに置く分だけ危うくなる。
//!
//! # なぜ必要か
//!
//! 中継サーバーが落ちている間に届いたメッセージが消えると、相手が不在のときに
//! 「投げておいて後で拾ってもらう」使い方が成立しない。ノード間の連絡経路が
//! ssh の代替として機能する場面では、ここが落ちると手段が丸ごと無くなる。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::bus::Message;

/// 受信箱の保存先。
pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 保存済みの受信箱を読み込む。
    ///
    /// ファイルが無い場合は空を返す（初回起動）。壊れていた場合も空を返して
    /// 起動は続ける。中継の停止を避けることを優先する。
    pub fn load(&self) -> HashMap<String, Vec<Message>> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return HashMap::new();
        };
        match serde_json::from_str(&text) {
            Ok(map) => map,
            Err(e) => {
                eprintln!("保存された受信箱を読めませんでした（無視して起動します）: {e}");
                HashMap::new()
            }
        }
    }

    /// 受信箱を書き出す。
    ///
    /// 同じディレクトリに一時ファイルを作ってから rename する。書き込み途中で
    /// 落ちても、読み手が壊れた JSON を掴むことがない。
    pub fn save(&self, inboxes: &HashMap<String, Vec<Message>>) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string(inboxes)?;
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用の一時ディレクトリ。プロセスIDとテスト名で衝突を避ける。
    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("agent-relay-test-{}-{name}", std::process::id()));
        p.push("inboxes.json");
        p
    }

    fn msg(id: u64, to: &str, body: &str) -> Message {
        Message {
            id,
            from: "honten".into(),
            to: to.into(),
            body: body.into(),
        }
    }

    #[test]
    fn 保存して読み戻せる() {
        let path = temp_path("roundtrip");
        let store = Store::new(&path);
        let mut inboxes = HashMap::new();
        inboxes.insert("shiten".to_string(), vec![msg(0, "shiten", "留守中の伝言")]);

        store.save(&inboxes).unwrap();
        let loaded = store.load();

        assert_eq!(loaded, inboxes);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn 保存先が無ければ空を返す() {
        // 初回起動。エラーにせず素通しする
        let store = Store::new(temp_path("missing"));
        assert!(store.load().is_empty());
    }

    #[test]
    fn 壊れたファイルでも起動を止めない() {
        let path = temp_path("broken");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ これは JSON ではない").unwrap();

        let store = Store::new(&path);
        assert!(store.load().is_empty(), "読めなくても空で続行する");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn 一時ファイルを残さない() {
        // rename で置き換えるので、成功後に .tmp が残っていてはいけない
        let path = temp_path("no-tmp");
        let store = Store::new(&path);
        store.save(&HashMap::new()).unwrap();

        assert!(!path.with_extension("tmp").exists());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn 上書きしても前の内容が混ざらない() {
        let path = temp_path("overwrite");
        let store = Store::new(&path);

        let mut first = HashMap::new();
        first.insert(
            "a".to_string(),
            vec![msg(0, "a", "1通目"), msg(1, "a", "2通目")],
        );
        store.save(&first).unwrap();

        let mut second = HashMap::new();
        second.insert("b".to_string(), vec![msg(2, "b", "別の宛先")]);
        store.save(&second).unwrap();

        assert_eq!(store.load(), second);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
