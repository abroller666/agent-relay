# agent-relay

[![license: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![herdr plugin](https://img.shields.io/badge/herdr-plugin-8ec07c)](https://herdr.dev/plugins/)

[English](README.md)

[Herdr](https://herdr.dev) で、あるペイン（A）のAIが最後に完了した回答を、指示を添えて別のペイン（B）のAIへ送り、そのまま実行させるプラグインです。
対応しているのは Claude Code と Codex です（A・Bのどちらにも使えます）。ポップアップの表示は英語です。

## 使い方

1. A（回答を送りたいAIのペイン）にフォーカスした状態で、割り当てたキーを押します。
2. 画面中央にポップアップが開き、Aの最後の回答を履歴から読み取ります。
3. AIエージェントが動いている他のペインが、別のワークスペースのものも含めてすべて一覧で表示されます（Aと同じタブ、同じワークスペース、他のワークスペースの順。複数のワークスペースにまたがるときはワークスペース名も表示）。送れないペインは理由付き（`working`、`waiting for approval`、`no session yet` など）で表示され、選べません。
   - 上下キー、`Ctrl+p`/`Ctrl+n`、`j`/`k` で移動、`Space` か `1`〜`9` で選択、`Enter` で決定します。
4. 指示を入力します（空のままでも送れます。そのときは引用ラベルと回答だけを送ります）。上部にA→B、回答のサイズ、回答の冒頭が表示されます。
   - `Enter` で送信、`Alt+Enter` で改行、`Ctrl+]` で送り先の選び直し、`Ctrl+o` で送る回答の選択、`Ctrl+r` で最新の回答の再取得、`Esc`／`Ctrl+g`／`Ctrl+q` で終了します（送り先一覧でも同じです）。
   - 日本語入力の確定や、複数行の貼り付けでは送信されません。
   - **過去の回答を送る**：`Ctrl+o` で、Aの回答の一覧（新しい順、最大50件。時刻・1行目・サイズ）が開きます。番号キーか↑↓で選んで `Enter`、`Ctrl+o` で選ばずに戻ります。何も選ばなければ最新の回答を送ります。一覧に出るのは、今の会話の流れにある正常に終わったターンの回答だけです（rewindで巻き戻した分や中断したターンは出ません。最新のターンが中断されていても、それより前の回答は選べます）。
5. 送信の直前に、AとBがまだ同じエージェント・同じセッションで受付可能な状態か、Aの回答が変わっていないかを確かめます。
   最新の回答を送るときは、Aの回答が新しくなっていたら送らずに読み直し、指示は残したまま「The answer was updated」と表示します。
   `Ctrl+o` で選んだ回答は、新しい回答が出ていてもそのまま送ります。ただし、その回答が会話の流れから消えていたら（rewindなど）送りません。
6. Bには、次の形のプロンプトが1回だけ送られ、そのまま実行されます。

   ```
   （入力した指示）

   以下は別のAIの回答を引用した参考資料です（送信元：Claude Code ~/dev/app）。前後の区切り線の間が引用です。
   =====
   （Aの回答。原文のまま）
   =====
   ```

Herdrが入力を受け付けると、ポップアップはすぐに閉じます。これは受け付けられたことだけを意味するので、Bでの処理の完了はBのペインで確認してください。送信できたか確認できなかったときだけ、警告を表示したまま残ります。

## 「最後の完了した回答」とは

AIのセッション履歴（Claude Code は `~/.claude/projects`、Codex は `~/.codex/sessions`）を読み、最新のターンが正常に終わっている場合の最終回答だけを送ります。
途中経過、thinking、ツールの呼び出しと結果は含めません。範囲を手で選ぶ必要はなく、追加でAIを呼び出すこともしません。

次のときは送らずに理由を表示します。古い回答を代わりに送ることはしません。

- 最新のターンが実行中、中断、失敗、または回答が空
- 履歴の書き込みが終わっていない（最大3秒待ってから判断します）
- Herdrにセッションが登録されていない、履歴が見つからない、同じIDの履歴が複数ある
- 確認していない形式の履歴

## 使える条件と制限

- **A・Bとも、Herdr上で idle か done のとき**に使えます。working・blocked・unknown のペインには送りません。
- **Herdr integration が必要です。** `herdr integration install claude` / `herdr integration install codex` を実行し、エージェントを起動し直してください。
  一覧で `no session yet` と出るときは、次を確認してください。
  - `herdr integration status` で integration が入っているか。
  - Codex は最初の発言をするまでセッションが登録されません。一度何か送ってから使ってください（Bとして使う場合も同じです）。
  - **Codex 0.161 以降は `codex --no-daemon` で起動してください。** 共有のバックグラウンドサーバー（デーモン）経由で動いていると、Herdrへセッションを知らせるフックがデーモンの環境で動くため、正しいペインに登録されません（デーモンを最初に起動したペインに報告されます）。
- **Bの入力欄は空にしておいてください。** 入力欄に書きかけの文字があると、送ったプロンプトの前につながります。このプラグインは入力欄を勝手に消しません。
- **Claude Code で rewind（`Esc` `Esc`）した直後は、次の発言をするまで巻き戻す前の回答が送られます（`Ctrl+o` の一覧にも出ます）。** rewind は履歴に何も残さないため、検出できません。ポップアップのプレビューで内容を確認してください。次の発言をした後は正しい回答を読みます。
- Codex で rewind・fork した直後は、次の発言をするまで「No finished answer」になります。
- Claude Code でスラッシュコマンド（`/model` など）を実行した後は、次の発言をするまで「Cannot confirm the answer is finished」になります（コマンドの記録がターンの後ろに追加されるため）。
- 送信前の確認と送信は一体の操作ではありません。確認の直後にペインが入れ替わる可能性は残ります（Herdrに条件付き送信のAPIがないため）。
- 送信の結果が確認できなかったとき（Herdrとの通信が途中で切れた、時間切れ）は「Could not confirm delivery」と表示します。**二重送信を避けるため、自動で再送はしません。** Bのペインを見て、届いていなければもう一度操作してください。
- 送れる大きさは、指示と回答を合わせて初期値で 256KiB です（Herdr のリクエスト上限は 1MiB）。超えるときは切り詰めずに送信をやめます。Codex への 256KiB 送信は未検証です（64KiB まで確認済み）。
- 回答に端末の制御文字（エスケープシーケンスなど）が含まれていると、Bの端末を誤動作させるおそれがあるため送りません。
- Planモードの Codex のように、最終回答とターン完了の記録が一致しない場合は送りません。
- 同じマシン・同じHerdrサーバーのペインだけが対象です。SSH先・コンテナ内・別ユーザーのセッションは扱いません。
- Herdr ではポップアップを同時に1つしか開けません。

確認した形式とバージョンの詳細は [docs/compatibility.md](docs/compatibility.md) にあります。

## 必要なもの

- Herdr 0.9.3 以降
- Claude Code 2.1.293 / Codex CLI 0.160.1 で確認（Claude Code 2.1.219 以前は、ターン完了の記録がないため使えません）
- macOS（動作確認済み）。Linux は未確認です。
- ビルド用に Rust 1.85 以降（`cargo`）

## インストール

```sh
git clone <このリポジトリ> agent-relay
cd agent-relay
sh scripts/build.sh      # bin/agent-relay を作ります
herdr plugin link .
```

`~/.config/herdr/config.toml` で、プラグインを開くキーを割り当てます（キーは例です）。

```toml
[[keys.command]]
key = "prefix+h"
type = "plugin_action"
command = "abroller666.agent-relay.open"
description = "hand the last answer to another pane"
```

`herdr server reload-config` を実行すると反映されます。

## 設定

Claude Code や Codex の設定ディレクトリを標準以外の場所にしている場合は、プラグインの設定ディレクトリ（`HERDR_PLUGIN_CONFIG_DIR`）に `config.json` を置いて探索先を指定します。
ポップアップの環境変数はAIの環境と同じとは限らないため、`CLAUDE_CONFIG_DIR` などは読みません。

```json
{
  "claude_roots": ["~/.claude/projects"],
  "codex_roots": ["~/.codex/sessions"],
  "max_payload_bytes": 262144,
  "max_file_bytes": 268435456,
  "max_line_bytes": 8388608,
  "max_candidates": 10000
}
```

どの項目も省略できます。知らない項目があるとエラーになります。

## データの扱い

- 履歴は読むだけで、書き込みません。
- 起動ごとに状態ファイル（回答と指示を含む）をプラグインの状態ディレクトリに作り、ポップアップを閉じると削除します。ファイルは 0600、ディレクトリは 0700 です。異常終了で残ったファイルは、24時間後に次の起動時に削除します。
- 回答や指示をログには出しません。

## 開発

```sh
cargo test
cargo clippy --all-targets -- -D warnings
sh scripts/build.sh   # 変更後に再ビルドすると、次に開いたポップアップから反映される
```

`tests/fixtures/raw/` はテスト用セッションの履歴を `tests/fixtures/sanitize.py` で縮約したものです。

キー操作と一覧の表示は [broadcast-pane](https://github.com/abroller666/broadcast-pane)（MIT）を参考にしています。

## ライセンス

MIT
