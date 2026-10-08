# 互換性・実現性の検証記録（タスク0）

検証日：2026-10-08（日本時間）、macOS（Darwin 27.0.0）。Linuxは未検証。

## 対象バージョンと参照ソース

| 対象 | バージョン | 参照 |
|---|---|---|
| Herdr | 0.9.3（API protocol 22、schema_version 1） | `herdrdev/herdr` タグ `v0.9.3`、commit `7b116c05bfda646af39d2524c54e70c751f57ee8` |
| Claude Code | 2.1.293 | 実機の履歴JSONL（ソース非公開） |
| Codex CLI | 0.160.1（`history_mode: "paginated"`） | 実機の履歴JSONL |

検証は専用のHerdrワークスペース（空のテスト用ディレクトリ1つ、pane 4つ）で行った。Claude 2つ（`--model haiku`）とCodex 2つを同じcwdで起動し、合成した指示文だけを送った。ユーザーの既存セッションへは何も送っていない（既存履歴は読み取りのみ）。

`tests/fixtures/raw/` は、このテストセッションの履歴を `tests/fixtures/sanitize.py` で縮約したもの。アカウントID、指示文、ツール入出力、推論、暗号化データは削除し、パスは `/work` と `/home/user` に置換した。一部のテストは、この上に合成レコードを追加する（テストコード内に明記）。

## 結論

**合格（制約付き）。** 両エージェントで、Herdrのセッション参照から対象の履歴だけを特定し、最後の完了した回答を抽出できた。同じcwdの2セッションも取り違えない。ただし、下記「既知の制約」の1は履歴からは検出できない（専用フックでも検出できない）。

## Herdr

- `agent.get` は `agent_session: {source, agent, kind, value}` を返す。Claude・Codexとも **`kind` は常に `"id"`**。Claudeのhookは `transcript_path` も報告するが、Herdr 0.9.3はpi/omp以外ではpathを公開しない（`src/agent_resume.rs` `session_ref_from_report`）。したがって、許可した履歴ルート内でIDが完全一致するファイルを探す。
- Codexの `agent_session` は **最初のプロンプトを送るまで登録されない**（CodexのSessionStart hookが初回ターンで発火するため）。起動直後や `codex fork` 直後は `SessionUnavailable` になる。
- `interactive_ready` は `herdr agent start` で起動した管理対象エージェントでのみ `true` になる（`managed_agent_interactive_ready`）。シェルから手動で起動したエージェントでは常に省略（false）。計画の「Bの`interactive_ready`がtrue」をそのまま条件にすると、手動起動のエージェントへは送れない。→ 実装では `launch_pending` が true なら拒否し、`interactive_ready` の省略は許容する（台帳の裁定を参照）。
- `agent.prompt` はblocked状態とstartup中の管理対象エージェントを書き込み前に拒否する（`agent_blocked` / `agent_not_ready`）。本文をbracketed pasteで送り、300ms後にEnterを送る。
- ソケットは1接続1リクエスト。リクエスト行の上限は **1MiB**（`MAX_INITIAL_REQUEST_BYTES`）、応答待ちは5秒（`APP_RESPONSE_TIMEOUT`）。
- 条件付き送信（比較付きトランザクション）やidempotency keyはない。

### 送信の実測

| 送信先 | 内容 | 結果 |
|---|---|---|
| Claude（haiku） | 3行＋コードフェンス＋日本語 | 1回の送信として処理 |
| Codex | 同上 | 1回の送信として処理 |
| Claude | 256KiB（4784行、リクエスト267,032バイト） | 全行を受信（行数を正確に回答） |
| Codex | 64KiB（1209行） | 全行を受信（行数を正確に回答） |

Codexへの256KiBは利用枠の都合で未検証。timeoutは再現していない。Herdr側の5秒応答待ちを超えた場合、本文が書き込まれたかどうかはクライアントから判別できないため、`DeliveryUnknown` として扱う。

## Claude Code 2.1.293

保存先：`~/.claude/projects/<cwdを変換した名前>/<session_id>.jsonl`。各レコードに `sessionId`。

