# ros-addresslist-updater

`ros-addresslist-updater` 是一个 Rust 命令行工具，用于从 TXT CIDR 列表和 V2Ray/Xray `geoip.dat` 生成 IPv4、IPv6 地址集合，并同步到一个或多个 RouterOS 节点。

## 快速开始

```sh
cargo build --release --locked
./target/release/ros-addresslist-updater -c conf/config.yaml check
```

按[使用指南](docs/usage.md#1-准备配置)创建 `conf/config.yaml`，填写 RouterOS 节点和数据源。配置校验通过后，可执行一个已启用的任务，或运行定时调度器：

```sh
./target/release/ros-addresslist-updater -c conf/config.yaml task cn-v4
./target/release/ros-addresslist-updater -c conf/config.yaml run
```

已有 Linux amd64 可执行文件时，可直接用它运行上述命令。REST 同步需要可访问的 RouterOS HTTP/HTTPS 服务。RSC 导入还需要执行器上的 OpenSSH `ssh` 和 `sftp`，以及已核验的 RouterOS 主机密钥。

## 主要特性

- 支持本地文件、HTTP/HTTPS 来源、多源回退、ETag 和 BLAKE3 缓存。
- 支持共享 GeoIP DAT、多标签提取、混合地址族白黑名单和 CIDR 合并。
- 支持 REST 增量更新、RSC 批量导入、多节点同步和逐任务配置。
- 通过记录数和变动比例限制异常更新，提供 DEBUG 条数统计和按日文件日志。
- 提供定时任务和 Live API，可把临时主机地址转发给 RouterOS 并设置 TTL。

## 文档

- [使用指南](docs/usage.md)涵盖配置、CLI、缓存、名单规则和 Live API。
- [设计说明](docs/Design.md)涵盖数据流、调度和 RouterOS 写入策略。
- [更新历史](docs/history.md)记录面向使用者的行为变化。

## 构建与验证

```sh
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
```
