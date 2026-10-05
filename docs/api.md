# pier-controller HTTP API

本文描述 controller 当前实现的 HTTP 接口。HTTP 服务默认监听 `127.0.0.1:8080`，支持直接 HTTP 访问，也可通过 HTTPS 反向代理访问；示例使用 `http://pier.example.com:8080`。远程直连时需将 `http_listen` 配置为服务器可达的监听地址。agent 的控制连接、凭据兑换和日常部署包下载使用独立的 Noise 加密 TCP 连接。

## 请求与响应约定

管理接口使用登录后由服务器设置的 Cookie：HTTP 为 `pier_session`，HTTPS 为 `__Host-pier_session`，不接受管理员 Bearer token。制品下载接口仍使用部署所属 agent 的 Bearer token。

首次打开 `/init` 设置管理员、密码、定义仓库和运行设置，成功后自动登录并立即启用 agent 通信。启动 YAML 只需 `http_listen` 和 `state_dir`；其他设置保存在 SQLite。运行设置以后通过 `/v1/settings` 修改，手动重启后生效；仓库地址和分支独立保存，定义仅手动同步。初始化不需要初始化码，只允许成功一次；重启不会重新开放初始化。后续登录使用 `/login`。

Cookie 均设置 `HttpOnly; SameSite=Strict; Path=/`，HTTPS 额外设置 `Secure`；没有 Domain，有效期固定为 8 小时。Cookie 的名称、签发、读取及清除策略由当前生效的 `public_url` 决定，不使用转发头推断协议。所有管理写请求必须携带与当前生效的 `public_url` 完全一致的 `Origin`，以及当前会话返回的 `X-CSRF-Token`。初始化和登录尚无会话，只校验来源；未迁移旧公开地址的首次初始化要求规范 HTTP 或 HTTPS Origin 与保留端口的 Host 相符，并原子保存该来源。修改公开地址后，重启前仍校验旧来源，重启后校验新来源及相应的 Cookie；HTTP 与 HTTPS 切换后需重新登录。GET 查询无需 CSRF。旧 YAML 运行字段仅迁移一次，此后不覆盖网页配置。禁止跨源访问，浏览器通过同源 HTTP 或 HTTPS 使用页面和 API。

以下 curl 示例使用 Cookie jar，登录示例还需要 `jq`：

```sh
CONTROLLER_URL='http://pier.example.com:8080'
umask 077
COOKIE_JAR=$(mktemp)
# AGENT_ID、DEPLOYMENT_ID 和 ENROLLMENT_ID 使用接口实际返回的值。
# AGENT_TOKEN 仅用于制品下载。
```

有 JSON 请求体时须发送 `Content-Type: application/json`。请求对象拒绝未知字段；绑定和镜像映射拒绝重复键。绑定变量通常为字符串，PATCH 绑定允许用 `null` 移除已保存值。

业务接口成功返回 **HTTP 200**，包括创建部署和终端；退出和修改密码返回 **204**，终端 WebSocket 握手返回 **101**。除制品下载、WebSocket 和 204 外，成功响应为 JSON。响应设置 `Cache-Control: no-store` 与 `X-Content-Type-Options: nosniff`。

时间戳为 UTC Unix 秒数；可空字段返回 `null`。架构为 `amd64` 或 `arm64`，列表接口没有分页参数。示例中的 ID、commit、凭据和摘要需替换为实际值；示例源码地址也需按实际项目配置。

### 错误响应

业务处理器的错误使用以下结构：

```json
{"error":"resource not found"}
```

| 状态码 | 含义与响应 |
| --- | --- |
| `400` | 业务参数无效、同步失败或初始化授权无效，返回 JSON 错误 |
| `401` | 管理接口未登录、凭据错误或会话失效，返回 `{"error":"authentication required"}`；制品接口 token 错误返回 `{"error":"unauthorized"}` |
| `403` | Origin 缺失/错误或 CSRF 错误，返回 `invalid origin` / `invalid csrf token` |
| `404` | agent、绑定、部署、授权记录或制品不存在，返回 JSON 错误 |
| `409` | 当前状态不允许操作，例如目录不可用、agent 离线或已有活动部署，返回 JSON 错误 |
| `429` | 认证限流返回 `too many authentication attempts; retry later`；终端数量超限返回 `terminal limit reached` |
| `500` | 内部操作失败，通常返回 `{"error":"internal operation failed"}`；同步工作线程失败返回 `{"error":"sync worker failed"}` |

JSON 语法、字段类型、Content-Type、查询参数解码或请求体大小等错误由 Axum 拒绝，状态码及响应体采用框架格式，不能假定它们都有 `error` 字段。客户端应先检查状态码和 Content-Type。

## 接口索引

| 方法 | 路径 | 鉴权 | 用途 |
| --- | --- | --- | --- |
| GET | `/v1/auth/status` | 公开 | 是否已初始化 |
| POST | `/v1/auth/init` | 同源 Origin | 首次创建管理员 |
| POST | `/v1/auth/login` | 同源 Origin | 用户名密码登录 |
| GET | `/v1/auth/session` | 管理员 Cookie | 当前会话与 CSRF token |
| POST | `/v1/auth/logout` | 管理员 Cookie | 撤销当前会话 |
| POST | `/v1/auth/password` | 管理员 Cookie | 修改密码并撤销全部会话 |
| GET | `/v1/settings` | 管理员 Cookie | 当前与已保存运行设置、待重启状态、agent 监听状态 |
| PUT | `/v1/settings` | 管理员 Cookie | 保存运行设置，手动重启后生效 |
| PATCH | `/v1/agents/{id}/bindings/{blueprint_id}` | 管理员 Cookie | 增量修改指定蓝图变量 |
| GET | `/v1/repository` | 管理员 Cookie | 仓库配置、commit 和同步状态 |
| PUT | `/v1/repository` | 管理员 Cookie | 保存仓库配置，不执行同步 |
| POST | `/v1/repository/sync` | 管理员 Cookie | 立即同步并校验定义仓库 |
| GET | `/v1/apps` | 管理员 Cookie | app 声明及变量 |
| GET | `/v1/blueprints` | 管理员 Cookie | blueprint 与 app 声明 |
| POST | `/v1/agents` | 管理员 Cookie | 手动注册 agent |
| GET | `/v1/agents` | 管理员 Cookie | agent 列表 |
| PUT | `/v1/agents/{id}/connection` | 管理员 Cookie | 修改被动 agent 的可达地址、SOCKS5 代理并重连 |
| GET | `/v1/agents/{id}` | 管理员 Cookie | agent 状态 |
| POST | `/v1/agents/{id}/apps/{instance}/terminals` | 管理员 Cookie、Origin、CSRF | 创建应用终端连接资格 |
| GET | `/v1/terminals/{id}/ws` | 创建者 Cookie、同源 Origin | 一次性 WebSocket 连接 |
| POST | `/v1/agents/{id}/bindings` | 管理员 Cookie | 新增蓝图绑定 |
| GET | `/v1/agents/{id}/bindings` | 管理员 Cookie | 绑定列表 |
| GET / PUT | `/v1/agents/{id}/bindings/{blueprint_id}` | 管理员 Cookie | 查询绑定 / 替换变量 |
| DELETE | `/v1/agents/{id}/bindings/{blueprint_id}` | 管理员 Cookie | 删除未部署或已停止的绑定 |
| POST | `/v1/deployments` | 管理员 Cookie | 创建异步部署 |
| GET | `/v1/deployments` | 管理员 Cookie | 部署列表 |
| GET | `/v1/deployments/{id}` | 管理员 Cookie | 部署详情 |
| POST | `/v1/enrollments` | 管理员 Cookie | 批准交互初始化 |
| GET | `/v1/enrollments/{id}` | 管理员 Cookie | 初始化授权状态 |
| GET | `/v1/artifacts/{deployment}/{app}` | 目标 agent | 下载部署包 |