- 会話レコード（`user`, `assistant`, `system`, `attachment`）は `uuid` と `parentUuid` を持ち、木構造をなす。メタデータ（`last-prompt`, `mode`, `permission-mode`, `ai-title`, `file-history-snapshot`, `cost-state` など）は `uuid` を持たない。
- assistantの1メッセージは **content blockごとに1レコード**（thinking、text、tool_useが別レコード）。同じ `message.id` を共有し、`parentUuid` で順につながる。
- 正常終了したメッセージは全レコードで `stop_reason: "end_turn"`。ツール呼び出し前は `"tool_use"`、ストリーミング途中は `null`。
- **ターン完了の証拠は `system` / `subtype: "turn_duration"`**。直前に `stop_hook_summary`（Stop hookがある場合のみ）、その前に `attachment` が入ることがある。手元の全履歴（49ファイル）では、2.1.280以降の正常終了ターンは1件を除き `turn_duration` を持つ。2.1.219以前は持たない（＝未対応版として拒否される）。Stop hookが1つもない環境での記録有無は未検証（本検証環境にはプラグインのStop hookがある）。
- idle中に `system` / `away_summary` が `turn_duration` の後へ追記されることがある。
- 中断：`user` の text `[Request interrupted by user]`（ツール実行中は `… for tool use]`）。その後に `turn_duration` が付く場合と付かない場合がある。
- `--resume <id>`：同じsession ID・同じファイルに追記。
- `--resume <id> --fork-session`：新しいsession ID。**最初の発言まではファイルが作られない**。作られると親の履歴がコピーされ、コピー分の `sessionId` も新IDに書き換わる。
- rewind（Esc Esc → Restore conversation）：**その時点では何も記録されない**。次の発言のuserレコードが、巻き戻し先の `turn_duration` を `parentUuid` に持つ。つまり有効な分岐は「ファイル中で最後の会話レコードから親をたどった鎖」。
- Herdrがidleを検知した時点（50ms間隔で観測）で、`turn_duration` まで書き込み済みだった（2回とも4〜7ms以内に読み取り成功）。

### 抽出規則（実装）

1. ルート直下の `*/<id>.jsonl` を探す。0件は `TranscriptUnavailable`、2件以上は `SessionAmbiguous`。
2. ストリーミングで読み、`uuid` を持ち `isSidechain` がtrueでないレコードの親子関係と最小限の情報（種別・message ID・stop_reason・text blockの位置）だけを保持する。末尾の改行なし行は書き込み途中として保留する。
3. 最後の会話レコードから親をたどる。`attachment` と `turn_duration` 以外の `system` は読み飛ばす。最初に `turn_duration` が見つからなければ未完了（中断なら `NoCompletedAnswer`、それ以外は `CompletionUncertain`）。
4. `turn_duration` からさらに親をたどり、`system`／`attachment` を飛ばした最初のレコードが `stop_reason: "end_turn"` のassistantであること。API エラー（`isApiErrorMessage`）は `NoCompletedAnswer`。
5. 同じ `message.id` が続く間だけ親をたどり、text blockを鎖の順に結合する（thinking・tool_use は含めない）。同じ `uuid` の重複は1回だけ数える。
6. 使用したレコードの `sessionId` がHerdrのsession IDと一致すること。

## Codex CLI 0.160.1

保存先：`~/.codex/sessions/YYYY/MM/DD/rollout-<時刻>-<thread_id>.jsonl`（ベース）と `rollout-<時刻>-<thread_id>_<segment_id>.jsonl`（セグメント）。各レコードに `ordinal`（スレッド内の通し番号）。

