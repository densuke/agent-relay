# agent-relay

別のマシンで動く AI コーディングエージェント同士が、メッセージをやり取りするための小さな MCP サーバーです。

中継サーバーの常駐メモリは **約 1.0MB**（musl ビルド実測）。非力なホストに置くことを想定しています。

配布物には2つのバイナリが入っています。

| バイナリ | 役割 | 置き場所 | サイズ |
|---|---|---|---|
| `agent-relay` | 中継サーバー本体（HTTP） | どこか1台 | 約 900KB |
| `agent-relay-shim` | 各エージェントが繋ぐ stdio MCP サーバー | エージェントを動かす各ホスト | 約 1.2〜1.8MB |

`agent-relay` だけでも動きます（各エージェントが HTTP MCP として直接繋ぎ、`poll_messages` で取りに行く形）。
shim を挟むと、届いたメッセージが Claude Code のセッションに**そのまま割り込む**ようになります。詳細は下記「Claude Code へ push する」を参照してください。

## 何のためのものか

Claude Code の `SendMessage` は、同じセッションが起動したサブエージェント宛てにしか届きません。別のマシンで動いているセッションとは通信できません（[claude-code#28300](https://github.com/anthropics/claude-code/issues/28300) で要望が挙がっていますが未実装です）。

同種の OSS はいくつかありますが、いずれもローカルの SQLite ファイルを共有する前提で、マシンをまたげません。

- [agent-bus](https://github.com/MustaphaSteph/agent-bus)
- [claude-peers-mcp](https://glama.ai/mcp/servers/Nexus-Digital-Automations/Claude_Code_MCP_2)
- [agent-comms-mcp](https://github.com/watchout/agent-comms-mcp)

agent-relay は中継サーバーを1つ立て、そこへ各エージェントが MCP クライアントとして接続する形をとります。ホストが違っても、同じサーバーに繋がってさえいれば会話できます。

```
  ホストA                        中継サーバー                    ホストB
┌──────────┐                 ┌──────────────┐              ┌──────────┐
│ Claude   │ ── MCP/HTTP ──▶ │ agent-relay  │ ◀── MCP/HTTP │ Claude   │
│ Code     │ ◀────────────── │              │ ─────────────│ Code     │
└──────────┘                 └──────────────┘              └──────────┘
                              register / send_message
                              poll_messages / list_peers / unregister
```

shim を挟むと、各ホスト側が stdio の MCP サーバーになり、着信が push で届きます。

```
  ホストA                                中継サーバー                            ホストB
┌──────────┐   stdio    ┌──────┐  HTTP  ┌──────────────┐  HTTP  ┌──────┐   stdio    ┌──────────┐
│ Claude   │ ─────────▶ │ shim │ ─────▶ │ agent-relay  │ ◀───── │ shim │ ◀───────── │ Claude   │
│ Code     │ ◀───────── │      │ ◀───── │              │ ─────▶ │      │ ─────────▶ │ Code     │
└──────────┘  channel   └──────┘        └──────────────┘        └──────┘  channel   └──────────┘
              通知                                                          通知
```

## 使い方

### サーバーを起動する

```bash
agent-relay
# => agent-relay listening on http://127.0.0.1:8765/mcp
```

待ち受けアドレスは環境変数で変えられます。

```bash
AGENT_RELAY_ADDR=127.0.0.1:9000 agent-relay
```

### 別ホストから繋ぐ（HTTP で直接つなぐ場合）

ここは shim を使わず、各エージェントが中継サーバーへ HTTP MCP で直接つなぐ構成の話です。push は使えず、`poll_messages` で取りに行く形になります。shim を使う場合は「Claude Code へ push する」を参照してください。

**既定では 127.0.0.1 のみを待ち受けます。** 外部に直接さらす代わりに、SSH ポートフォワードを使ってください。認証を自前で持たずに済み、経路も暗号化されます。

```bash
# 手元のマシンから、中継サーバーのいるホストへトンネルを掘る
ssh -N -L 8765:127.0.0.1:8765 user@relay-host
```

これで手元の `http://127.0.0.1:8765/mcp` が中継サーバーに繋がります。

### Claude Code に登録する

```bash
claude mcp add --transport http agent-relay http://127.0.0.1:8765/mcp
```

### 5つのツール

| ツール | 引数 | 動作 |
|---|---|---|
| `register` | `name`, `secret`（任意） | 名前を登録し、**トークンを受け取る**。最初に一度だけ呼ぶ |
| `send_message` | `token`, `to`, `body` | 指定した相手へ送る |
| `poll_messages` | `token` | 自分宛の未読を取り出す（取り出したら消える） |
| `list_peers` | なし | 登録済みのピア一覧 |
| `unregister` | `token` | 登録を解除する（名前が空く。未読は破棄） |

会話の例です。

```
[ホストA] register(name="alice")
         => alice として登録しました。
            token: 3f9a1c...

[ホストB] register(name="bob")
         => token: 7d2e40...

[ホストA] send_message(token="3f9a1c...", to="bob", body="ビルド通った？")
[ホストB] poll_messages(token="7d2e40...")
         => [{ "id": 0, "from": "alice", "to": "bob", "body": "ビルド通った？" }]
```

送信者名を引数で受け取らないのは、他人を騙って送れないようにするためです。`from` はサーバーがトークンから決めます。

### 動作確認

```bash
curl -s http://127.0.0.1:8765/healthz
# => {"status":"ok","peers":2,"pending":0}
```

## Claude Code へ push する

ここまでの構成では、エージェントは自分で `poll_messages` を呼ばないと着信に気づけません。`agent-relay-shim` を挟むと、届いたメッセージが**セッションに割り込む**ようになります。相手に話しかけられたら、そのまま会話が始まります。

仕組みは Claude Code の `claude/channel` を使っています。この capability を宣言した MCP サーバーが `notifications/claude/channel` を送ると、その内容が新しいターンとして注入されます。公式の Discord プラグインと同じ経路です。

### 使えるための条件

`claude/channel` にはゲートがあります。次を満たさないと通知は捨てられます（Claude Code 2.1.226 で確認）。

- **stdio トランスポートであること** — HTTP 接続は「unsolicited notification path が無い」として弾かれます。shim が stdio で立つのはこのためです
- **起動時に `--channels` で明示されていること** — セッションごとの opt-in です
- **対話モードであること** — `-p` のヘッドレス実行では登録処理そのものが走りません
- ファーストパーティのプロバイダであること（Bedrock / Vertex / Foundry では使えません）
- 組織のポリシーで無効化されていないこと

### 設定

まず接続情報を `~/.config/agent-relay/env` に置きます。shim がここから読むので、**合言葉がモデルのコンテキストに載りません**。

```bash
mkdir -p ~/.config/agent-relay && chmod 700 ~/.config/agent-relay
cat > ~/.config/agent-relay/env <<'EOF'
AGENT_RELAY_URL=https://relay.example.com/mcp
AGENT_RELAY_JOIN_SECRET=your-secret
AGENT_RELAY_NAME=honten
EOF
chmod 600 ~/.config/agent-relay/env
```

MCP サーバーとして登録します。

```bash
claude mcp add --scope user agent-relay ~/.local/bin/agent-relay-shim
```

起動時にチャンネルを有効化します。サーバー名は上で登録した名前（`agent-relay`）です。

```bash
claude --dangerously-load-development-channels server:agent-relay
```

初回は確認画面が出ます。バイパス権限の警告と違い**この承認は保存されない**ので、起動のたびに出ます。

承認済みのプラグインとして配れば `--channels plugin:<name>@<marketplace>` で済み、確認画面も出なくなります。ただし managed settings の `allowedChannelPlugins` は**デフォルトの Anthropic allowlist を置き換える**点に注意してください。設定すると、そこに列挙しなかった公式プラグイン（Discord など）のチャンネルが動かなくなります。

既に別のチャンネルを使っている場合も、そのまま併記できます。

```bash
claude --channels plugin:discord@claude-plugins-official \
       --dangerously-load-development-channels server:agent-relay
```

### モデルから見えるもの

shim が公開するツールは2つだけです。

| ツール | 引数 | 動作 |
|---|---|---|
| `send_message` | `to`, `body` | 相手へ送る |
| `list_peers` | なし | 参加中のノード一覧 |

`register` と `poll_messages` は shim が内部で行うので出てきません。トークンもモデルには渡りません。

届いたメッセージはこう見えます。

```
← agent-relay · shiten: ビルド通った？
```

`meta.chat_id` と `meta.user` に送信元の名前が入るので、返信は `send_message(to="shiten", ...)` です。

### 名前の重複

同じ名前が埋まっていた場合、shim は `honten-2`、`honten-3` … と連番を付けて登録します。1 台のホストでセッションを何本も立てる運用で、後発が締め出されないようにするためです。

### 他のチャンネルと併用するとき

1つのセッションに複数のチャンネルを繋げられます（例: Discord プラグインと本 shim）。どれも同じ `<channel source="..." ...>` の形で届くため、`source` 属性でしか区別がつきません。

shim の `instructions` は適用範囲を `source="agent-relay"` に明示的に限定しています。これが無いと、Discord 宛の返事を中継網へ流してしまいます（実際に起きました）。

### 返信の抑制

相手も自動で動くセッションです。「了解」「ありがとう」の往復がそのままトークン消費になるため、shim の `instructions` で「返信は必要なときだけ」と指示しています。相槌だけの返信は送らないよう促されます。

### この仕組みは experimental です

`claude/channel` は Claude Code の非公開・実験的な機能で、ドキュメント化されていません。更新で黙って壊れる可能性があります。公式の Discord プラグイン（`external_plugins/discord`）が同じ経路を使っているので、動かなくなったらそちらの実装と Claude Code のバイナリを見比べるのが早いです。

## 環境変数

| 変数 | 既定 | 用途 |
|---|---|---|
| `AGENT_RELAY_ADDR` | `127.0.0.1:8765` | 待ち受けアドレス |
| `AGENT_RELAY_JOIN_SECRET` | なし | `register` に必要な合言葉。公開時は必須 |
| `AGENT_RELAY_ALLOWED_ORIGINS` | なし | 追加で許可する Origin（カンマ区切り） |

shim 側（`~/.config/agent-relay/env` からも読みます。環境変数が優先）。

| 変数 | 既定 | 用途 |
|---|---|---|
| `AGENT_RELAY_URL` | `http://127.0.0.1:8765/mcp` | 中継サーバーのエンドポイント |
| `AGENT_RELAY_JOIN_SECRET` | なし | 参加の合言葉 |
| `AGENT_RELAY_NAME` | ホスト名 | 中継網での自分の名前 |
| `AGENT_RELAY_POLL_MS` | `2000` | 受信を確認する間隔（ミリ秒） |

## 設計について

### なぜサーバー本体は poll なのか

中継サーバーは受信側が取りに来る形にしています。SSE ストリームは持ちません。MCP の Streamable HTTP 仕様でも、POST の中身がリクエストであれば `application/json` を単発で返してよいと定められています（SSE は必須ではありません）。GET には 405 を返します。

「それでは届いたことに気づけないのでは」という話は、shim 側で解決しています。shim が 2 秒間隔でサーバーを叩き、着信があった瞬間に Claude Code へ通知を上げます。**このポーリングはエージェントのターンの外で起きるので、トークンを一切消費しません。** 実際にメッセージが届いたときだけターンが立ちます。

ポーリングの往復を端（shim）に閉じ込め、サーバー本体は単純な HTTP のままにする、という切り分けです。

### ピアの登録は 60 秒で失効します

`poll_messages` も `send_message` も呼ばれないまま 60 秒経つと、そのピアの登録は解除され、名前が空きます。shim は 2 秒間隔で叩くので、動いている限り失効しません。

失効を設けているのは、落ちたセッションが名前を占有し続けるのを防ぐためです。1 台のホストで Claude Code のセッションが何本も立つ運用だと、これが無いと後から起動したセッションが同じ名前で参加できなくなります。

失効の場合、受信箱の中身は**残します**。落ちたのか一時的に止まっているだけなのか区別できないので、復帰したときに受け取れるほうが実用的だと判断しました。明示的に `unregister` した場合だけは、本人が降りる意思を示しているので受信箱も破棄します。

### メッセージは短命です

受け取ったら消えます。履歴を貯める用途は想定していません。

未読が消える条件は2つだけです。混同しやすいので整理しておきます。

| 出来事 | 受信箱 |
|---|---|
| ピアの登録が失効（60秒無応答） | **残る**（復帰したら受け取れる） |
| 明示的な `unregister` | 破棄される |
| 中継サーバーの再起動 | **失われる**（状態はメモリのみ） |

「エージェント同士がその場で会話する」ことに絞った設計です。相手が落ちている間の取り置きは効きますが、サーバーの再起動をまたぐ保証はありません。記録を残したいものは、Issue なり別の仕組みなりに書いてください。

### 宛先が未登録でも送れます

相手がまだ `register` していなくても送信は成功し、受信箱に積まれます。後から登録して `poll_messages` すれば受け取れます。起動順に依存させないための判断です。

### セキュリティ

MCP の仕様が挙げている注意点に従っています。

- **既定で 127.0.0.1 のみを待ち受ける**。全インターフェースには開きません
- **`Origin` ヘッダを検証する**（DNS リバインディング対策）。ホスト名を取り出して完全一致で判定するので、`http://localhost.evil.example.com` のような偽装は弾きます

### 参加を制限する

`AGENT_RELAY_JOIN_SECRET` を設定すると、`register` に合言葉が要るようになります。

```bash
AGENT_RELAY_JOIN_SECRET=your-secret agent-relay
```

```
register(name="alice", secret="your-secret")   # 通る
register(name="intruder")                       # 拒否される
```

**公開ホストに置く場合は必ず設定してください。** 未設定だと、到達できる相手は誰でもピアとして参加でき、メッセージを送ったり `list_peers` で構成を覗いたりできます。未設定で起動すると警告を出します。

役割が2段に分かれています。

- **合言葉** — 参加してよい相手かどうか（入口）
- **トークン** — 誰として送るか（同定）

### リバースプロキシ越しに公開する

Caddy や nginx の後ろに置く場合、そのホスト名を許可してください。

```bash
AGENT_RELAY_ALLOWED_ORIGINS=https://relay.example.com agent-relay
```

カンマ区切りで複数指定できます。既定の localhost は常に許可されます。

agent-relay 自体は 127.0.0.1 を待ち受けたままにし、プロキシから転送する構成を推奨します。

```
外部 :443 (TLS)  ──▶  Caddy  ──▶  127.0.0.1:8765 (agent-relay)
```

TLS の終端はプロキシに任せます。agent-relay は HTTP のみを話します。

## インストール

[Releases](https://github.com/densuke/agent-relay/releases) から、環境に合ったバイナリを取得してください。静的リンク（musl）なので、置くだけで動きます。

```bash
curl -sL -o agent-relay.tar.gz \
  https://github.com/densuke/agent-relay/releases/latest/download/agent-relay-x86_64-unknown-linux-musl.tar.gz
tar xzf agent-relay.tar.gz
./agent-relay
```

### 常駐させる（systemd ユーザーサービス）

root 権限は要りません。

`~/.config/systemd/user/agent-relay.service`:

```ini
[Unit]
Description=agent-relay (MCP relay for AI coding agents)
After=network.target

[Service]
Type=simple
ExecStart=%h/.local/bin/agent-relay
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now agent-relay
```

ログアウト後も動かし続けたい場合は `loginctl enable-linger $USER` を実行してください。

## ソースからビルドする

```bash
cargo build --release
cargo test
```

## ライセンス

MIT
