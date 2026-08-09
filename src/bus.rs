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
//!
//! # 送信者の同定
//!
//! `register` した時点でサーバがトークンを発行し、以降の操作はトークンで本人を特定する。
//! 送信者名を引数で受け取る方式だと、誰でも他人を騙って送れてしまうため採らない。

use std::collections::HashMap;

/// トークンのバイト長。128bit あれば総当たりは現実的でない。
const TOKEN_BYTES: usize = 16;

/// この秒数だけ音沙汰がないピアは登録を失効させる。
///
/// クライアントは1〜2秒間隔で `poll` するため、生きている限り失効しない。
/// 失効を設けないと、落ちたセッションが名前を占有し続け、同じ名前で
/// 再接続できなくなる（同じホストで何本もセッションが立つ運用では詰まる）。
pub const PEER_TTL_SECS: u64 = 60;

/// 1通のメッセージ。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Message {
    /// 送信順に振る通し番号。受信側が順序を判断するために使う
    pub id: u64,
    pub from: String,
    pub to: String,
    pub body: String,
}

/// 操作が拒否された理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusError {
    /// トークンがどのピアにも紐づかない
    UnknownToken,
    /// その名前は別のピアが使用中
    NameTaken,
    /// 参加の合言葉が違う
    WrongSecret,
}

impl std::fmt::Display for BusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownToken => write!(f, "トークンが不正です。register からやり直してください"),
            Self::NameTaken => write!(f, "その名前は既に使われています"),
            Self::WrongSecret => write!(f, "参加の合言葉が違います"),
        }
    }
}

/// ピアとその受信箱を保持する。
#[derive(Debug, Default)]
pub struct Bus {
    /// 宛先名 -> 未読メッセージ
    inboxes: HashMap<String, Vec<Message>>,
    /// トークン -> ピア名
    tokens: HashMap<String, String>,
    /// 登録順のピア名（一覧の並びを安定させるため）
    peers: Vec<String>,
    /// ピア名 -> 最後に接触した時刻（UNIX 秒）。失効判定に使う
    last_seen: HashMap<String, u64>,
    next_id: u64,
    /// 参加に必要な合言葉。`None` なら誰でも参加できる。
    ///
    /// 公開ホストに置く場合に、無関係な第三者がピアとして入り込むのを防ぐ。
    /// トークン（送信者の同定）とは役割が違い、こちらは入口の可否を決める。
    join_secret: Option<String>,
}

impl Bus {
    /// 合言葉を要求しない Bus を作る。主にテスト用。
    ///
    /// 本体は `with_join_secret` を使う（環境変数の有無で分岐させないため）。
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    /// 参加に合言葉を要求する。`None` または空文字なら誰でも参加できる。
    pub fn with_join_secret(secret: Option<String>) -> Self {
        Self {
            join_secret: secret.filter(|s| !s.is_empty()),
            ..Self::default()
        }
    }

    /// 参加に合言葉が要るか。
    pub fn requires_secret(&self) -> bool {
        self.join_secret.is_some()
    }

    /// ピアを登録し、以降の操作に使うトークンを発行する。
    ///
    /// 合言葉を設定している場合は一致しないと拒否する。同じ名前を**生きている**
    /// ピアが使っていれば拒否する（先に登録した側を守り、後から来た方に別名を
    /// 選ばせる）。失効済みの名前は空いているものとして扱う。
    ///
    /// `now` は UNIX 秒。呼び出し側から渡すことで、テストが時計に依存しない。
    pub fn register(
        &mut self,
        name: &str,
        secret: Option<&str>,
        now: u64,
    ) -> Result<String, BusError> {
        if let Some(expected) = &self.join_secret
            && secret != Some(expected.as_str())
        {
            return Err(BusError::WrongSecret);
        }
        self.sweep(now);
        if self.peers.iter().any(|p| p == name) {
            return Err(BusError::NameTaken);
        }
        let token = generate_token();
        self.tokens.insert(token.clone(), name.to_string());
        self.peers.push(name.to_string());
        self.last_seen.insert(name.to_string(), now);
        Ok(token)
    }

    /// 音沙汰の絶えたピアの登録を解除する。
    ///
    /// 受信箱は**残す**。明示的な `unregister` と違い、失効は「落ちたかもしれない」
    /// という推測にすぎず、復帰して受け取りたい場合があるため。未登録の宛先へ
    /// 送れる設計（下記 `send`）とも一貫する。
    pub fn sweep(&mut self, now: u64) {
        let expired: Vec<String> = self
            .peers
            .iter()
            .filter(|p| {
                self.last_seen
                    .get(*p)
                    .is_none_or(|seen| now.saturating_sub(*seen) >= PEER_TTL_SECS)
            })
            .cloned()
            .collect();
        for name in expired {
            self.tokens.retain(|_, n| n != &name);
            self.peers.retain(|p| p != &name);
            self.last_seen.remove(&name);
        }
    }