- 先頭は `session_meta`。`payload.id` がthread ID（Herdrのsession IDと一致）。
- ターン：`event_msg` `task_started{turn_id}` → … → `task_complete{turn_id, last_agent_message}`。中断は `turn_aborted{turn_id, reason: "interrupted"}`。
- assistantの発言は `response_item` `message`（`role: "assistant"`）で、`phase` が `"commentary"`（途中経過）か `"final_answer"`（最終回答）。本文は `content[].output_text`。`internal_chat_message_metadata_passthrough.turn_id` でターンと対応づく。
- `event_msg` `item_completed` にも同じ本文の `AgentMessage` が出る（二重に数えない）。
- **rewind（Esc Esc → Enter）**：その時点で新しいセグメントファイルが作られ、`session_meta.history_base = {thread_id, end_ordinal_exclusive, end_byte_offset}` が巻き戻し先を指す。以降のターンはセグメントへ書かれ、ベースは更新されない。
- `codex resume <id>`：最新のセグメントへ追記。
- `codex fork <id>`：新しいthread IDのファイル。`history_base.thread_id` と `forked_from_id` が親を指し、親の履歴はコピーされない。
- 過去の版（0.159.x）のforkは、親の `session_meta` と履歴をファイル内にコピーする形式だった（先頭の `session_meta` が新ID）。
- 0.154.0で、同じmessage IDの `final_answer` が2回記録され、後の方が `last_agent_message` と一致した例がある（重複レコード）。
- Planモードのターンで、`last_agent_message` が `final_answer` でなく commentary と一致した例がある（0.159.2）。
- `thread_rolled_back` イベントは手元の履歴に1件もない。

### 抽出規則（実装）

1. ルート配下の `rollout-*-<id>.jsonl` と `rollout-*-<id>_<segment>.jsonl` を集める。ベースが2件以上なら `SessionAmbiguous`。セグメントがあれば segment ID（UUIDv7、時刻順）が最大のものを、なければベースを読む。
2. 読んだファイルの先頭 `session_meta.id` がHerdrのsession IDと一致すること。セグメントなら `history_base.thread_id` も一致すること。
3. そのファイル内で最後の `task_started` のターンを対象にする。ターン内に `thread_rolled_back` など未確認のイベントがあれば `UnsupportedTranscript`。
4. `turn_aborted` なら `NoCompletedAnswer`。`task_complete` がなければ `CompletionUncertain`。
5. `phase: "final_answer"` のassistantメッセージを、message IDで重複除去（後勝ち）して最後のものを回答とする。passthroughのturn_idがあれば一致を確認する。
6. `last_agent_message` が回答本文と異なれば `CompletionUncertain`（Planモードの例を安全側で拒否する）。
7. `phase` を持たないassistantメッセージだけのターンは、`task_complete` があり、その `last_agent_message` と本文が一致する場合だけ採用する。
8. ファイル内に `task_started` が1つもない（rewind・fork直後で新しい発言がない）場合は `NoCompletedAnswer`。`history_base` をたどって親の履歴は読まない。

## 既知の制約

1. **Claudeのrewind直後（次の発言を送る前）は、巻き戻される前の回答を返す。** rewindは履歴にもhookにも何も残さないため、履歴方式でも専用Stop hook方式でも検出できない。プレビューで本文を確認してから送るよう、READMEに記載する。次の発言を送った後は正しい分岐を読む。
2. Codexのrewind直後・fork直後は、新しい発言をするまで「完了した回答なし」として止まる（巻き戻し先の回答は存在するが、安全側に倒す）。
3. Codexは最初のプロンプトを送るまでHerdrにセッション参照が登録されない。
4. Claude 2.1.219以前など `turn_duration` を記録しない版は、すべて `CompletionUncertain` になる。
5. Claudeのローカルコマンド（`/model` など）は `turn_duration` の後に `<command-name>` / `<local-command-stdout>` のuserレコードを追記する（手元の履歴で31件確認）。その後は次の発言まで `CompletionUncertain`。読み飛ばすと、コマンド型プロンプトで新しいターンが始まった直後に古い回答を返しうるため、読み飛ばさない。
6. 事前チェックと送信は不可分ではない。チェック直後の入れ替わりは排除できない。Bの入力欄に未送信の文字があると、送った本文の前に連結される（Claudeはrewind後に入力欄へ前の指示を戻す）。利用条件として「Bの入力欄が空であること」をREADMEに記載する。

## 実機受け入れ（タスク6）

