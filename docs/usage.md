# 使用指南

## 1. 准备配置

以下示例从配置目录中的 TXT 文件同步一个 IPv4 测试列表。将其保存为 `conf/config.yaml`，替换节点地址、账号和密码。将测试数据放到 `conf/data/cn-v4.txt`。

```yaml
global-settings:
  first-run: no
  work-dir: /tmp/ros-addresslist-updater
  log-dir: /tmp/ros-addresslist-updater/logs
  log-level: debug
  default-scheduler: "0 0 */2 * * *"
  ros-comment-prefix: "=="
  inline-comment-prefix: "#"
  use-etag: yes
  diff-algo: blake3
  min-record-size: 1
  update-method: api

ros-endpoints:
  - name: host1
    address: 192.0.2.10
    port: 443
    ssl: yes
    enable: yes
    username: updater
    password: "replace-me"
    # ssh-port: 22
    # ssh-user: updater
    # ssh-key: ./keys/updater

task-lists:
  - name: cn-v4
    enable: yes
    type: txt
    ip-version: 4
    source:
      - data/cn-v4.txt
```

```text
192.0.2.0/24
198.51.100.10
```

每个任务的 `name` 同时是 RouterOS 列表名，必须以字母或数字开头，且不能包含 `*`。`enable: no` 会将该任务排除在手动执行、批量执行和调度之外。节点的 `enable` 单独控制是否接收写入。通过 `apply-endpoints: [host1]` 可限制任务目标，省略时选择全部启用节点。

HTTPS 用 `ssl: yes`，程序验证服务端证书。HTTP 测试环境可显式用 `ssl: no` 和对应端口。REST 从 YAML 读取 `username`、`password`。RSC 使用系统 `ssh` 和 `sftp`，设置了 `ssh-key` 时使用密钥，否则使用配置中的密码。SSH 默认端口为 22，默认用户名沿用节点的 `username`。运行程序的本地账号应已有可信的 RouterOS `known_hosts` 条目。

### RouterOS 账户权限

建议为更新器创建专用账号和用户组，根据使用的功能分配权限。

| 用途 | 用户组权限 |
| --- | --- |
| REST 读取与 RSC 导入后的读回 | `read,api,rest-api` |
| REST 更新地址列表和 Live API 转发 | `read,write,api,rest-api` |
| SFTP 上传、清理脚本及 SSH 执行 `/import` | 建议配置 `ssh,ftp,read,write`，并验证目标设备允许该账号上传、导入和删除临时脚本。 |
| 同一账号处理 REST 和 RSC | `read,write,api,rest-api,ssh,ftp` |

例如，用管理员账号在 RouterOS 创建同时支持 REST 和 RSC 的专用账号。请先替换示例密码和 `address` 中的执行器来源地址：

```routeros
/user/group/add name=addresslist-updater policy=read,write,api,rest-api,ssh,ftp
/user/add name=updater group=addresslist-updater address=192.0.2.20/32 password="replace-me"
```