以下各节列出业务错误；公共鉴权错误和内部错误见上表。

## 应用 Bash 终端

### POST /v1/agents/{id}/apps/{instance}/terminals

`instance` 使用 `report.apps[].instance`，不是应用显示名。请求体为 `{"cols":100,"rows":30}`，行列数均须为 1–500。agent 必须在线且声明 `report.capabilities` 包含 `app_terminal_v1`，旧 agent 可省略此字段。账户由 agent 根据实际安装状态验证，不接受指定用户名、命令、目录或环境变量。

```json
{
  "id": "terminal-uuid",
  "websocket_url": "/v1/terminals/terminal-uuid/ws",
  "expires_at": 1790000030
}
```

连接资格绑定创建时的 Web 登录会话、agent 控制连接和 app 实例，最长 30 秒有效，仅可使用一次；创建资格不会启动 Bash。每个 agent 最多 8 个终端，controller 最多 64 个，待连接资格也计入限额。应用不存在返回 `404`；离线、能力不支持或正在部署/升级返回 `409`；超限返回 `429`。

### GET /v1/terminals/{id}/ws

浏览器将相对地址转为同源 WebSocket URL，HTTP 使用 `ws://`、HTTPS 使用 `wss://`，Cookie 自动携带，握手必须提供匹配公开地址的 `Origin`。此 GET 不需要 CSRF header，使用前一步经过 CSRF 校验的一次性资格；其他登录会话不能使用它。过期或重复连接返回 `409`，不同拥有者返回 `404`。握手后 agent 连接和 Bash 初始化分别有 10 秒超时。

| 方向 | 帧 | 内容 |
| --- | --- | --- |
| 服务端 → 浏览器 | JSON 文本 | `{"type":"ready","user":"Web.Site","home":"/var/lib/pier-agent/blueprints/.../data"}`，`user` 为 `pier-blueprint.yml` 的 `name`，收到后才可输入 |
| 浏览器 → 服务端 | 二进制 | 原始输入字节，UTF-8 文本或 Ctrl+C 等控制字符，每帧 1–32768 字节 |
| 服务端 → 浏览器 | 二进制 | 原始 PTY 输出，每帧最多 32768 字节；交给终端模拟器，不按帧独立解码 UTF-8 |
| 浏览器 → 服务端 | JSON 文本 | `{"type":"resize","cols":120,"rows":40}` |
| 浏览器 → 服务端 | JSON 文本 | `{"type":"ack","bytes":1024}`，确认终端完成渲染的输出字节数，不能确认未收到的数据 |
| 服务端 → 浏览器 | JSON 文本 | `{"type":"exit","code":0,"reason":"shell_exited"}`，`code` 可为空或省略 |

控制文本最多 4096 字节；未确认输出最多 256 KiB，达到窗口后暂停读取 PTY。标准 WebSocket Ping/Pong 每 15 秒检查连接，45 秒未响应关闭。终端独立使用现有 agent TCP 端口上的 Noise 连接，不占用控制通道的数据队列。

Bash 使用 app 的 UID/GID，以 `bash -il` 进入账户主目录，提供基础 `HOME/USER/LOGNAME/PATH/SHELL/TERM/LANG` 环境；不继承 agent 或应用服务的变量，Bash 自身仍读取正常的登录启动文件。应用自动重启保留终端；重新部署、回滚、agent 升级/停止、控制连接断开、关闭网页或 Web 登录失效都会结束会话。重连必须重新 POST 创建新 Bash。

退出原因包括 `shell_exited`、`deployment_started`、`agent_upgrading`、`agent_stopping`、`terminal_unavailable`、`connection_lost`、`login_expired`、`session_revoked_or_agent_disconnected` 和 `connection_closed`。关闭通知可重复，客户端应幂等处理。服务端只记录会话元数据与原因，不保存输入输出；Bash 历史由用户配置决定。

## 账号与会话

### GET /v1/auth/status

公开查询，两个布尔字段分别表示已有管理员和已保存仓库配置。未初始化时返回可用于填充表单的默认值：

```json
{
  "initialized": false,
  "repository_configured": false,
  "setup_defaults": {
    "tcp_listen": "0.0.0.0:7443",
    "public_url": "",
    "agent_endpoint": "",
    "max_concurrent_builds": 2,
    "proxy_configured": false
  }
}
```

已初始化时 `setup_defaults` 为 `null`。默认值可包含从旧 YAML 导入的运行值；代理只返回是否已配置，不返回地址或凭据。此接口不返回账号或仓库地址。

### POST /v1/auth/init

首次创建管理员：

```json
{
  "username": "admin",
  "password": "replace-with-your-password",
  "repository": {"url": "https://git.example.com/services.git", "reference": "main"},
  "settings": {
    "tcp_listen": "0.0.0.0:7443",
    "public_url": "http://pier.example.com:8080",
    "agent_endpoint": "pier.example.com:7443",
    "max_concurrent_builds": 2,
    "build_proxy": {}
  }
}
```

`reference` 默认 `main`；`repository` 在已导入旧 YAML 仓库配置时可省略，否则必填。`settings` 及其字段可省略，保留默认值或迁移值；字段格式见下文运行设置。未指定公开地址时采用当前 HTTP 或 HTTPS Origin，提交值必须与该来源相同。未指定 agent 公布地址时采用该来源主机及 `tcp_listen` 端口。用户名为 1–64 字节，无首尾空白和控制字符；密码至少 12 个字符、最多 1024 字节，不裁剪空白。确认密码由页面校验，不是 API 字段。密码使用随机盐 Argon2id 哈希保存。

需携带 `Origin`。未迁移旧公开地址时还要求 Host 与 HTTP 或 HTTPS Origin 匹配（含非默认端口），反向代理使用 `proxy_set_header Host $http_host`。先绑定 agent 监听端口，再将管理员、会话、仓库及运行设置在同一事务中保存，成功后立即启用监听，无需重启。端口不可用返回 `409 agent listener unavailable; check its address, port and permissions`，不留下部分初始化数据，可修正参数重试。此请求不访问 Git，成功后需手动同步。成功设置 Cookie，返回与登录相同的会话结构；重复或并发初始化只有一个请求成功，其余返回 `409 already initialized`。未知字段或格式错误由请求解析器拒绝。

