# Pier 管理控制台

React 19、TypeScript、Ant Design 6 和 Vite 单页应用。生产静态资源由 Rust controller 内嵌并提供，页面和 API 共用配置的 HTTPS 来源。

## 构建

使用 Node.js >=22.12.0（推荐 Node 24），在本目录执行：

```sh
npm ci
npm run format:check
npm run typecheck
npm run build
npm run build:check
```

`package-lock.json` 与 `dist/` 一同纳入版本管理。`build:check` 在临时目录重新构建并比较文件内容，用于发现过期的内嵌资源。更改页面后重新构建 controller；普通 Cargo 构建只读取已有资源，不运行 npm。构建产物缺失时 Rust 构建会明确报错。

`npm ci` 的 postinstall 为锁定版本的 `@rc-component/util` 补上动态样式 nonce：其滚动条测量与弹层锁定样式没有继承 ConfigProvider 的 CSP 配置（[上游问题](https://github.com/react-component/util/issues/613)）。补丁仅作用于该库创建的 style 元素；版本变化时安装会明确失败，需重新检查补丁和浏览器 CSP 测试。

## 开发

`npm run dev` 启动 Vite，默认把 `/v1` 转发到本机 `127.0.0.1:8080`。开发时将 HTTPS 反向代理上游指向 Vite，并支持 WebSocket 热更新；controller 的 `public_url` 与浏览器访问来源一致。生产上游仍指向 controller。

controller 启动 YAML 只保留 `http_listen` 和 `state_dir`。初次访问 `/init` 设置管理员、仓库，以及“运行设置”中的 agent 监听、公开地址、agent 公布地址、构建并行数和代理；初始化成功立即生效。后续在 `/settings/controller` 保存运行设置并手动重启，页面显示当前值、保存值、待重启状态及监听错误。初始化后到“定义仓库”手动同步；该页也支持保存新的地址或分支，保存不会触发拉取。后续在 `/login` 登录。API 通过 HttpOnly Cookie 认证，写请求带会话返回的 CSRF token。密码、登录 Cookie、代理凭据、agent 凭据和绑定变量不会写入 localStorage/sessionStorage。`/agent/init` 在原地址完成登录，保留 init 链接的 fragment。

“仓库同步与构建代理”复用 `build_proxy`，定义仓库同步按仓库协议使用 HTTP 或 HTTPS 代理，遵循 NO_PROXY；未配置对应代理时直连，不继承系统环境或 Git 的 HTTP 代理。保存或清除后重启 controller 生效。app 仍由其 `proxy.enabled` 控制，SSH 与本地仓库连接方式保持不变。

“全局变量”页面可维护所有服务器共享的字符串值，并显示引用位置。在 Blueprint 绑定表单选择“引用全局变量”后保存变量名，创建部署时获取最新值；已排队的部署使用创建时的快照。所有全局变量值可查看，被引用时禁止删除。变量和绑定均不写入浏览器持久存储。

## 测试

仓库代理的 Rust 集成测试需要 Python 3、OpenSSL 和 Git，通过本地 smart HTTP 服务、带认证的 HTTP(S) 代理及受信任的临时证书验证真实 Git 拉取，不访问外部仓库：`cargo test -p pier-controller repository_proxy_transport_and_restart`（在仓库根目录执行）。

先从仓库根目录执行 `cargo build -p pier-controller --locked`，再在本目录执行：

```sh
npx playwright install --with-deps chromium
npm test
```

测试需要 Git、OpenSSL、Chromium 运行依赖，以及可用的本机端口 8444、18084、17444。测试会启动临时 controller、Git 仓库和 HTTPS 代理，退出时清理数据；可在一次性 Ubuntu 容器中安装浏览器依赖后执行。

浏览器测试使用真实后端验证初始化及运行设置立即生效、保存设置后待重启、登录、Cookie/CSRF、仓库、变量绑定、配对授权和改密，并检查手机尺寸登录页面及会话失效后的状态清理。部署表单测试模拟在线 agent、commit 冲突和任务进度；真实加密传输、进程启动、升级及回退由仓库的 Docker 生命周期测试覆盖。