    /// このトークンのピアが生きていることを記録する。
    fn touch(&mut self, token: &str, now: u64) {
        if let Some(name) = self.tokens.get(token) {
            let name = name.clone();
            self.last_seen.insert(name, now);
        }
    }

    /// トークンからピア名を引く。
    pub fn whoami(&self, token: &str) -> Result<&str, BusError> {
        self.tokens
            .get(token)
            .map(String::as_str)
            .ok_or(BusError::UnknownToken)
    }

    /// 登録を解除する。名前が空くので、同じ名前で登録し直せる。
    pub fn unregister(&mut self, token: &str) -> Result<String, BusError> {
        let name = self.whoami(token)?.to_string();
        self.tokens.remove(token);
        self.peers.retain(|p| p != &name);
        self.last_seen.remove(&name);
        // 受信箱は残さない。次に同じ名前で登録した別人が読めてしまうため。
        // 失効（sweep）では残すが、こちらは本人が明示的に降りた意思表示なので破棄する
        self.inboxes.remove(&name);
        Ok(name)
    }

    /// 登録済みのピア名を返す（登録順）。
    pub fn peers(&self) -> &[String] {
        &self.peers
    }

    /// メッセージを送る。送信者はトークンから決まるので詐称できない。
    ///
    /// 宛先が未登録でも受信箱を作って積む。相手が後から `register` して
    /// `poll` すれば受け取れる。起動順に依存させないための判断。
    pub fn send(
        &mut self,
        token: &str,
        to: &str,
        body: &str,
        now: u64,
    ) -> Result<Message, BusError> {
        let from = self.whoami(token)?.to_string();
        self.touch(token, now);
        let msg = Message {
            id: self.next_id,
            from,
            to: to.to_string(),
            body: body.to_string(),
        };
        self.next_id += 1;
        self.inboxes
            .entry(to.to_string())
            .or_default()
            .push(msg.clone());
        Ok(msg)
    }

    /// 自分宛の未読をすべて取り出す。取り出したものは受信箱から消える。
    ///
    /// 未読が無ければ空を返す。エラーにはしない（定期的に叩かれる想定のため）。
    pub fn poll(&mut self, token: &str, now: u64) -> Result<Vec<Message>, BusError> {
        let name = self.whoami(token)?.to_string();
        self.touch(token, now);
        Ok(self.inboxes.remove(&name).unwrap_or_default())
    }

    /// 受信箱に溜まっている総数。監視用。
    pub fn pending_count(&self) -> usize {
        self.inboxes.values().map(|v| v.len()).sum()
    }
}