### POST /v1/auth/login

请求仅含 `username`、`password`，不接受 `repository` 或 `settings`。用户名或密码不匹配返回 `401`；尚未初始化返回 `409 initialization required`。初始化、登录、改密合计每个 controller 每分钟最多接受 60 次密码运算请求，同时最多运行两次密码运算；超过限制返回 `429`。

将凭据写入权限为 `0600` 的 `login.json`，不要提交到仓库：

```json
{"username":"admin","password":"replace-with-your-password"}
```

```sh
curl --fail-with-body -X POST "$CONTROLLER_URL/v1/auth/login" \
  --cookie "$COOKIE_JAR" --cookie-jar "$COOKIE_JAR" \
  -H "Origin: $CONTROLLER_URL" -H 'Content-Type: application/json' \
  --data @login.json --output session.json
CSRF_TOKEN=$(jq -r .csrf_token session.json)
```

首次初始化时将路径改为 `/v1/auth/init`，并在 JSON 中增加仓库配置（已有旧 YAML 配置时可省略）。成功响应如下；会话凭据只出现在 Set-Cookie，不放入 JSON：

```json
{"username":"admin","expires_at":1790496000,"csrf_token":"本次会话的随机CSRF值"}
```

登录会为当前浏览器创建新会话并撤销该浏览器此前的会话；其他浏览器的会话继续有效。会话固定 8 小时，不因请求自动续期。脚本应保留 Cookie jar，并在写操作传入对应的 CSRF token。

### GET /v1/auth/session

使用 Cookie 查询当前会话，响应结构与登录相同，可用于页面刷新或脚本恢复 CSRF token。过期、退出、被改密撤销或不存在的会话返回 `401`。

```sh
curl --fail-with-body "$CONTROLLER_URL/v1/auth/session" --cookie "$COOKIE_JAR"
```

### POST /v1/auth/logout

无请求体，要求 Cookie、Origin 和 CSRF。成功删除服务器会话，返回 `204`，同时以 `Max-Age=0` 清除 Cookie。

```sh
curl --fail-with-body -X POST "$CONTROLLER_URL/v1/auth/logout" \
  --cookie "$COOKIE_JAR" --cookie-jar "$COOKIE_JAR" \
  -H "Origin: $CONTROLLER_URL" -H "X-CSRF-Token: $CSRF_TOKEN"
```

### POST /v1/auth/password

要求 Cookie、Origin 和 CSRF，请求字段均必填：

```json
{"current_password":"replace-with-your-password","new_password":"replace-with-a-new-password"}
```

新密码规则与初始化相同。当前密码错误返回 `401`。成功返回 `204` 并清除当前 Cookie，全部已有会话立即失效，使用新密码重新登录；用户名不变。请求方式与其他 JSON POST 一致。

## Controller 运行设置

### GET /v1/settings

要求管理员 Cookie，返回：

```json
{
  "active": {
    "tcp_listen": "0.0.0.0:7443",
    "public_url": "http://pier.example.com:8080",
    "agent_endpoint": "pier.example.com:7443",
    "max_concurrent_builds": 2,
    "build_proxy": {"http_proxy": null, "https_proxy": null, "no_proxy": null}
  },
  "saved": {
    "tcp_listen": "0.0.0.0:7443",
    "public_url": "http://pier.example.com:8080",
    "agent_endpoint": "pier.example.com:7443",
    "max_concurrent_builds": 4,
    "build_proxy": {"http_proxy": null, "https_proxy": null, "no_proxy": null}
  },
  "restart_required": true,
  "agent_listener": {"listening": true, "error": null}
}
```

`active` 是当前进程使用的配置，`saved` 是下次启动读取的配置。两者不同时 `restart_required` 为 `true`。代理 URL 可能含凭据，本接口仅向管理员返回；网页只显示配置状态，不回填已有凭据。`agent_listener` 单独反映实际监听结果，配置已生效不代表端口绑定成功。重启时端口被占用会保留 Web，`listening` 为 `false`、`error` 为说明文字；接入授权和新部署返回 `409`，管理员仍可修改设置。

### PUT /v1/settings

要求 Cookie、当前生效来源的 Origin 和 CSRF。可只提供需修改的字段：

```json
{
  "tcp_listen": "0.0.0.0:8443",
  "agent_endpoint": "pier.example.com:8443",
  "max_concurrent_builds": 4,
  "build_proxy": {
    "http_proxy": "http://proxy.example.com:7890",
    "https_proxy": "http://proxy.example.com:7890",
    "no_proxy": "localhost,127.0.0.1,.internal"
  }
}
```

| 字段 | 约束 |
| --- | --- |
| `tcp_listen` | IP 与端口，IPv6 使用 `[::]:7443`；端口为 1–65535 |
| `public_url` | 规范 HTTP 或 HTTPS 来源，不能包含路径、查询、fragment 或末尾斜杠 |
| `agent_endpoint` | 非空 `host:port`；IPv6 主机用方括号包裹 |
| `max_concurrent_builds` | 整数 1–64 |
| `build_proxy` | 代理对象；HTTP/HTTPS URL 最多 4096 字节，使用 `http://` 或 `https://`，不可带路径、查询、fragment 或控制字符；`no_proxy` 最多 4096 字节，无控制字符 |

省略字段保留已保存值；省略 `build_proxy` 保留全部代理设置，传 `{}` 清除代理，提供代理对象则整体替换，内部省略或 `null` 表示未配置。定义仓库同步与 app 构建共用当前生效的 `build_proxy`；仓库同步直接使用此配置，app 仍由其 `proxy.enabled` 决定是否使用代理。Docker 拉取镜像仍由 Docker daemon 配置代理。

校验全部通过后原子保存，返回与 GET 相同的结构；业务参数无效返回 `400`，类型或格式错误由 Axum 拒绝，失败不修改任何设置。保存时不尝试重新绑定端口，不改变当前来源校验、接入地址或构建参数，不同步 Git、不自动重启。执行 `sudo systemctl restart pier-controller` 后使用新值；监听失败时修正后再次重启。调整 `public_url` 后重启前仍使用旧来源，重启后改用新来源；调整 `agent_endpoint` 不会自动更新已有 agent 本地配置。

## 仓库与定义目录

controller 仅在调用手动同步接口时从 Git 仓库及引用同步定义；初始化、保存配置、启动和重启均不触发同步。`apps` 和 `blueprints` 都是以定义所在目录相对仓库根路径为键的对象；根目录自身的 ID 是 `.`。app 来源为 `pier-pkg.yml`，blueprint 来源为 `pier-blueprint.yml`。

### GET /v1/repository

无参数，返回仓库地址、分支、最近成功目录 commit、同步错误及待同步标记：

```sh
curl --fail-with-body "$CONTROLLER_URL/v1/repository" \
  --cookie "$COOKIE_JAR" \
  -H "Origin: $CONTROLLER_URL" -H "X-CSRF-Token: $CSRF_TOKEN"
```

