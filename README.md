# agent-relay

別のマシンで動く AI コーディングエージェント同士が、メッセージをやり取りするための小さな MCP サーバーです。

常駐時のメモリは **約 2.4MB**、バイナリは **約 600KB**。非力なホストに置くことを想定しています。

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
│ Code     │ ◀────────────── │  (2.4MB)     │ ─────────────│ Code     │
└──────────┘                 └──────────────┘              └──────────┘
                              register
                              send_message
                              poll_messages
                              list_peers
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

### 別ホストから繋ぐ

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

### 4つのツール

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

## 環境変数

| 変数 | 既定 | 用途 |
|---|---|---|
| `AGENT_RELAY_ADDR` | `127.0.0.1:8765` | 待ち受けアドレス |
| `AGENT_RELAY_JOIN_SECRET` | なし | `register` に必要な合言葉。公開時は必須 |
| `AGENT_RELAY_ALLOWED_ORIGINS` | なし | 追加で許可する Origin（カンマ区切り） |

## 設計について

### なぜ push ではなく poll なのか

Claude Code のようなエージェントは、ツールを実行したタイミングでしか外部の情報を受け取れません。サーバーから一方的に送りつけても、次にツールが呼ばれるまで読まれません。それなら受信側が取りに来る形のほうが素直だと判断しました。

この判断の結果、SSE ストリームを持たずに済んでいます。MCP の Streamable HTTP 仕様でも、POST の中身がリクエストであれば `application/json` を単発で返してよいと定められています（SSE は必須ではありません）。GET には 405 を返します。

### メッセージは短命です

受け取ったら消えます。履歴を貯める用途は想定していません。永続化もしないので、サーバーを再起動すると未読は失われます。

「エージェント同士がその場で会話する」ことに絞った設計です。記録を残したいものは、Issue なり別の仕組みなりに書いてください。

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
外部 :9998  ──▶  Caddy  ──▶  127.0.0.1:8765 (agent-relay)
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