/// OS の乱数から16進のトークンを作る。
///
/// 疑似乱数だと推測されてなりすましを許すため、OS のエントロピーを使う。
fn generate_token() -> String {
    let mut buf = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut buf).expect("OS の乱数を取得できません");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 登録してトークンを得る。テストの見通しを良くするための補助。
    fn reg(bus: &mut Bus, name: &str) -> String {
        bus.register(name, None, 0).expect("登録できるはず")
    }

    #[test]
    fn 登録するとトークンが発行される() {
        let mut bus = Bus::new();
        let token = reg(&mut bus, "honten");
        assert_eq!(token.len(), TOKEN_BYTES * 2, "16進文字列で返る");
        assert_eq!(bus.whoami(&token).unwrap(), "honten");
    }

    #[test]
    fn ピアごとに異なるトークンが出る() {
        // 使い回されると送信者を区別できない
        let mut bus = Bus::new();
        let a = reg(&mut bus, "honten");
        let b = reg(&mut bus, "shiten");
        assert_ne!(a, b);
    }

    #[test]
    fn 同じ名前は二重に登録できない() {
        // 後から来た方が既存のピアを乗っ取れてしまうため拒否する
        let mut bus = Bus::new();
        reg(&mut bus, "honten");
        assert_eq!(bus.register("honten", None, 0), Err(BusError::NameTaken));
    }

    #[test]
    fn 未知のトークンは拒否される() {
        let bus = Bus::new();
        assert_eq!(bus.whoami("deadbeef"), Err(BusError::UnknownToken));
    }

    #[test]
    fn 登録したピアが一覧に出る() {
        let mut bus = Bus::new();
        reg(&mut bus, "honten");
        reg(&mut bus, "shiten");
        assert_eq!(bus.peers(), ["honten", "shiten"]);
    }

    #[test]
    fn 送信者はトークンから決まる() {
        // from を引数で受け取らないので詐称できない
        let mut bus = Bus::new();
        let honten = reg(&mut bus, "honten");
        let shiten = reg(&mut bus, "shiten");

        bus.send(&honten, "shiten", "やあ", 0).unwrap();

        let got = bus.poll(&shiten, 0).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].from, "honten", "送信者はサーバが決める");
        assert_eq!(got[0].body, "やあ");
    }

    #[test]
    fn 不正なトークンでは送信できない() {
        let mut bus = Bus::new();
        reg(&mut bus, "shiten");
        assert_eq!(
            bus.send("nisemono", "shiten", "なりすまし", 0),
            Err(BusError::UnknownToken)
        );
    }

    #[test]
    fn 不正なトークンでは受信できない() {
        // 他人の受信箱を覗けないことの担保
        let mut bus = Bus::new();
        let honten = reg(&mut bus, "honten");
        reg(&mut bus, "shiten");
        bus.send(&honten, "shiten", "支店宛の秘密", 0).unwrap();

        assert_eq!(bus.poll("nisemono", 0), Err(BusError::UnknownToken));
        // 本店のトークンで支店宛は読めない
        assert!(bus.poll(&honten, 0).unwrap().is_empty());
    }

    #[test]
    fn 受け取ったメッセージは消える() {
        let mut bus = Bus::new();
        let honten = reg(&mut bus, "honten");
        let shiten = reg(&mut bus, "shiten");
        bus.send(&honten, "shiten", "一度きり", 0).unwrap();

        assert_eq!(bus.poll(&shiten, 0).unwrap().len(), 1);
        assert!(bus.poll(&shiten, 0).unwrap().is_empty(), "2回目は空");
    }

    #[test]
    fn 未登録の宛先へ送っても失敗しない() {
        // 起動順に依存させない
        let mut bus = Bus::new();
        let honten = reg(&mut bus, "honten");
        bus.send(&honten, "mada-inai", "先に送っておく", 0).unwrap();

        let later = reg(&mut bus, "mada-inai");
        let got = bus.poll(&later, 0).unwrap();
        assert_eq!(got.len(), 1, "後から登録しても受け取れる");
        assert_eq!(got[0].body, "先に送っておく");
    }

    #[test]
    fn 複数のメッセージは送信順に並ぶ() {
        let mut bus = Bus::new();
        let honten = reg(&mut bus, "honten");
        let codex = reg(&mut bus, "codex");
        let shiten = reg(&mut bus, "shiten");

        bus.send(&honten, "shiten", "1通目", 0).unwrap();
        bus.send(&honten, "shiten", "2通目", 0).unwrap();
        bus.send(&codex, "shiten", "3通目", 0).unwrap();

        let got = bus.poll(&shiten, 0).unwrap();
        let bodies: Vec<&str> = got.iter().map(|m| m.body.as_str()).collect();
        assert_eq!(bodies, ["1通目", "2通目", "3通目"]);
        assert!(got[0].id < got[1].id && got[1].id < got[2].id);
    }

    #[test]
    fn 登録解除で名前が空く() {
        let mut bus = Bus::new();
        let token = reg(&mut bus, "honten");
        assert_eq!(bus.unregister(&token).unwrap(), "honten");
        assert!(bus.peers().is_empty());
        // 同じ名前で登録し直せる
        assert!(bus.register("honten", None, 0).is_ok());
    }

    #[test]
    fn 登録解除すると古いトークンは使えない() {
        let mut bus = Bus::new();
        let token = reg(&mut bus, "honten");
        bus.unregister(&token).unwrap();
        assert_eq!(bus.whoami(&token), Err(BusError::UnknownToken));
    }

    #[test]
    fn 登録解除で受信箱も消える() {
        // 同じ名前で登録した別人が、前任者宛のメッセージを読めてしまわないこと
        let mut bus = Bus::new();
        let honten = reg(&mut bus, "honten");
        let shiten = reg(&mut bus, "shiten");
        bus.send(&honten, "shiten", "前任者宛", 0).unwrap();

        bus.unregister(&shiten).unwrap();
        let newcomer = reg(&mut bus, "shiten");
        assert!(
            bus.poll(&newcomer, 0).unwrap().is_empty(),
            "別人が前任者宛を読めてはいけない"
        );
    }

    #[test]
    fn 合言葉なしなら誰でも参加できる() {
        // 既定の挙動。ローカル利用では合言葉を要求しない
        let mut bus = Bus::new();
        assert!(!bus.requires_secret());
        assert!(bus.register("dare-demo", None, 0).is_ok());
    }

    #[test]
    fn 合言葉が一致すれば参加できる() {
        let mut bus = Bus::with_join_secret(Some("aikotoba".into()));
        assert!(bus.requires_secret());
        assert!(bus.register("honten", Some("aikotoba"), 0).is_ok());
    }

    #[test]
    fn 合言葉が違えば参加を拒否する() {
        // 公開ホストに置いたとき、第三者がピアとして入り込むのを防ぐ
        let mut bus = Bus::with_join_secret(Some("aikotoba".into()));
        assert_eq!(
            bus.register("shinnyusha", Some("chigau"), 0),
            Err(BusError::WrongSecret)
        );
        assert_eq!(
            bus.register("shinnyusha", None, 0),
            Err(BusError::WrongSecret),
            "省略も拒否する"
        );
        assert!(bus.peers().is_empty(), "拒否された相手は登録されない");
    }

    #[test]
    fn 空の合言葉は無指定として扱う() {
        // 環境変数が空文字で渡ったときに、意図せず全開放にならないようにする…
        // のではなく、空文字は「設定していない」と解釈する（誤って空を設定した場合に
        // 誰も参加できなくなるより、既定の挙動へ倒すほうが混乱が少ない）
        let bus = Bus::with_join_secret(Some(String::new()));
        assert!(!bus.requires_secret());
    }

    #[test]
    fn 音沙汰が絶えたピアは失効する() {
        // 落ちたセッションが名前を占有し続けると、同じ名前で再接続できない
        let mut bus = Bus::new();
        bus.register("shiten", None, 1000).unwrap();
        assert_eq!(bus.peers(), ["shiten"]);

        bus.sweep(1000 + PEER_TTL_SECS - 1);
        assert_eq!(bus.peers(), ["shiten"], "期限内はまだ生きている");

        bus.sweep(1000 + PEER_TTL_SECS);
        assert!(bus.peers().is_empty(), "期限を過ぎたら消える");
    }

    #[test]
    fn 失効した名前は再登録できる() {
        let mut bus = Bus::new();
        let old = bus.register("shiten", None, 1000).unwrap();
        // 期限切れの時刻で登録すると、内部で sweep されて名前が空く
        let new = bus.register("shiten", None, 1000 + PEER_TTL_SECS).unwrap();
        assert_ne!(old, new);
        assert_eq!(bus.whoami(&new).unwrap(), "shiten");
        assert_eq!(
            bus.whoami(&old),
            Err(BusError::UnknownToken),
            "旧トークンは無効"
        );
    }

    #[test]
    fn pollし続ければ失効しない() {
        // クライアントは1〜2秒間隔で叩く。生きている限り名前を保持できること
        let mut bus = Bus::new();
        let token = bus.register("shiten", None, 1000).unwrap();
        for t in (1000..1000 + PEER_TTL_SECS * 3).step_by(2) {
            bus.poll(&token, t).unwrap();
        }
        assert_eq!(bus.peers(), ["shiten"]);
    }

    #[test]
    fn 送信でも生存とみなす() {
        let mut bus = Bus::new();
        let token = bus.register("honten", None, 1000).unwrap();
        bus.send(&token, "shiten", "生きてるよ", 1000 + PEER_TTL_SECS - 1)
            .unwrap();
        bus.sweep(1000 + PEER_TTL_SECS + 1);
        assert_eq!(bus.peers(), ["honten"], "送信も接触に数える");
    }

    #[test]
    fn 生きているピアの名前は奪えない() {
        // 失効を入れても、稼働中の相手から名前を横取りできてはいけない
        let mut bus = Bus::new();
        bus.register("shiten", None, 1000).unwrap();
        assert_eq!(
            bus.register("shiten", None, 1000 + PEER_TTL_SECS - 1),
            Err(BusError::NameTaken)
        );
    }

    #[test]
    fn 失効しても受信箱は残る() {
        // 明示的な unregister と違い、失効は「落ちたかもしれない」という推測。
        // 復帰したときに受け取れるほうが実用的
        let mut bus = Bus::new();
        let honten = bus.register("honten", None, 1000).unwrap();
        let shiten = bus.register("shiten", None, 1000).unwrap();
        bus.send(&honten, "shiten", "留守中の伝言", 1000).unwrap();

        bus.sweep(1000 + PEER_TTL_SECS);
        assert!(bus.whoami(&shiten).is_err(), "失効している");

        let revived = bus.register("shiten", None, 1000 + PEER_TTL_SECS).unwrap();
        let got = bus.poll(&revived, 1000 + PEER_TTL_SECS).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "留守中の伝言");
    }

    #[test]
    fn 溜まっている総数を数えられる() {
        let mut bus = Bus::new();
        let a = reg(&mut bus, "a");
        assert_eq!(bus.pending_count(), 0);

        bus.send(&a, "b", "1", 0).unwrap();
        bus.send(&a, "c", "2", 0).unwrap();
        assert_eq!(bus.pending_count(), 2);

        let b = reg(&mut bus, "b");
        bus.poll(&b, 0).unwrap();
        assert_eq!(bus.pending_count(), 1);
    }
}