```json
{
  "repository": {"url":"https://git.example.com/services.git","reference":"main"},
  "commit": "0123456789abcdef0123456789abcdef01234567",
  "error": null,
  "needs_sync": false
}
```

`commit` 在尚无可用目录时为 `null`。同步失败保留上次可用目录，并在 `error` 返回错误字符串；因此非空 `commit` 和非空 `error` 可以同时出现。这个查询本身仍返回 `200`。

`repository` 未配置时为 `null`；`needs_sync` 表示当前设置没有对应的成功目录。更改仓库后，`commit` 可能仍是旧设置的成功快照，不能据此创建新部署；直到当前设置手动同步成功后才允许部署。相同设置同步失败不会使之前成功的目录失效。

### PUT /v1/repository

保存仓库地址和分支，只校验格式，不访问网络、不拉取 Git。请求：

```json
{"url":"https://git.example.com/services.git","reference":"main"}
```

`reference` 省略时为 `main`。URL 和引用均不得为空、以 `-` 开头或包含首尾空白、控制字符；长度分别不超过 4096 和 256 字节。支持 Git 现有 HTTPS、SSH 和本地路径格式，建议本地路径使用绝对路径。未知字段或格式错误被拒绝。成功返回与 GET 相同的结构；更改地址或分支后通常 `needs_sync=true`，保存相同值不使成功目录失效。Cookie、Origin、CSRF 为必需。

```sh
curl --fail-with-body -X PUT "$CONTROLLER_URL/v1/repository" \
  --cookie "$COOKIE_JAR" -H "Origin: $CONTROLLER_URL" \
  -H "X-CSRF-Token: $CSRF_TOKEN" -H 'Content-Type: application/json' \
  --data '{"url":"https://git.example.com/services.git","reference":"main"}'
```

仓库保存与同步请求串行处理；失败不会部分覆盖已有配置。运行中的部署保留自己的定义快照，新配置不会自动应用到服务器。

### POST /v1/repository/sync

无请求体，等待同步和目录校验完成后返回当前 commit：

HTTP 仓库使用当前生效的 `build_proxy.http_proxy`，HTTPS 仓库使用 `build_proxy.https_proxy`；代理地址均可为 `http://` 或 `https://`，可带用户名和密码。两字段不互相回退，`no_proxy` 指定按 Git/libcurl 规则直连的主机。未配置对应代理时明确直连，不继承环境变量或系统、用户 Git 配置中的 HTTP 代理。代理失败不回退直连，TLS 证书仍正常校验；SSH 和本地路径继续使用原有 Git/SSH 连接方式。

保存或清除代理配置后重启 controller 生效；同步开始时使用当前运行配置的快照。代理变更不触发自动同步、不使已有目录失效，代理凭据不会写入定义快照的 Git 配置或同步错误信息。

```sh
curl --fail-with-body -X POST "$CONTROLLER_URL/v1/repository/sync" \
  --cookie "$COOKIE_JAR" \
  -H "Origin: $CONTROLLER_URL" -H "X-CSRF-Token: $CSRF_TOKEN"
```

```json
{"commit":"0123456789abcdef0123456789abcdef01234567"}
```

同步或目录校验失败返回 `400`，错误为 `repository sync or catalog validation failed; previous catalog retained`。失败不替换上次可用目录；同步成功也不自动触发部署。

### GET /v1/apps

无参数，返回当前 commit 和全部 app 声明；目录不可用时返回 `409`。

```sh
curl --fail-with-body "$CONTROLLER_URL/v1/apps" \
  --cookie "$COOKIE_JAR" \
  -H "Origin: $CONTROLLER_URL" -H "X-CSRF-Token: $CSRF_TOKEN"
```

```json
{
  "commit": "0123456789abcdef0123456789abcdef01234567",
  "apps": {
    "examples/recipes/rust/1.0.0": {
      "name": "demo-rust",
      "version": "{{ VERSION }}",
      "variables": {
        "DB_HOST": {"default": null},
        "PORT": {"default": "8080"},
        "VERSION": {"default": "1.0.0"}
      },
      "source": "git"
    }
  }
}
```

示例只展示一个 app 条目。app 元数据包含 `name`、`version`、`variables` 和 `source`；`source` 为 `git` 或 `binary`。名称和版本是未渲染声明，可能含模板。每个变量的 `default` 为字符串或 `null`，`null` 表示必填，不等同于空字符串默认值。

### GET /v1/blueprints

无参数，返回 `commit`、全部 `blueprints` 及全部 `apps`；`apps` 结构与上一接口一致，供客户端同时展示 app 的全部变量声明。目录不可用时返回 `409`。

```sh
curl --fail-with-body "$CONTROLLER_URL/v1/blueprints" \
  --cookie "$COOKIE_JAR" \
  -H "Origin: $CONTROLLER_URL" -H "X-CSRF-Token: $CSRF_TOKEN"
```

以下响应展示一个 blueprint 及其引用的两个 app 条目：

```json
{
  "commit": "0123456789abcdef0123456789abcdef01234567",
  "blueprints": {
    "examples/blueprints/web": {
      "schema": 1,
      "name": "web-server",
      "variables": {
        "DB_HOST": {"default": null},
        "API_PORT": {"default": "8080"},
        "WORKER_PORT": {"default": "8081"}
      },
      "apps": [
        {
          "id": "api",
          "app": "examples/recipes/rust/1.0.0",
          "variables": {"DB_HOST": "{{ DB_HOST }}", "PORT": "{{ API_PORT }}"}
        },
        {
          "id": "worker",
          "app": "examples/recipes/go/1.0.0",
          "variables": {"DB_HOST": "{{ DB_HOST }}", "PORT": "{{ WORKER_PORT }}"}
        }
      ]
    }
  },
  "apps": {
    "examples/recipes/rust/1.0.0": {
      "name": "demo-rust",
      "version": "{{ VERSION }}",
      "variables": {
        "DB_HOST": {"default": null},
        "PORT": {"default": "8080"},
        "VERSION": {"default": "1.0.0"}
      },
      "source": "git"
    },
    "examples/recipes/go/1.0.0": {
      "name": "demo-go",
      "version": "{{ VERSION }}",
      "variables": {
        "DB_HOST": {"default": null},
        "PORT": {"default": "8080"},
        "VERSION": {"default": "1.0.0"}
      },
      "source": "git"
    }
  }
}
```

blueprint 的 `apps` 为有序列表。`apps[].id` 是 blueprint 内唯一的实例 ID，`apps[].app` 是 app 定义的目录 ID；同一个定义可以被多个实例引用。`apps[].variables` 的键是 app 变量名，值是使用 blueprint 变量的模板字符串；没有映射的 app 变量使用 app 自身默认值，必填 app 变量须在定义中映射。

## Agent

### POST /v1/agents

手动创建 agent 身份，适用于自行管理 agent 配置的客户端。请求唯一字段 `name` 必填，字符串长度为 1–256 字节。

