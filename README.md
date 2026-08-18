# tabia-shogi-server

CSA プロトコルで対局する将棋サーバーです。
[shogi-server](https://github.com/shogi-server/shogi-server) とおおむね同等ですが、開始局面に平手だけでなく互角局面集や駒落ち局面を指定できる点が異なります。

- 実装言語: Rust
- ライセンス: GPL v3
- 状態: 開発初期

## Docker で実行する

```sh
docker build -t tabia-shogi-server .
docker run --rm -p 4081:4081 -v "$PWD/config:/etc/tabia:ro" tabia-shogi-server
```

運用に関わるものはイメージに焼き込みません。設定ファイル、局面集、TLS の証明書と鍵は、いずれもマウントで渡します。`config.toml` に書くパス、つまり `positions` と `[server.tls]` は*コンテナ内の*パスなので、`/etc/tabia/positions.txt` にマウントした局面集なら、`positions` にはそのパスを書きます。
イメージは非 root ユーザーで動き、設定ファイルの既定のパスは `/etc/tabia/config.toml` です。別の場所にマウントするときは、そのパスをコマンドとして渡してください。