REST 需要对应的 `www-ssl` 或 `www` 服务。SFTP 使用 `ssh` 服务，`ftp` 用户组权限与独立的 FTP 服务开关分开。若配置了独立的 `ssh-user`，REST 账号仍需要读回权限。RouterOS 的 `write` 授权覆盖一般配置写入，请配合账号的 `address` 限制管理来源。 [RouterOS REST API](https://manual.mikrotik.com/docs/developer-guides/rest-api/) · [用户组权限](https://manual.mikrotik.com/docs/cli-reference/user/group/) · [RouterOS SSH](https://manual.mikrotik.com/docs/management-tools/ssh/)

## 2. 校验并执行任务

以下命令假定从项目根目录使用构建产物或同名软链接。默认可查找 `conf/config.yaml`，也可用 `-c` 指定配置。

```sh
./ros-addresslist-updater -c conf/config.yaml check
./ros-addresslist-updater task cn-v4
./ros-addresslist-updater task '*'
./ros-addresslist-updater run
```

| 命令 | 行为 |
| --- | --- |
| `check` | 校验 YAML 语法、配置字段和任务引用，无需连接数据源或 RouterOS。 |
| `task <name>` | 执行一个已启用任务。 |
| `task '*'` | 按配置顺序执行全部启用任务。 |
| `task '*geoip'` | 检查共享 DAT，首次下载、内容变化或上次处理失败时执行启用的 GeoIP 子任务。 |
| `run` | 启动 Cron 调度及已启用的 Live API。 |
| `init` | 初始化全部启用任务，要求 `first-run: yes`。 |

`*` 和 `*geoip` 在 shell 中要加引号。Cron 用含秒的六段表达式和执行器本地时区。TXT 任务可用 `scheduler` 覆盖全局 `default-scheduler`。GeoIP 任务由共享更新器的 Cron 触发。启用任务的 `check-onstart: yes` 会让 `run` 立即检查该任务，默认开启。

## 3. 初始化和常规同步

```sh
./ros-addresslist-updater task cn-v4 --init
./ros-addresslist-updater task '*geoip' --init
./ros-addresslist-updater task '*' --init
./ros-addresslist-updater init
```

**初始化会清空选中列表的全部条目，包括 Live 动态条目，再通过 RSC 全量导入。** 独立 `init` 和 `task '*' --init` 等价，均要求 `first-run: yes`。指定任务和 `*geoip` 的 `--init` 在两种配置值下均可执行。初始化保留条数上下限校验，跳过变动比例限制。GeoIP 即使 DAT 未变也会初始化选中的任务。

常规同步根据 `update-method: api|rsc` 提交。默认 `diff-update: yes` 只同步差异。显式关闭时重建静态集合，常规同步保留 Live 动态条目。同一地址族和列表名下的静态条目由任务独占，手工维护的静态地址应使用单独列表。

## 4. 本地文件、远程来源与 GeoIP

`source` 接受 HTTP/HTTPS URL 和本地文件。相对路径以配置文件所在目录为基准。填写多个候选时，按 `download-order` 顺序或随机尝试，使用第一个通过验证的来源。

```yaml
geoip-updater:
  source:
    - data/geoip.dat
    - https://github.com/v2fly/geoip/releases/latest/download/geoip.dat
  scheduler: "59 59 23 * * *"

task-lists:
  - name: cn-geo-v4
    enable: yes
    type: geoip
    ip-version: 4
    tags: [cn, hk, mo]
  - name: cn-geo-v6
    enable: yes
    type: geoip
    ip-version: 6
    tags: [cn, hk, mo]
```

此片段与前面的 `global-settings`、`ros-endpoints` 一起使用。两个 GeoIP 任务共用同一 DAT，各自输出匹配地址族的标签并集。标签匹配忽略大小写。

## 5. 白黑名单与注释

在任务内添加以下字段。内联和文件可混用，每行一个 IP 或 CIDR，按任务地址族选取。YAML 内联值含 `#` 时要加引号。

```yaml
whitelist:
  - 203.0.113.1
  - '203.0.113.2 # 机房 A'
whitelist-file: white.lst
blacklist:
  - 198.51.100.10
blacklist-file: black.lst
```

```text
203.0.113.2 # 机房 B
2001:db8::10 # IPv6 任务使用
```

白名单补充源集合，已被现有网段覆盖的条目会跳过。黑名单命中时剔除整条重叠 CIDR。白黑名单规则相互重叠时，两侧参与冲突的规则先抵消，并输出警告。

普通 TXT/GeoIP 条目和无备注的白名单条目省略 RouterOS 注释。白名单实际新增的条目有备注时，以上例为 `== 机房 A | 机房 B ==`。同一地址的相同备注只保留一次。

## 6. 文件缓存和更新保护

```sh
./ros-addresslist-updater task cn-v4 --nocache
./ros-addresslist-updater task '*geoip' --nocache
./ros-addresslist-updater init --nocache
```

普通远程检查可用 ETag 发条件请求，304 从本地快照读取。BLAKE3 用于比较内容。即使 TXT 没变，任务仍会应用当前本地白黑名单并读取 RouterOS 列表做差异比较。普通 `task '*geoip'` 对未变且已处理成功的 DAT 跳过子任务。`--nocache` 重新获取完整文件或读取本地原文件，经校验后发布快照并继续处理。GeoIP 子任务也会继续执行。

| 工作目录 | 用途 |
| --- | --- |
| `sources/<任务名>/<来源标识>/` | TXT 源快照，来源标识含主机名或文件名和短哈希。 |
| `sources/_geoip/<来源标识>/` | 共享 DAT 快照。 |
| `tasks/<任务名>/ipv4.txt` 或 `ipv6.txt` | 上次接受的 CIDR 集合，用于比例检查。 |
| `geoip-applied` | 最近成功处理的 DAT 摘要。 |
| `rsc/` | 上传前生成的临时脚本。 |
| `.lock` | 工作目录的进程互斥锁。 |

默认最终条数允许 10 至 10000 条。常规同步根据 `(新增 + 删除) / 旧条数` 检查变动，默认阈值 50%，超过时跳过该任务写入。初次没有基线时使用条数保护。任务字段可覆盖全局的条数限制、变动阈值、下载参数、调度和同步方式。

`log-level: debug` 的 `record counts` 显示源条数、原始白黑名单、完全重复数、网段覆盖与合并减少量，以及实际增删和最终条数。GeoIP 按配置中的 tag 顺序分列。RouterOS 操作计划另列 `add`、`delete` 和 `edit`，可用来判断耗时是否发生在读取列表阶段。

## 7. Live API

在配置顶层加入 `api`，并启动 `run`：

```yaml
api:
  enable: yes
  listen: 127.0.0.1
  port: 8080
  path: /api/v1/live
  network-acl: [127.0.0.1/32]
  address-list-allow: [cn-v4]
  default-ttl: 300
```

```sh
curl -sS -X POST http://127.0.0.1:8080/api/v1/live \
  -H 'Content-Type: application/json' \
  -d '{"list":"cn-v4","address":"203.0.113.10","ttl":60}'
```

`address` 是单个主机地址，必须符合任务地址族。`ttl` 单位为秒，可省略。服务按请求对端 IP 和目标列表放行，直接向目标 RouterOS 转发，返回各节点结果。RouterOS 负责 TTL 到期和重复条目行为。

## 8. 查看帮助

```sh
./ros-addresslist-updater --help
./ros-addresslist-updater task --help
./ros-addresslist-updater init --help
```