```sh
curl --fail-with-body -X POST "$CONTROLLER_URL/v1/agents" \
  --cookie "$COOKIE_JAR" \
  -H "Origin: $CONTROLLER_URL" -H "X-CSRF-Token: $CSRF_TOKEN" \
  -H 'Content-Type: application/json' \
  --data '{"name":"web-01"}'
```

```json
{
  "id": "37db8dee-0a89-4eec-b37d-d4461cb2d1db",
  "token": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
}
```

`id` 是 agent UUID，`token` 是 64 位十六进制长期凭据，仅在此注册响应中返回；后续 agent 查询不返回凭据或摘要。保存返回值并配置 agent 的 `agent_id`、`token_file` 和 `controller_tcp`。注册不会启动进程，重复 POST 会创建不同身份。名称长度无效返回 `400`。

交互式 `pier-agent init` 使用后文的初始化授权流程，无需先调用本接口。

### GET /v1/agents

无参数，返回 `{"agents":[Agent]}`；无记录时数组为空。

```sh
curl --fail-with-body "$CONTROLLER_URL/v1/agents" \
  --cookie "$COOKIE_JAR" \
  -H "Origin: $CONTROLLER_URL" -H "X-CSRF-Token: $CSRF_TOKEN"
```

```json
{
  "agents": [
    {
      "id": "37db8dee-0a89-4eec-b37d-d4461cb2d1db",
      "name": "web-01",
      "info": null,
      "last_seen": null,
      "report": {"deployment_id": null, "apps": [], "result": null},
      "online": false
    }
  ]
}
```

### GET /v1/agents/{id}

`id` 为注册或初始化获得的 agent ID。返回单个 Agent 对象（与列表中的元素一致，没有 `agent` 外层），不存在时返回 `404`。

```sh
curl --fail-with-body "$CONTROLLER_URL/v1/agents/$AGENT_ID" \
  --cookie "$COOKIE_JAR" \
  -H "Origin: $CONTROLLER_URL" -H "X-CSRF-Token: $CSRF_TOKEN"
```

### Agent 响应字段

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `id`、`name` | string | agent ID 和注册名称 |
| `info` | object / null | 主机信息；手动注册且尚未连接时为 `null` |
| `info.architecture` | string | `amd64` 或 `arm64` |
| `info.hostname`、`info.os_release` | string | 主机名和系统发行信息文本 |
| `last_seen` | integer / null | 最近连接或状态上报时间；尚未连接时为 `null` |
| `online` | boolean | controller 当前是否持有该 agent 的控制会话 |
| `report.deployment_id` | string / null | agent 上报的当前部署 ID |
| `report.apps` | array | app 进程状态列表，字段见下表 |
| `report.result` | object / null | 最近部署结果，包含 `id`、`state`、`error` |

`report.result.id` 是部署 ID，`state` 是部署结果状态，`error` 是字符串或 `null`。离线时 `info`、`report` 和 `last_seen` 仍可保留，不能把历史报告当作实时状态。

| `report.apps[]` 字段 | 类型 | 说明 |
| --- | --- | --- |
| `instance` | string | 由 blueprint 路径与实例 ID 计算的实例身份摘要 |
| `id` | string | blueprint 中的 app 实例 ID，例如 `api` |
| `state` | string | 进程状态：`starting`、`running`、`failed`、`backoff` 或 `stopped` |
| `pid` | integer / null | 当前进程 PID |
| `restarts` | integer | 自动重启计数 |
| `exit_code` | integer / null | 记录的进程退出码；无可用退出码时为 `null` |

### Agent 软件版本与自动升级

`GET /v1/agents` 的每个元素及 `GET /v1/agents/{id}` 增加以下字段。旧 agent 的 `software` 为 `null`，仍可连接和部署，需要手动安装一次带更新器的原生包并重启。

| 字段 | 说明 |
| --- | --- |
| `software.version` | 当前进程的 Cargo 版本，例如 `0.1.0` |
| `software.package` | 已安装原生包 `{ "version": "0.1.0", "revision": 2 }`，无法识别时为 `null` |
| `software.system` / `format` | `ubuntu24.04` / `deb`、`almalinux8` / `rpm` 或 `almalinux9` / `rpm`，未识别时为 `null` |
| `software.supported` / `reason` | 是否支持自动升级及不可用原因 |
| `upgrade.target` | 当前 controller 内置的匹配发行包，或 `null` |
| `upgrade.status` | 最近一次升级记录，或 `null` |
| `upgrade.reason` | 未提供升级的原因，正常时为 `null` |

`software.reason` 由 agent 在启动时上报具体检查失败原因，包括系统识别、原生包查询/安装状态/版本/架构、可执行文件路径、systemd MainPID，以及升级目录或记录问题。`software.supported=false` 时，`upgrade.reason` 保留该原因；旧 agent 未提供原因时返回手动更新提示。诊断不包含命令原始输出，修复后需重启 agent 重新检测。服务器列表和详情页展示当前原因，历史升级状态不会将其隐藏。

发行版后缀仅用于原生包元数据和文件名，`software.package` 仍使用基础版本与数字修订号。旧版 Ubuntu agent 需要手动安装一次带 `.ubuntu24.04` 的新包并重启，之后恢复自动升级。

发行包对象含 `package`（版本与修订号）、`system`、`format`、`architecture`（`amd64` / `arm64`）、`sha256`、`size`（字节数）。状态对象含 `release`（发行包）、`phase`、`error`（可空的简短原因）、`updated_at`（Unix 秒）。HTTP 不暴露升级授权凭据，也不提供手动上传或触发升级接口。

| `phase` | 说明 |
| --- | --- |
| `downloading` | 下载中，网络中断后退避重试 |
| `waiting` | 包已校验，等待活动部署结束 |
| `installing` | 已预留升级，安装本地原生包 |
| `restarting` | 重启 agent，等待本地服务恢复及 systemd 就绪 |
| `succeeded` | 升级成功或已手动恢复到目标及更新版本 |
| `failed` | 已暂停该目标版本，需要手动恢复 |

升级通过已有端口上的独立 Noise 认证连接完成。只升级更高版本或修订号，不自动降级。controller 与 agent 双方检查部署空闲；`installing` / `restarting` 期间创建部署返回 `409`。安装/启动失败不自动回滚，事务与记录持久化；controller 重启保留升级预留，超过 45 分钟未完成则标记失败并解除预留。新进程就绪的本地确认不依赖 controller 在线。重启 agent 会短暂中断其管理的 app，保留身份、配置和部署数据。

## Blueprint 绑定

一个 agent 可绑定多个不同蓝图，每份蓝图在该 agent 上只绑定一次。旧的单绑定 `/binding` 接口已替换为以下集合接口；旧配置需按新接口重新绑定。绑定身份 `blueprint_id` 为仓库相对路径的 SHA-256，而 Linux 用户和组使用蓝图的 `name`。绑定不自动部署，变量按绑定独立保存，不返回变量值。

### POST /v1/agents/{id}/bindings

新增绑定，请求示例：