2026-10-08、macOS、上記と同じテスト用ワークスペース。ポップアップ本体（`bin/agent-relay`）をテスト用の5つ目のpaneで直接起動し、`herdr pane send-text` / `send-keys` で操作した。Aの固定は `agent-relay open` と同じ形式の状態ファイルをスクリプトで作って行った。**`herdr plugin link` とキー割り当てによる起動（`open` アクション → `plugin.pane.open` のpopup表示）は、ユーザーのHerdr設定を変更するため未実施。**

Aの回答はいずれも「見出し・日本語の段落・Rustのコードブロック・40行の番号付きリスト・`<TAG>-END`」（54行、約0.8KB）。指示は日本語（1件目は `Alt+Enter` で2行）。

| ケース | 結果 |
|---|---|
| Claude → Codex | 1回だけ届き、回答部分は原文と完全一致。Codexが「40行 / ALPHA-END」と処理 |
| Codex → Claude | 1回だけ届き、完全一致（Claudeは長い貼り付けを `<pasted_content>` で包んで記録するが中身は同一）。Claudeが処理 |
| Codex → Codex | 1回だけ、完全一致、処理された |
| Claude → Claude | 1回だけ、完全一致、処理された |
| 送信時に `Enter` を3回連続 | 1回だけ送信。残りは破棄され「送信しました」画面のまま |
| ポップアップ表示後にAが新しい回答 | 送信せず、新しい回答を読み直し、指示を保持して「回答が更新されました」 |
| Bが処理中 | 一覧で `✕ 処理中`、選んでも送れない |
| エージェントのいないpane | 一覧で `✕ AIエージェントなし` |
| 稼働中のA（このセッション自身） | `AgentNotReady` で取得しない |
| 終了時 | 状態ファイルが削除される |

自動テスト：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test`（98件）、`sh scripts/build.sh` がすべて成功。

未確認：Linux、`herdr plugin link` 経由の起動とpopup表示、Codexへの256KiB送信、送信のtimeout（実際には発生させていない。fakeのソケットで `DeliveryUnknown` になることのみ確認）、Stop hookのない環境でのClaudeの `turn_duration`。

## Codex 0.161.0 のデーモン（2026-10-09 追記）

Codex 0.161.0 の TUI は、既定で共有のバックグラウンド app-server（`codex app-server --managed-daemon`）を使う。SessionStart フックはこのデーモンの中で実行され、環境変数はデーモンを起動したペインのもの（実測：`HERDR_PANE_ID=w1:p4J`）になる。そのため Herdr の Codex 連携（integration v8）は、Codex が動いているペインにセッションを登録できない（テスト用ペインで再現。フックは `hook/started`→`hook/completed` と実行されていた）。

- `codex --no-daemon` で起動すると、最初の発言後にセッションが登録され、本プラグインで最新の回答・回答一覧とも読めた（0.161.0 の rollout 形式は 0.160.1 と同じ範囲で読めた）。
- デーモンを起動したペインが後で Codex を動かすと、他のペインの Codex セッションまでそのペインに登録されるおそれがある（未再現）。`--no-daemon` で避けられる。

### 対応（2026-10-09）

既定（デーモン経由）の Codex のペインは、ペインの端末タイトル「スレッド名 | プロジェクト」と作業ディレクトリが、デーモン（`$CODEX_HOME/app-server-control/app-server-control.sock`、Unix ソケット上の WebSocket JSON-RPC）の `thread/loaded/list` → `thread/read` の `name`・`cwd` と1件だけ一致したとき、そのスレッドIDで扱う（`src/codex_daemon.rs`）。Herdr の登録がデーモン上のスレッドを指すときはタイトルで確かめられた場合だけ使い、デーモンにないスレッド（`--no-daemon`）は Herdr の登録をそのまま使う。

実機確認：同じディレクトリでデーモン経由の Codex 2つと Claude 1つを動かし、Codex 2つが別スレッドとして特定され、Codex→Claude・Claude→Codex の送信がともに処理された。同名スレッドの曖昧さ、名前が付く前、デーモンに届かない場合は自動テストで確認。