```json
{"blueprint":"examples/blueprints/web","variables":{"DB_HOST":"db.internal","API_PORT":"8080","WORKER_PORT":"8081"}}
```

`blueprint` 必填，必须存在于当前目录；`variables` 默认为 `{}`，未知变量和缺少必填变量返回 `400`。蓝图名称必须符合系统账户规则，且不得与同机其他蓝图重名；校验失败保留已有绑定。重复绑定同一路径返回 `409`。

响应为 `{"agent_id":"...","id":"<blueprint_id>","blueprint":"examples/blueprints/web","variable_names":["API_PORT","DB_HOST","WORKER_PORT"]}`。`variable_names` 仅包含显式保存的变量名。

### GET /v1/agents/{id}/bindings

返回 `{"bindings":[<绑定概要>, ...]}`，尚无绑定时数组为空；agent 不存在返回 `404`。

### GET /v1/agents/{id}/bindings/{blueprint_id}

返回单个绑定概要，未找到返回 `404`。

### PUT /v1/agents/{id}/bindings/{blueprint_id}

请求格式与新增绑定相同，但仅替换指定绑定的全部变量。`blueprint` 必须仍对应 URL 中的身份，不能把现有绑定改成另一个蓝图。未找到返回 `404`，身份不匹配返回 `409`。

### PATCH /v1/agents/{id}/bindings/{blueprint_id}

```json
{"blueprint":"examples/blueprints/web","variables":{"API_PORT":"9090","WORKER_PORT":null}}
```

`variables` 必填：省略的键保留原值，字符串覆盖原值（包括空字符串），`null` 移除显式值并恢复默认值。蓝图路径不匹配返回 `409`；未知变量、删除无默认值的必填变量或渲染失败返回 `400`，整个修改不生效。PUT/PATCH 均返回绑定概要。目标蓝图有活动任务时禁止编辑。

### DELETE /v1/agents/{id}/bindings/{blueprint_id}

agent 必须在线、支持 `multi_blueprint_v1` 且不在升级。目标蓝图没有活动任务，并且尚未部署或已经 `stopped` 时，删除绑定并返回 `{"removed":true}`；否则返回 `409`，应先创建停止任务并等待成功上报。此操作不停止进程，不删除账户、数据或历史版本。重新绑定相同蓝图可复用保留的账户和数据。

## 部署

### POST /v1/deployments

为在线 agent 的指定绑定蓝图创建后台任务，要求 agent 上报 `multi_blueprint_v1`。任务仅影响目标蓝图。

| 请求字段 | 类型 | 必填 / 默认值 |
| --- | --- | --- |
| `agent_id` | string | 必填，目标 agent ID |
| `blueprint` | string | 必填，目标绑定的蓝图路径 |
| `action` | string | 默认 `deploy`；也可为 `stop` |
| `commit` | string | `deploy` 时必填，必须等于当前目录 commit；`stop` 时省略 |
| `images` | object，string → string | 可省略，默认 `{}`；每个源码 app 必须提供构建镜像 |

先读取 `/v1/blueprints` 或 `/v1/repository` 获得当前 commit，再将请求保存为 `deployment.json`：

```json
{
  "agent_id": "37db8dee-0a89-4eec-b37d-d4461cb2d1db",
  "blueprint": "examples/blueprints/web",
  "commit": "0123456789abcdef0123456789abcdef01234567",
  "images": {
    "api": "pier-builder-rust:almalinux8",
    "worker": "pier-builder-go:almalinux8"
  }
}
```

```sh
curl --fail-with-body -X POST "$CONTROLLER_URL/v1/deployments" \
  --cookie "$COOKIE_JAR" \
  -H "Origin: $CONTROLLER_URL" -H "X-CSRF-Token: $CSRF_TOKEN" \
  -H 'Content-Type: application/json' --data @deployment.json
```

```json
{"id":"61ca2f29-97a2-4329-9737-8b4834e674d7","state":"building"}
```

`images` 按 blueprint 的 **app 实例 ID** 指定，不使用 app 目录路径或实例摘要。每个 `source: git` app 必须有镜像，`source: binary` app 禁止提供镜像，未知实例也会被拒绝。架构从在线 agent 的信息读取，无请求架构字段；调用方负责选择兼容目标系统的镜像，controller 不根据发行版替换镜像。

每次部署捕获固定的定义 commit、绑定变量、镜像参数和当前生效的构建设置。每个 agent 同时只允许一个未完成任务；默认最多同时处理两个 agent 的构建任务，可通过 `/v1/settings` 的 `max_concurrent_builds` 调整并重启生效。`200` 表示任务已创建，实际结果须查询部署详情；全部包构建成功后才下发 agent，后台校验或构建失败不会修改服务器上的服务。

| 状态码 | 条件 |
| --- | --- |
| `400` | 绑定变量解析失败、未知镜像实例、源码 app 缺镜像或二进制 app 指定了镜像 |
| `404` | agent 不存在 |
| `409` | 仓库待同步、agent 离线、架构未知、已有活动部署、目录不可用、commit 已过期、未绑定 blueprint 或绑定的 blueprint 已不在当前目录 |

commit 过期时重新读取目录、核对定义后再创建任务。此接口没有幂等键；响应丢失时先查询该 agent 的部署列表，确认已有任务。

停止已部署蓝图使用同一接口：

```json
{"agent_id":"37db8dee-0a89-4eec-b37d-d4461cb2d1db","blueprint":"examples/blueprints/web","action":"stop"}
```

停止请求不携带 `commit` 或构建镜像；任务直接进入 `ready`，仅停止目标蓝图并关闭其终端，保留绑定与数据。即使仓库待同步或蓝图定义已从 Git 删除，也可停止已安装蓝图。等待任务成功且蓝图状态上报 `stopped` 后才可解除绑定。

### GET /v1/deployments

唯一使用的查询参数 `agent_id` 可选，按 agent ID 精确筛选；省略时返回全部任务，无匹配时数组为空。

```sh
curl --fail-with-body --get "$CONTROLLER_URL/v1/deployments" \
  --cookie "$COOKIE_JAR" \
  -H "Origin: $CONTROLLER_URL" -H "X-CSRF-Token: $CSRF_TOKEN" \
  --data-urlencode "agent_id=$AGENT_ID"
```

```json
{
  "deployments": [
    {
      "id": "61ca2f29-97a2-4329-9737-8b4834e674d7",
      "agent_id": "37db8dee-0a89-4eec-b37d-d4461cb2d1db",
      "blueprint": "examples/blueprints/web",
      "commit": "0123456789abcdef0123456789abcdef01234567",
      "state": "building",
      "error": null,
      "created_at": 1790467200,
      "plan": null
    }
  ]
}
```

### GET /v1/deployments/{id}

`id` 为部署 ID。返回单个部署对象（与列表元素一致，没有 `deployment` 外层）；不存在时返回 `404`。

```sh
curl --fail-with-body "$CONTROLLER_URL/v1/deployments/$DEPLOYMENT_ID" \
  --cookie "$COOKIE_JAR" \
  -H "Origin: $CONTROLLER_URL" -H "X-CSRF-Token: $CSRF_TOKEN"
```

响应包含 `id`、`action`、`agent_id`、`blueprint`、`commit`、`state`、`error`、`created_at` 和 `plan`。`error` 为错误字符串或 `null`。构建完成前 `plan` 为 `null`，完成后包含：

| `plan` 字段 | 类型 | 说明 |
| --- | --- | --- |
| `id`、`agent_id`、`blueprint`、`commit` | string | 与部署记录对应的固定身份及定义版本 |
| `blueprint_name` | string | 固定的蓝图名称，也是用户和组名 |
| `action` | string | `deploy` 或 `stop` |
| `architecture` | string | `amd64` 或 `arm64` |
| `apps` | array | 按 blueprint 顺序排列的部署包列表 |
| `apps[].instance` | string | 64 位十六进制实例身份摘要 |
| `apps[].id` | string | blueprint 中的 app 实例 ID，也是制品下载的 `{app}` 参数 |
| `apps[].sha256` | string | tar.gz 文件的 64 位十六进制 SHA-256 |
| `apps[].size` | integer | tar.gz 文件字节数 |

响应不包含保存的变量值、token 或 controller 本地制品路径。构建失败通常返回 `package validation or build failed; no server changes applied`；账户命名、归属和冲突错误展示经过限制的具体原因；其他 agent 任务错误转换为 `agent reported deployment failure; inspect local agent logs`。

### 部署状态

正常流程为 `building` → `ready` → `downloading` → `applying` → `succeeded`；失败可能直接进入 `failed`，或经 `rolling_back` 进入 `rolled_back` / `rollback_failed`。轮询可能跳过短暂状态。

| 状态 | 含义 |
| --- | --- |
| `building` | 等待构建名额、校验或打包中 |
| `ready` | 所有包已准备好，也可能正在等待 agent 重连 |
| `downloading` | agent 下载并验证部署包 |
| `applying` | agent 更新并启动服务 |
| `rolling_back` | 本次部署失败，正在恢复之前的部署 |
| `succeeded` | 部署成功 |
| `failed` | 任务失败 |
| `rolled_back` | 本次部署失败，已完成回退 |
| `rollback_failed` | 回退失败，需要查看 agent 日志 |

最后四种是终态，均不再占用该 agent 的活动部署名额。任务结果保存在 SQLite，agent 重连后同步；controller 在构建中重启会将该任务标记失败，已经交给 agent 的任务会恢复结果同步。

## Agent 连接方式

`GET /v1/agents` 的每个 agent 和 `GET /v1/agents/{id}` 都增加 `connection`：

```json
{"mode":"controller_to_agent","endpoint":"agent.example.com:7444","proxy_configured":true,"state":"reconnecting","last_error":"无法连接 SOCKS5 代理，正在重试"}
```

`mode` 为 `agent_to_controller`（默认）或 `controller_to_agent`；主动模式 `endpoint` 为 `null`。`state` 为 `connected`、`reconnecting`（被动 agent 离线）或 `waiting`（主动 agent 离线）；在线时 `last_error` 为 `null`。`proxy_configured` 仅表示是否保存了代理；不返回代理 URL、用户名、密码、配对秘密、token 或摘要。

### PUT /v1/agents/{id}/connection

要求管理员 Cookie、Origin 和 CSRF。`endpoint` 必填，`proxy` 可省略：

```json
{"endpoint":"agent.example.com:7444","proxy":"socks5://user:password@proxy.example.com:1080"}
```

支持域名、IPv4 和 `[IPv6]:端口`，不能使用通配地址、零端口、URL 路径或账号信息。仅用于已完成首次接入的被动 agent，不允许修改连接模式。成功返回更新后的 Agent 对象；保存后立即断开旧控制连接、业务通道及终端，并自动连接新地址，本地应用继续运行。返回结果中的在线状态可能短暂反映刚关闭的会话，以后续查询为准。

`proxy` 的更新语义如下，`POST /v1/enrollments` 中的 `agent_proxy` 使用相同规则：

| 值 | 行为 |
| --- | --- |
| 省略字段 | 保留已有配置；新授权默认直连 |
| `null` | 清除代理，改为直连 |
| URL 字符串 | 替换代理 |

仅支持 `socks5://host:port` 或 `socks5://username:password@host:port`，代理主机支持域名、IPv4、`[IPv6]`，必须显式指定非零端口。禁止路径（包括末尾 `/`）、查询参数、fragment 和非法百分号编码。用户名密码需同时提供，按 URL 百分号解码后的 UTF-8 各占 1–255 字节；特殊字符应百分号编码。不支持 HTTP/HTTPS 代理或 `socks5h://`；`socks5://` 本身已由代理解析目标域名。

每个 agent 的注册、控制、部署包、自动升级和终端连接均使用同一配置。TCP 建连、SOCKS5 协商和 Noise 认证合计最多 10 秒；失败后重试并提供不含凭据的错误原因，不自动回退直连。不读取代理环境变量，此设置与构建代理完全独立，仅保存在 controller 的数据库，不写入 agent 配置或配对凭据。修改代理同样会立即重连并关闭现有终端。

代理 URL 或 JSON 字段格式无效返回 `422`；地址无效返回 `400`，身份不存在返回 `404`，主动模式、初始化尚未完成、存在活动部署或升级时返回 `409`。已有模式不会自动切换；只更改 controller 保存的连接设置，不修改 agent 本地监听配置。

## 初始化授权

`GET /agent/init` 由内嵌 React 控制台提供，静态资源位于 `/assets/`。未登录时显示登录表单，controller 尚未初始化时显示管理员初始化表单；URL fragment 保持不变。账号登录不会自动批准接入，仍需点击授权。

交互初始化流程：

1. `pier-agent init` 生成请求 ID 和服务器信息，并输出 `http://pier.example.com:8080/agent/init#<base64url(JSON)>`。fragment 包含下文的请求对象。
2. 管理员在浏览器登录后核对服务器信息；页面使用 Cookie 和 CSRF 调用 `POST /v1/enrollments` 获取一次性配对凭据。
3. 用户将配对凭据粘贴回终端。agent 通过 Noise 加密 TCP 兑换长期身份，保存配置后确认完成；兑换和确认没有 HTTP 接口。
4. 页面可用 `GET /v1/enrollments/{id}` 查询进度；agent 启动后出现在 agent 列表中。

### POST /v1/enrollments

请求体最大 64 KiB。除注明默认值的连接字段外，其余字段必填；`info` 使用 AgentInfo 结构：

| 请求字段 | 类型 | 约束 |
| --- | --- | --- |
| `request_id` | string | init 生成的 64 位十六进制随机请求 ID |
| `connection_mode` | string | 可省略，默认 `agent_to_controller`；被动模式为 `controller_to_agent` |
| `listen` | string | 被动模式必填，由 init 提供的本机 IP:端口；主动模式禁止 |
| `agent_endpoint` | string | 被动模式必填，管理员在 Web 填写 controller 可达的 agent host:port；主动模式禁止 |
| `agent_proxy` | string / null | 被动模式可配置 SOCKS5；省略保留，`null` 清除，字符串替换；主动模式禁止设置代理 |
| `name` | string | 去除首尾空白后非空，不含控制字符，最多 256 字节 |
| `public_url` | string | 必须等于 controller 配置的规范 HTTP 或 HTTPS 来源地址，不含路径、查询、fragment 或末尾斜杠 |
| `info.architecture` | string | `amd64` 或 `arm64` |
| `info.hostname` | string | 最多 256 字节 |
| `info.os_release` | string | 最多 8192 字节 |

通常由授权页面发送。直接调用时，将来自 init 的请求保存为 `enrollment.json`，以下为结构示例：

```json
{
  "request_id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "name": "web-01",
  "public_url": "http://pier.example.com:8080",
  "info": {
    "architecture": "amd64",
    "hostname": "web-01",
    "os_release": "ID=ubuntu\nVERSION_ID=24.04\n"
  }
}
```

```sh
curl --fail-with-body -X POST "$CONTROLLER_URL/v1/enrollments" \
  --cookie "$COOKIE_JAR" \
  -H "Origin: $CONTROLLER_URL" -H "X-CSRF-Token: $CSRF_TOKEN" \
  -H 'Content-Type: application/json' --data @enrollment.json
```

```json
{
  "pairing": "pier-pair-v2.<base64url编码的配对凭据>",
  "id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "expires_at": 1790467800
}
```

`id` 当前等于 `request_id`，用于查询授权状态；`pairing` 是供终端使用的完整字符串，含一次性秘密，有效期为 10 分钟。它不是 agent 的长期 token，应完整复制，不写入日志。

有效期内重试完全相同且未完成的请求返回原配对凭据，不延长到期时间。未完成的授权过期后可用相同请求重新批准；同一个请求 ID 的内容（含模式和可达地址）改变或流程已完成时返回 `400`。业务错误为 `invalid or already completed enrollment; check connection settings`。代理配置可单独修正：未完成且未过期时重新提交相同请求、地址及新的 `agent_proxy`，保留配对秘密、有效期和已经签发的 agent 身份；已签发身份的连接配置同步更新并重连。Web 在配对等待期间提供“更新代理并重试”。部署或升级时拒绝此更新。

被动模式下 controller 在授权后开始主动拨号，agent 粘贴凭据后才接受注册握手。agent 保存配置并由 systemd 启动正常监听后，controller 以长期凭据建立控制连接并收到首次上报，授权状态转为 `completed`；重启和重复兑换保持同一身份。controller 入站监听器故障不阻止被动 agent 授权、连接或部署。主动模式保持原有兑换和确认流程。

此管理写接口要求 Cookie、Origin 和 CSRF；Origin 必须与配置的 public_url 完全一致，脚本调用也不能省略。

### GET /v1/enrollments/{id}

`id` 为批准初始化时返回的授权 ID，不是 agent ID。不存在时返回 `404`。

```sh
curl --fail-with-body "$CONTROLLER_URL/v1/enrollments/$ENROLLMENT_ID" \
  --cookie "$COOKIE_JAR" \
  -H "Origin: $CONTROLLER_URL" -H "X-CSRF-Token: $CSRF_TOKEN"
```

```json
{
  "id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "state": "authorized",
  "agent_id": null,
  "expires_at": 1790467800,
  "proxy_configured": true,
  "last_error": null
}
```

| `state` | 含义 |
| --- | --- |
| `authorized` | 已批准，尚未兑换 agent 身份 |
| `issued` | 已有关联的 agent 身份，等待 agent 确认完成 |
| `completed` | agent 已确认身份保存完成，临时秘密已清除 |
| `expired` | 尚未完成且已过期，需要重新授权 |

`agent_id` 是关联身份或 `null`；重新批准过期请求时可以保留先前分配的身份。已完成记录仍显示 `completed`，不会因到期改为 `expired`。`proxy_configured` 表示授权时的代理配置，`last_error` 提供当前注册拨号错误（无错误为 `null`）。查询不返回代理地址或凭据、配对凭据、长期 token 或任何 token 摘要。

## 制品下载

### GET /v1/artifacts/{deployment}/{app}

使用该部署目标 agent 的 token 下载自己的 tar.gz 包。管理员登录 Cookie 和其他 agent 的 token 均不能替代它。agent 日常下载使用加密 TCP，此 HTTP 接口保留供兼容客户端通过 HTTP 或 HTTPS 调用。

| 路径参数 | 含义 |
| --- | --- |
| `deployment` | 部署 ID |
| `app` | blueprint 的 app 实例 ID，例如 `api`；对应 `plan.apps[].id` |

```sh
curl --fail "$CONTROLLER_URL/v1/artifacts/$DEPLOYMENT_ID/api" \
  -H "Authorization: Bearer $AGENT_TOKEN" \
  --output api.tar.gz
```

成功返回 `200`，响应体为二进制流，包含 `Content-Type: application/gzip` 和 `Content-Length`。客户端应根据部署计划中的 `size` 与 `sha256` 校验下载内容。

部署或所属 agent 不存在返回 `404`；存在的部署使用错误或缺失的 token 返回 `401` JSON 错误；通过鉴权后，包尚未构建完成、app 不存在或制品文件缺失返回 `404`。

## 一次部署的调用顺序

1. 通过 `pier-agent init` 完成网页授权并启动 agent，或手动注册并配置启动 agent。
2. 查询 `/v1/agents`，确认目标 agent 在线且架构正确。
3. 查询 `/v1/blueprints`，读取当前 commit、blueprint 变量和 app 声明。
4. PUT agent 的 binding，提供所有无默认值的 blueprint 变量，按需覆盖其他变量。
5. POST deployment，传入 agent ID、已核对的 commit，以及每个源码实例的构建镜像。
6. 查询部署详情直到终态；结合 agent 的进程报告检查运行状态。

修改绑定或同步 Git 只更新后续部署所用的定义，需要再次创建部署才会应用到服务器。

### 按蓝图上报运行状态

agent 的 `report.blueprints` 数组包含 `id`（蓝图路径摘要）、`blueprint`（路径）、`name`（账户名）、`deployment_id`（该蓝图最后成功任务）、`state`、`account_reserved`（是否保留专属账户）、`apps` 和 `result`（该蓝图最近任务结果）。`state` 为 `starting`、`running`、`backoff`、`failed` 或 `stopped`。停止或解除绑定不删除 agent 保存的账户和蓝图记录。

`apps` 中的应用状态字段保持不变，同一蓝图应用共享账户和数据，故障时一起重启。为兼容查询，顶层 `report.apps` 仍为所有蓝图应用的展开列表，`report.result` 为整台 agent 最近任务结果；顶层 `deployment_id` 仅在恰有一个蓝图时有值，多蓝图客户端应使用分组字段。终端接口仍按应用实例 ID 打开，但终端账户和主目录属于整个蓝图。部署一个蓝图不关闭其他蓝图的终端。
