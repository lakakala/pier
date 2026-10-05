# controller 与 agent

`pier-controller` 和 `pier-agent` 是两个可执行服务，同时提供可嵌入的 `run(Config)` Rust API。`pier-pkg` 继续保持库形式。服务支持 Linux `amd64`、`arm64`，目标运行环境为 AlmaLinux 8、AlmaLinux 9 和 Ubuntu 24.04。

```text
Git 定义仓库 → controller → pier-pkg → tar.gz
                  ↑                    ↓ 加密 TCP 下载
                  └── TCP + Noise ──── agent → 多蓝图 → 蓝图共享用户 / 应用进程
```

controller 使用本地 SQLite 和产物目录，第一版运行单实例；agent 使用自己的 SQLite 保存成功部署和事务恢复记录。两个服务均锁定各自的数据目录，禁止同一目录并发运行多个实例。不要让两个服务共用数据目录。

## 安装与交互初始化

agent 提供 Ubuntu 24.04 的 DEB 包和 AlmaLinux 8/9 的 RPM 包，两者均支持 amd64/arm64。DEB 架构名称为 `amd64`、`arm64`，RPM 对应 `x86_64`、`aarch64`。

```sh
# Ubuntu 24.04
sudo apt install ./pier-agent_0.1.0-1.ubuntu24.04_amd64.deb
# AlmaLinux 8
sudo dnf install ./pier-agent-0.1.0-1.el8.x86_64.rpm
# AlmaLinux 9
sudo dnf install ./pier-agent-0.1.0-1.el9.x86_64.rpm

sudo pier-agent init
```

首次安装只安装程序、unit 和文档，不启动或启用 agent，也不要求在软件包安装脚本中输入信息。`init` 需要 root、交互终端和运行中的 systemd；SSH 场景可使用 `ssh -t`。界面支持方向键和回车，`TERM=dumb` 使用数字菜单。

向导依次填写 controller 的 **HTTP 或 HTTPS 网页地址**、选择连接方式、agent 名称（默认主机名），选择数据目录，确认后显示授权链接。可在另一台电脑打开链接，也可选择尝试打开本机浏览器。网页登录管理员账号后核对服务器信息，并批准接入，然后将一次性配对凭据粘贴回终端。配对输入不回显。初始化成功后自动执行 `systemctl enable --now pier-agent` 并退出向导，服务在后台运行。

连接方式默认“Agent 主动连接 Controller”，沿用现有网络要求。选择“Controller 主动连接 Agent”时，向导要求本机监听地址（默认 `0.0.0.0:7444`），Web 授权页要求填写 controller 能访问的 agent 地址，例如 `agent.example.com:7444` 或 `[2001:db8::1]:7444`。本机监听地址与可达地址可以不同，支持 NAT 端口映射。两种模式可同时接入一个 controller；已有 agent 缺少模式配置时继续主动连接。本次不提供已有身份切换模式的向导。

Web 授权页和服务器详情页可为每个被动 agent 配置独立的 SOCKS5 代理。选择“替换代理”并填写 `socks5://proxy.example.com:1080` 或 `socks5://user:password@proxy.example.com:1080`；用户名密码中的特殊字符需要 URL 百分号编码。支持无认证或用户名密码认证，目标域名由代理解析，目标及代理地址支持 IPv4/IPv6。Web 只显示“已配置 SOCKS5”或“直连”，不回显已保存的代理 URL。

“保留已有代理”不会覆盖现有配置，新授权默认直连；“不使用代理”清除配置。授权尚未完成时，代理填错可点击“更新代理并重试”，原配对凭据、有效期和身份保持不变。服务器详情保存后立即重连并关闭已有终端，应用继续运行，部署或升级期间禁止修改。

此代理覆盖注册、控制、部署包传输、自动升级和终端，独立于构建代理，也不读取 controller 的代理环境变量。配置仅保存在 controller 数据库，不进入 agent 配置或配对凭据。TCP 建连、SOCKS5 协商及 Noise 认证共用 10 秒超时，代理失败后显示原因并自动重试，不回退直连。仅支持 `socks5://`，URL 必须带端口，不能包含路径、查询或 fragment。

被动模式下，agent 及升级辅助进程均不向 controller 发起 TCP 连接。浏览器可以在其他电脑上完成授权，配对仍通过粘贴临时凭据验证对端。controller 主动连接 agent 的同一个端口完成注册、控制、制品下载、升级和终端；业务数据使用独立认证连接，不占用心跳队列。临时监听器在配对完成后移交给 systemd 服务；正常控制握手及首次上报成功后，网页才显示注册完成。

服务器详情显示连接方向、可达地址和最近连接错误。被动 agent 完成初始化后可修改可达地址，保存立即重连并关闭已有终端，应用继续运行；正在部署或升级时拒绝修改。无法连接时按 1–60 秒退避重试，地址保存在数据库并在 controller 重启后恢复。只需允许 controller 访问 agent 的监听端口；无需向被动 agent 开放 controller 的入站 TCP 端口，也不会自动修改服务器防火墙。

被动 agent 配置示例（由 `init` 生成，无需手工编辑）：

```yaml
agent_id: replace-with-enrolled-agent-id
token_file: /etc/pier/agent.token
connection_mode: controller_to_agent
listen: 0.0.0.0:7444
state_dir: /var/lib/pier-agent
heartbeat_seconds: 15
```

主动模式使用 `connection_mode: agent_to_controller`（可省略）和 `controller_tcp`，不能配置 `listen`；被动模式必须配置非零监听端口，不能配置 `controller_tcp`。controller 初始化中的 `agent_endpoint` 仍是主动 agent 连接的 controller 公布地址，与每个被动 agent 的可达地址分别维护。


配置固定保存在 `/etc/pier/agent.yml`，独立 token 保存在 `/etc/pier/agent.token`，两者权限均为 `0600`。数据目录默认为 `/var/lib/pier-agent`。初始化进度保存在受限权限的 `/etc/pier/agent.init.json`；取消或网络中断后再次运行 `init` 可继续，过期凭据需重新在网页授权。已有配置时可选择启动服务、查看状态或退出，不覆盖身份、不重复注册。

```sh
sudo systemctl status pier-agent
sudo journalctl -u pier-agent -f
sudo systemctl stop pier-agent
sudo systemctl restart pier-agent
```

systemd 以 root 运行 agent，以便创建蓝图专属用户和切换身份。agent 恢复本地部署后通过 `READY=1` 报告就绪，此时不要求 controller 在线；每个蓝图的应用进程组由 agent 守护。`Restart=on-failure` 在 agent 异常退出后重启，间隔 5 秒。`KillMode=mixed` 先向 agent 发送 SIGTERM，由它停止 app；停止超时 900 秒后 systemd 清理整个进程组所属的 cgroup。初始化失败保留已经写好的身份配置，并显示日志排查命令。

agent 与 controller 握手时报告运行版本、已安装包版本、发行版和自动升级能力。controller 内置对应平台的新包时，agent 自动下载，在部署空闲后安装并重启；其管理的 app 会短暂中断并从本地状态恢复。仅升级，不自动降级；版本按数字 `x.y.z`、修订号比较，重新构建同版本同修订号不会触发升级。

下载使用独立的认证加密 TCP 连接，不占用心跳连接。安装前验证长度、SHA256、包名、架构、发行版标识和原生包版本。EL8 与 EL9 的 RPM 不能互相作为自动升级目标。安装由独立的 `pier-agent-upgrade.service` 完成，使用本地 `dpkg --install` 或 `rpm --upgrade`，不自动下载额外依赖。安装和重启期间拒绝新部署，新进程恢复本地服务并通知 systemd 就绪后才算成功；确认不依赖 controller 在线。配置、token、SQLite 和 app 数据保留。

下载中断会退避重试。校验、安装、启动失败或安装被重启打断后，暂停同一目标版本，需要手动恢复，不自动回滚。服务器详情显示失败原因。排查并手动安装正确的软件包、重启 agent；成功运行到目标或更新版本后恢复状态。事务及下载包位于 `/var/lib/pier-agent-upgrade/`（root、0700），不要在升级单元运行时删除或修改。日志命令：

```sh
sudo journalctl -u pier-agent-upgrade -u pier-agent
sudo systemctl status pier-agent-upgrade pier-agent
# 手动修复软件包后
sudo systemctl reset-failed pier-agent
sudo systemctl restart pier-agent
```

旧版 Ubuntu agent 无法识别新增的 `.ubuntu24.04` 原生版本后缀，需要从 GitHub Release 下载对应架构的新 DEB，手动安装一次并重启。建议先迁移 Ubuntu agent，再升级 controller，避免旧 agent 尝试新包后进入失败暂停；后续版本恢复自动升级。已有配置、token 和部署数据保留，不需要再次执行 `init`。新 agent 能读取旧式数字 DEB 修订号，但新发布的升级包必须包含正确的发行版标识。EL8 原生版本格式不变，可继续自动升级。

```sh
# 使用本次 Release 的实际版本、修订号和架构替换文件名
sudo dpkg --force-confold --install ./pier-agent_0.1.0-2.ubuntu24.04_amd64.deb
sudo systemctl restart pier-agent
```

更早、没有更新器的 agent 同样需要手动安装一次新版包并重启。直接运行开发二进制、非 systemd 实例和非受支持发行版不自动升级。

服务器列表和详情页显示自动升级不可用的具体原因。agent 启动时依次检查系统发行版、原生安装包和 systemd 管理状态，上报第一个未通过的检查：无法读取系统信息、系统不支持、包管理器查询失败（含查询工具及退出码或超时）、安装包未完整安装、RPM Epoch 不支持、包架构或版本后缀不匹配、运行程序不在 `/usr/bin/pier-agent`、服务无主进程或 MainPID 与当前进程不符。随后还会检查升级目录权限、锁和升级记录归属；重新初始化后遗留其他 agent 的升级记录也会明确提示。修正后重启 agent 重新检测。历史升级成功或失败记录不会遮住当前不可用原因；旧 agent 需要手动更新后才能上报这些分项诊断。

手动执行包管理器安装仍不会在安装脚本中重启 agent；需要执行 `systemctl restart pier-agent`。DEB 附带仅针对本服务的 needrestart 设置。卸载停止两个单元并禁用主服务，保留配置、token、蓝图用户、部署数据与升级记录；DEB purge 同样保留这些运行数据。

## 在 Web 中打开应用终端

登录 controller，进入“服务器 → 服务器详情”，在蓝图下的已部署应用行点击“终端”，即可打开蓝图系统用户的交互式 Bash。终端可全屏，支持 Tab 补全、Ctrl+C、复制粘贴和窗口缩放；顶部显示实际用户名与主目录。

默认进入蓝图用户的主目录（蓝图共享数据目录），使用基础 Shell 环境，不自动加载 `service.env` 或 app 声明的变量。账户保留 `nologin`，由 agent 验证身份后通过 PTY 启动 Bash，无需 SSH 配置或新端口。安装包显式依赖 Bash，应用发布目录保持原有权限。

应用退出触发蓝图整组自动重启时，终端继续保留。关闭终端只结束该终端的 Bash 和会话内作业；显式部署、回滚或停止蓝图只结束目标蓝图的终端。agent 升级/停止、控制连接丢失或 Web 登录失效会结束关联终端，并显示原因。网络中断最长 45 秒检测，重连创建新会话，不恢复旧 Bash。

每个 agent 最多 8 个终端，controller 最多 64 个。浏览器使用同源 WebSocket（HTTP 对应 WS，HTTPS 对应 WSS），根据连接模式由 agent 或 controller 发起独立 Noise 连接，复用对应接收方的通信监听端口。使用反向代理时须支持 WebSocket Upgrade，见 [Nginx 示例](../examples/services/controller.nginx.conf)。服务端不记录终端输入输出；Bash 历史由账户配置决定。

新 controller 与旧 agent 继续使用协议 v2，并保留原自动升级通道；多蓝图部署需要 `multi_blueprint_v1` 能力，终端按钮另需 `app_terminal_v1` 能力。接口及帧格式见 [应用终端 API](api.md#应用-bash-终端)。GitHub Actions 的六个平台安装测试覆盖真实 PTY、账户环境、流控和进程清理。

## Controller 安装与网页初始化

controller 同样提供 DEB/RPM 和 amd64/arm64 两种架构，无需 `pier-controller init` 命令。

```sh
# Ubuntu 24.04
sudo apt install ./pier-controller_0.1.0-1.ubuntu24.04_amd64.deb
# AlmaLinux 8
sudo dnf install ./pier-controller-0.1.0-1.el8.x86_64.rpm
# AlmaLinux 9
sudo dnf install ./pier-controller-0.1.0-1.el9.x86_64.rpm

# 启动后可直接通过 HTTP 访问；HTTPS 反向代理可选
sudo systemctl enable --now pier-controller
sudo journalctl -u pier-controller -f
```

包创建无登录权限的 `pier-controller` 系统用户，安装程序到 `/usr/bin/pier-controller`。首次生成 `/etc/pier/controller.yml`（root:pier-controller、0640），数据目录为 `/var/lib/pier-controller`（专用用户、0700）。systemd 使用专用用户、`Type=simple`、`Restart=on-failure`，异常退出 5 秒后重启，日志写入 journal。首次安装不启动或启用服务；升级不停止、不自动重启，维护窗口执行 `sudo systemctl restart pier-controller`。DEB 的 needrestart 配置只抑制本服务自动重启，管理员更高优先级策略仍可覆盖。卸载（包括 DEB purge）停止并禁用服务，保留生成的配置、系统用户、SQLite、仓库快照及制品。

启动配置只保留 Web 监听地址和数据目录：

```yaml
http_listen: 127.0.0.1:8080
state_dir: /var/lib/pier-controller
```

未初始化时只启动 Web，不监听 agent TCP。Git 和 CA 证书通过包依赖安装。网页已内嵌，无需 Node.js 或 HTTPS 代理；源码构建所需的 Docker 自行准备。远程直接访问时，将 `http_listen` 改为 `0.0.0.0:8080` 或指定网卡地址，再打开 `http://服务器地址:8080/init`。随包的默认配置与 Nginx 示例位于 `/usr/share/doc/pier-controller/examples/`。

通过实际 HTTP 或 HTTPS 地址打开 `/init`，填写管理员账号、密码、仓库地址和分支（默认 `main`）。展开“运行设置”可配置 agent TCP 监听地址（默认 `0.0.0.0:7443`）、公开 HTTP 或 HTTPS 地址、agent 公布地址、并行构建数（默认 2，范围 1–64）及构建代理。公开地址默认采用当前网页来源，初始化时必须与它一致；agent 公布地址留空时取同一主机及所选 TCP 端口，支持 IPv6。使用反向代理时必须保留包含端口的 Host。

初始化先尝试绑定 agent 端口，再将账号、会话、仓库和运行设置在同一事务中保存，成功后立即启用 agent 通信，无需重启。端口被占用时初始化失败，可修改端口重试，不会留下半完成的管理员。初始化不拉取 Git，完成后到“定义仓库”页面点击“立即同步”。

后续在“控制器设置”（`/settings/controller`）修改运行设置。页面分别显示当前生效值和已保存值；保存后提示“待重启”，现有监听、来源校验和构建参数继续使用当前值，执行 `sudo systemctl restart pier-controller` 后切换。修改公开地址后，重启前仍用旧地址，重启后使用新地址；HTTP 与 HTTPS 切换后需重新登录。修改 agent 公布地址不会自动重写已注册 agent 的本地配置。代理可选择保留、替换或清除，页面不回填已有代理凭据。仓库地址和分支仍在“定义仓库”单独保存、手动同步。

重启时如果 agent 端口无法绑定，Web 仍可登录并修改设置，概览和设置页显示错误；接入授权和新部署暂时不可用。修正监听地址并再次重启即可恢复。

**定义仓库仅手动同步**：首次初始化、保存配置、启动和重启均不拉取，没有定时任务。修改地址或分支后显示“待同步”，当前配置成功同步前不能创建新部署；原有服务继续运行，旧快照保留。相同配置同步失败时，仍可使用之前成功的目录。网页也可再次修改配置或手动重试。旧 YAML 的仓库配置仅在首次迁移时导入数据库，此后网页配置为准；旧 `sync_interval_seconds` 字段允许解析但已不生效。源码 app 的 Git 下载仍在部署打包时执行。

源码打包需要专用用户能访问本机 Docker。例如使用系统 Docker 组时，管理员执行以下操作；这会授予该用户 Docker daemon 权限：

```sh
sudo usermod -aG docker pier-controller
sudo systemctl restart pier-controller
sudo -u pier-controller env HOME=/var/lib/pier-controller docker info
```

服务的 HOME 为数据目录，Git SSH 密钥、known_hosts 或 credential 配置应由该用户持有。构建临时目录位于数据目录的 `tmp/`，通过真实路径交给 Docker；不要为此服务配置 `PrivateTmp=yes`。自定义数据路径时同时调整 systemd 的 WorkingDirectory、HOME、TMPDIR 和 ExecStartPre。服务退出不影响 agent 上已运行的 app。

已有 root 运行的 controller 迁移时，先停止旧进程、备份数据，再为专用用户调整所用配置和数据目录权限；包安装不会递归 chown 已有数据。默认路径示例：

```sh
# 确认旧进程已停止，且目录为本 controller 专用
sudo chown -R pier-controller:pier-controller /var/lib/pier-controller
sudo chmod 0700 /var/lib/pier-controller
sudo chown root:pier-controller /etc/pier/controller.yml
sudo chmod 0640 /etc/pier/controller.yml
sudo systemctl enable --now pier-controller
```

保留旧 YAML 启动一次即可迁移：仓库配置、agent 监听、公开地址、agent 公布地址、构建并行数和代理按旧版本的有效值导入 SQLite，已有管理员和 Cookie 会话、agent、绑定与部署不重置。迁移成功后 YAML 只需保留 `http_listen` 和 `state_dir`；旧运行字段为兼容仍可解析，但不会再次覆盖数据库或网页保存值。旧 `public_url` 和 `agent_endpoint` 仅在首次迁移时按原覆盖规则读取。

## GitHub Actions 构建与发布

服务安装包通过仓库的 **Packages** 工作流构建。AMD64 使用 `ubuntu-24.04`，ARM64 使用 `ubuntu-24.04-arm`；两者分别在对应架构的虚拟机内执行现有 Dockerfile。Rust 在 AlmaLinux 8 容器中编译，DEB 使用 Ubuntu 24.04 原生工具打包，RPM 使用 AlmaLinux 8 原生工具打包。工作流不需要镜像仓库账号，构建镜像只在任务内使用。

| 触发方式 | 行为 |
| --- | --- |
| 推送 `master` | 自动分配发布版本，执行 CI，构建并验证十二个安装包，创建标签并发布 GitHub Release |
| 其他分支提交、PR | 独立执行 CI：Rust 格式、Clippy、工作区测试、工作流及发布脚本检查，Web 类型、构建一致性与浏览器测试 |

两个服务共用根 `Cargo.toml` 的 `[workspace.package].version`，各自通过 `version.workspace = true` 继承。升级软件版本时只修改根版本，然后运行 `cargo check --workspace` 更新 `Cargo.lock`，将两者一起提交并推送 `master`。`pier-pkg` 和 `pier-protocol` 的 crate 版本仍独立维护，通信协议版本不随软件版本自动变化。

```toml
# 根 Cargo.toml
[workspace.package]
version = "0.1.0"
```

无需手动打标签，也没有手动运行 Packages 的入口。工作流读取触发提交中的共享版本，不改写源码版本；标签推送不会触发打包。

| 共享 Cargo 版本 | 同一版本的发布 | Release / Tag | DEB / RPM 修订号 |
| --- | --- | --- | --- |
| `0.1.0` | 首次 | `v0.1.0` | `1` |
| `0.1.0` | 第二次 | `v0.1.0-r1` | `2` |
| `0.1.0` | 第三次 | `v0.1.0-r2` | `3` |
| `0.2.0` | 首次 | `v0.2.0` | `1` |

`-rN` 是发布标签的重打包后缀，原生包修订号为 `N + 1`。例如 `v0.1.0-r1` 包含 `pier-agent_0.1.0-2.ubuntu24.04_amd64.deb` 和 `pier-agent-0.1.0-2.el8.x86_64.rpm`，controller 使用相同基础版本和修订号。`--version` 仍显示 Cargo 基础版本；agent 通过原生包版本和修订号识别升级。

版本分配分页查询所有 Release（包括草稿）和 Git 标签，以当前基础版本的最大已用后缀递增，不补空缺。其他基础版本和非标准名称不参与编号。查询失败或超出范围时停止；基础版本只接受无前导零的 `X.Y.Z`，修订号上限为 `2^64 - 3`，为升级测试预留两个后续修订。

整个发布流程使用固定并发组串行执行，`queue: max` 保留最多 100 个等待运行的任务，避免后续推送取消已排队构建。超出队列容量时 GitHub 会取消新增等待任务；执行顺序以进入并发队列的顺序为准，不保证提交顺序。每次发布始终使用该次触发提交的 SHA，标签不会指向构建期间后来推送的提交。相关机制见 [GitHub 并发队列说明](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/control-workflow-concurrency)。当前 actionlint 版本尚不识别 `queue`，检查脚本仅忽略这一条语法诊断，其他检查正常执行。

流程先由六个 agent 任务分别编译、打包一个目标系统和架构，再交给六个 controller 任务制作内嵌升级 bundle。每个 controller 任务独立编译自身平台的二进制，并只在该系统、该架构的临时 systemd 容器内验证安装、部署、重启及自动升级；测试包使用下一修订号，与正式包分开保存。汇总任务校验十二个包的原生元数据、controller 内嵌包与正式 agent 包的一致性，最后生成统一的 `SHA256SUMS`。

构建完成后，可在运行页面下载 **pier-packages** artifact（保留 30 天）。发布任务再次校验附件和版本占用情况，原子创建指向触发提交的标签，将十二个 DEB/RPM 和校验文件上传到同名草稿 Release，附件齐全后才公开并设为 Latest（包括 `-rN` 发布）。已有标签、Release 或草稿均不覆盖。

发布失败时保留已创建的标签及草稿。选择 **Re-run all jobs** 会重新查询远端并分配可用版本；已有草稿或标签会占用对应编号。仅重跑失败任务沿用原先分配的版本，遇到占用时会失败，不能给已构建的包临时换号。创建标签前失败的构建尚未占用版本，完整重跑时可再次使用该编号。

```sh
# 解压 Actions artifact 或下载 Release 附件后
sha256sum --check SHA256SUMS
```

任一检查失败都会阻止正式发布。任务日志可在 Actions 查看，打包日志、systemd 日志与浏览器失败诊断会保留为独立 artifact（7 天）。前端资源仍纳管在 `web/dist`；构建一致性检查失败时，先在前端目录执行 `npm ci && npm run build`，提交更新后重试。Rust 工具链与构建 Dockerfile 保持一致，Cargo 和 npm 使用锁文件，原生编译缓存按服务、目标系统、架构及实际镜像 ID 隔离。

版本分配任务和发布任务使用 `GITHUB_TOKEN` 的 `contents: write` 权限，分别用于读取完整草稿历史和创建标签、Release；其余任务只读。默认使用 `master` 作为发布分支，仓库需允许执行 Actions 和使用此令牌创建标签及 Release。app 的源码/二进制 tar.gz 打包仍由 `pier-pkg` 与 controller 执行。

## Actions 构建与内嵌升级包

服务安装包的编译、打包与安装验证全部在 GitHub Actions 执行，安装包从 Release 或 `pier-packages` artifact 获取。仓库中的打包、测试夹具和发布脚本是工作流内部组件，不需要在本地准备 Docker、QEMU 或安装包。

Actions 按目标系统和架构拆分：agent、controller 各有六个任务，例如 `agent (almalinux8, amd64)`、`agent (almalinux9, arm64)`、`controller (ubuntu24.04, amd64)`。amd64 使用 GitHub 的 `ubuntu-24.04` runner，arm64 使用 `ubuntu-24.04-arm` runner；任务名称显示容器内实际编译、打包和测试的目标系统。

| 目标系统 | Rust 编译镜像 | 安装包 |
| --- | --- | --- |
| Ubuntu 24.04 | `pier-builder-rust:ubuntu24.04`，基于 `ubuntu:24.04` | `.ubuntu24.04` DEB |
| AlmaLinux 8 | `pier-builder-rust:almalinux8`，基于 `almalinux:8.10` | `.el8` RPM |
| AlmaLinux 9 | `pier-builder-rust:almalinux9`，基于 `almalinux:9.8` | `.el9` RPM |

三个系统分别编译自己的二进制，使用一致的固定 Rust 版本和独立 Dockerfile。每个任务只生成当前平台的一个正式包和一个下一修订号测试包。安装镜像依赖前刷新软件源索引；编译缓存和编译目录按服务、目标系统、架构及实际镜像 ID 隔离，测试夹具使用对应系统和架构的二进制副本。ELF 校验要求 GLIBC 不超过 EL8 的 2.28、EL9 的 2.34、Ubuntu 24.04 的 2.39，原生打包工具同时生成动态库依赖。

矩阵由 `scripts/release.py matrix` 从受支持的平台集合生成，agent、controller 使用同一矩阵。工作流内部打包脚本必须传入 `--system ubuntu24.04|almalinux8|almalinux9`，以及编译镜像和架构；包格式由系统决定。升级夹具和自动升级测试也显式指定系统。Artifact 与日志名称包含服务、系统和架构，例如 `packages-agent-almalinux9-arm64`，不同任务不会覆盖产物。Ubuntu 旧式 DEB 迁移夹具只在 Ubuntu 任务中生成和使用。

Ubuntu 包的 `Version` 为 `X.Y.Z-N.ubuntu24.04`，RPM 的 `Version` 为 `X.Y.Z`、`Release` 为 `N.el8` 或 `N.el9`，文件名同步反映这些字段。发行版标识不进入 Cargo 版本或 Release 标签，也不参与 agent 的基础版本与数字修订号比较。本次支持 AlmaLinux 8/9 和 Ubuntu 24.04，未扩展到 Rocky Linux、RHEL 或其他 Ubuntu 版本。

controller 打包前必须取得同版本、同修订号的六个 agent 包（三个系统 × 两种架构）。工作流通过原生工具读取元数据并计算 SHA256；缺包、重复平台、发行版标识或版本不一致都会中止打包。manifest 和六个包安装到 `/usr/share/pier-controller/agent-releases/`。controller 启动时验证并复制到自身数据目录的不可变缓存；安装新 controller 包后重启才发布新升级目标。bundle 缺失或校验失败只停用自动升级，Web 和普通部署继续工作。

## 命令行与运行接口

命令行由 clap 解析。agent 使用 `init` 或 `run` 子命令，`run` 省略 `--config` 时读取 `/etc/pier/agent.yml`；根级 `pier-agent --config PATH` 已移除。controller 的 `--config PATH` 必填。两个服务均支持 `-h/--help`、`-V/--version`，agent 可通过 `pier-agent init --help`、`pier-agent run --help` 查看子命令帮助。显式帮助和版本查询退出码为 0，参数错误和 agent 未指定子命令时退出码为 2；参数解析完成后才读取配置或执行初始化。

两个服务仍提供 `run(Config)` Rust API。配置中的相对文件路径按配置所在目录解析。controller 需要 Git 和系统 `ca-certificates` 包来初始化源码下载客户端，源码打包还需要 Docker；agent 无需 Docker。可设置 `RUST_LOG=info`。agent 处理 SIGTERM/SIGINT，停止 app 后退出；重启从本地成功部署恢复。配置示例见 [controller.yml](../examples/services/controller.yml) 和 [agent.yml](../examples/services/agent.yml)。

## 前端开发与内嵌资源

前端位于 `crates/pier-controller/web`，Node.js 要求 >=22.12.0，推荐 Node 24。生产资源保存在该目录的 `dist/` 并随源码纳管，Rust 编译时嵌入 controller。部署二进制无需 Node.js；普通 Cargo 构建不执行 npm，不访问前端 CDN。

```sh
cd crates/pier-controller/web
npm ci
npm run typecheck
npm run build
npm run build:check
# 返回仓库根目录重新编译 controller 以嵌入更新后的资源
cd ../../..
cargo build -p pier-controller --locked
```

`npm run dev` 启动本地 Vite 开发服务，`/v1` 转发到 `127.0.0.1:8080`。开发时可直接通过 HTTP 访问 Vite，也可使用 HTTPS 代理；controller 的 `public_url` 须与浏览器来源一致。使用代理时需支持 WebSocket 热更新。生产环境由 controller 提供嵌入式网页，无需独立静态目录。

GitHub Actions 在独立的临时仓库和 controller 状态下，依次运行 HTTP、HTTPS 两套浏览器回归。HTTP 直接连接 controller，使用映射到本机的 `pier-http.test` 域名，避免 localhost 的安全上下文特例；HTTPS 使用测试反向代理。两套验证均覆盖初始化、Cookie/CSRF、接入授权、代理编辑、终端、退出和改密，HTTP 额外验证剪贴板不可用时的手动复制提示。测试结束后清理状态；协议由 `PIER_E2E_SCHEME=http|https` 选择，诊断文件按协议分别保留。

页面深层路径支持刷新；不存在的 API 和静态资源返回 404。HTML 使用每次请求生成的 CSP nonce，Ant Design 动态样式沿用该 nonce；脚本仅允许同源资源。API 和页面响应均禁止缓存。

## 网页授权与无证书通信

controller 本身提供 HTTP，默认监听 `127.0.0.1:8080`。可直接使用 HTTP，或通过可选的 HTTPS 反向代理访问。网页运行设置中的 `public_url` 是规范的 HTTP 或 HTTPS 来源地址，例如 `http://pier.example.com:8080` 或 `https://pier.example.com`，不含路径或末尾斜杠；`agent_endpoint` 是服务器实际可连接的 `host:port`，例如 `pier.example.com:7443`。监听地址和公开地址分开配置。未迁移旧公开地址时，首次初始化要求规范 Origin 与保留端口的 Host 一致，校验协议取自 Origin；初始化后固定使用当前生效的来源，不按后续请求或转发头重新推断地址。反向代理示例见 [controller.nginx.conf](../examples/services/controller.nginx.conf)。根据访问方式开放 Web 端口，以及对应连接模式的 controller 或 agent 加密 TCP 端口。

首次打开 controller 的 `/init` 页面，设置唯一管理员用户名、密码、确认密码和定义仓库，成功后自动登录。已有 YAML 仓库配置迁移后无需重复填写。初始化不需要初始化码，只允许成功一次；后续访问 `/login`。管理员账号和会话保存在 SQLite，密码使用 Argon2id 随机盐哈希；controller 重启不会重新开放初始化。

完整管理控制台使用 React、TypeScript 与 Ant Design，支持运行设置、仓库同步、app/blueprint 声明查看、agent 绑定及变量编辑、创建部署、查询任务和进程状态、接入授权、修改密码。Git 定义仍在仓库中修改。修改密码会使所有浏览器会话失效。

浏览器认证使用 HttpOnly、SameSite=Strict、Path=/ 的 Cookie，固定 8 小时有效；HTTP 使用 `pier_session`，HTTPS 使用带 Secure 的 `__Host-pier_session`。Cookie 策略以当前生效的公开地址为准；所有管理 API 写操作校验同源 Origin 和会话 CSRF token。客户端不保存管理员密码或长期管理 token。退出立即撤销当前会话。脚本同样先登录并保存 Cookie，调用示例见 [API 文档](api.md)。

agent 授权链接 fragment 只携带请求 ID 和待确认的服务器信息。登录或首次初始化时保留原 fragment，不转为查询参数；登录后仍需点击授权。授权响应包含随机的 256 位配对秘密，10 分钟后失效，复制回终端完成配对；HTTP 下剪贴板 API 不可用时可手动选中并复制。凭据只留在当前页面内存，完成或过期后清除。

### 从管理员 Bearer 认证迁移

从 controller 配置删除 `admin_token_file`，启动后在 `/init` 创建管理员。已有 agent、绑定、部署和 app 数据保留，无需重新注册 agent。管理 API 不再接受管理员 Bearer；脚本须改为 Cookie jar、Origin 和 CSRF。agent 的 Noise 认证及 HTTP 制品下载仍使用自己的长期凭据，不受该改动影响。

agent **不进行 HTTPS 连接，也不配置 CA 或证书**。初次连接使用一次性秘密通过 `Noise_NNpsk0_25519_ChaChaPoly_SHA256` 建立认证加密通道，再向加密兑换接口领取长期 token。后续连接使用 `SHA-256(token)` 的原始 32 字节作为 Noise PSK；controller 保存的 token 摘要因此也属于认证秘密，不能公开。新 token 使用系统随机源生成 32 随机字节后编码为 64 个十六进制字符。

控制连接、凭据兑换和安装包下载使用不同用途的握手，用途、版本和身份绑定到握手。每次连接产生新会话密钥，不回退明文。包通过独立加密连接分块传输，要求结束标记、准确长度和 SHA-256 校验，不阻塞控制连接心跳。主动模式的 agent 不需要入站端口；被动模式的 agent 需开放其监听端口。两种模式均不继承 HTTP 代理环境变量。

授权兑换结果与 agent 身份在 SQLite 事务中原子保存，有效期内重复兑换返回同一份凭据。agent 持久化配置后通过长期凭据确认完成，controller 清除临时配对秘密；过期清理每 30 秒执行。确认丢失可重试，不会创建新 agent；清理是数据库逻辑更新，不承诺擦除历史备份或 SQLite WAL 中的旧字节。

原有 `POST /v1/agents` 手动注册 API 保留，返回 `id` 和 `token`，仍可手动配置 agent。管理 API 与原有 HTTP 产物接口仅通过受保护的反向代理访问；agent 自身的部署包下载使用加密 TCP。默认每 15 秒上报状态，controller 45 秒无消息判定失联，连接发起方按 1–60 秒退避重连。

### 从 TLS 协议 v1 迁移

以下仅描述通信协议迁移。升级到多蓝图版本还需遵守[账户与部署状态的兼容边界](#用户目录和守护行为)，旧的逐应用部署状态不能直接恢复。controller 和 agent 必须一起升级，v2 不接受旧 TLS 连接，也不自动降级。

- controller 将 `https_listen` 替换为 `http_listen`，移除 `tls_cert`、`tls_key`，直接通过 HTTP 访问或设置可选 HTTPS 反向代理；在 Web 初始化或设置页配置 `public_url` 和 `agent_endpoint`。
- agent 移除 `ca_cert`、`controller_https`、`controller_server_name`；保留 `agent_id`、`token_file`、`controller_tcp`、`state_dir` 和运行选项。
- 保留原 token、SQLite 数据、app 用户和数据目录，无需重新注册。Rust 调用方相应更新 `Config` 字段。
- 安排维护窗口重启双方；agent 本地 app 在 controller 暂时离线期间继续运行。

## 定义仓库与变量

controller 仅在手动同步请求时拉取配置的仓库和分支，生成固定 Git commit 的快照；启动、初始化、配置保存和重启都不会自动同步。递归扫描 `pier-pkg.yml` 和 `pier-blueprint.yml`，以所在目录相对仓库根的路径作为 ID；根目录自身的 ID 为 `.`。不跟随符号链接，不展开 Git 子模块。

app 沿用 `pier-pkg` schema 2。完整 blueprint 示例见 [pier-blueprint.yml](../examples/blueprints/web/pier-blueprint.yml)：

```yaml
schema: 1
name: web-server
variables:
  DB_HOST: {}
  API_PORT: {default: "8080"}
apps:
  - id: api
    app: apps/api/1.0.0
    variables:
      DB_HOST: "{{ DB_HOST }}"
      PORT: "{{ API_PORT }}"
```

`apps` 是有序列表。实例 `id` 在 blueprint 内唯一，允许字母、数字、下划线和连字符，最长 80 字符；同一个 app 定义可以用不同实例 ID 引用多次。blueprint 路径与实例 ID 共同决定服务器上的实例身份，改变其中任一个会被视为移除旧实例、创建新实例。

变量处理顺序：绑定提供的值覆盖 blueprint 默认值；`apps[].variables` 使用这些值进行一次严格模板渲染；结果作为 `PackOptions.variables` 传入 app；没有映射的 app 变量使用 app 自身默认值。所有变量值和默认值必须是字符串。blueprint 必填变量必须提供，app 必填变量必须映射，未知变量和重复映射键报错。blueprint 变量声明、默认值、app 引用路径和实例 ID 不进行模板渲染。

`GET /v1/blueprints` 返回 blueprint 映射及所有 app 的变量声明，便于管理客户端生成填写表单。`GET /v1/apps` 单独返回 app 声明。app 的名称和版本可能仍含模板；目录 ID 不依赖渲染值。绑定查询仅返回已填写变量名，不返回其值。

一个 agent 可以绑定多个不同 blueprint，同一个 blueprint 在该 agent 上只绑定一份，也可以绑定到其他 agent。每个绑定分别保存变量，通过 `/v1/agents/{id}/bindings` 管理。建立或修改绑定、同步 Git 均不触发部署。无效的新目录快照不会替换上次可用快照，查询仓库状态可以看到同步错误。

## HTTP API 与部署

完整接口列表、鉴权方式、请求与响应、错误状态和调用示例见 [controller API 文档](api.md)。部署前先查询定义并绑定 blueprint，再明确提交目标 blueprint 路径、当前 commit 与各源码实例所需的构建镜像；具体参数约束统一在 API 文档中维护。

接口返回任务 ID，controller 在后台打包，默认最多同时处理两个服务器的构建任务。全部包构建完成后才通知 agent，因此打包失败不会改变服务器。每次使用独立产物目录，避免同名包及不同服务器的配置相互覆盖。

客户端通过查询部署详情跟踪[任务状态](api.md#部署状态)。部署结果保存在 SQLite，断线重连后对账；重复任务使用任务 ID 和计划摘要去重。controller 在构建中重启时，该构建标记失败，需要重新调用部署 API；已经交给 agent 的任务会恢复结果同步。

agent 在修改服务前下载并验证所有包，包括整体 SHA-256、目标架构、文件清单、文件哈希和启动文件；拒绝路径越界、软硬链接、特殊文件及未声明文件。包解压上限为 10 GiB、100000 个条目，清单上限为 4 MiB。随后持久化原部署，按列表顺序更新 app。默认启动后连续存活 10 秒视为成功，可通过 agent 配置调整。

各 blueprint 独立部署、停止和回滚；同一 agent 同时最多执行一个部署或停止任务，其他 blueprint 持续运行。任何 app 启动失败只回退所属 blueprint：停止本次新版本，恢复之前的程序、配置和 app 列表。数据目录不参与回退；数据库迁移等外部副作用也不回滚。部署中 agent 重启时使用事务记录恢复目标 blueprint 之前的成功部署，并恢复其他 blueprint 各自的快照。首次部署失败会停止本次已启动的 app。回退失败的 app 仍会按退避策略尝试恢复，任务明确记录 `rollback_failed`。

## 用户、目录和守护行为

每个蓝图使用 `pier-blueprint.yml` 的 `name` 原文创建系统用户及同名组，禁用登录。例如 `name: Web.Site` 创建 `Web.Site` 用户和组，不添加前缀、不转换大小写，也不对蓝图名称做模板渲染。名称必须符合 `[A-Za-z_][A-Za-z0-9_.-]{0,31}`；不合法时拒绝绑定或部署。应用的包名不再承担账户命名职责，继续使用打包库原有规则。

同机不同蓝图不能占用同名用户或组，包括已停止、已解除绑定但保留的账户。controller 在构建前检查名称及已知的蓝图冲突；agent 验证全部制品和账户后才创建目录或停止原服务。不接管系统用户、其他蓝图或其他 agent 的账户。

蓝图身份为仓库相对路径的 SHA-256；应用身份仍由蓝图路径与 `apps[].id` 共同决定。升级可以更改应用版本和 recipe 目录，但必须保持蓝图路径和蓝图 `name` 不变。agent 验证保存的账户归属、UID/GID 和主目录后复用原用户。改名或账户身份异常会拒绝部署。

同一蓝图的全部应用共享账户、主目录和 `PIER_DATA_DIR`，可以相互访问数据。各应用分别保存程序版本和日志，`PIER_LOG_DIR` 指向该应用的日志目录。应用应自行协调共享文件名及数据格式；共享数据不参与部署回滚。

```text
/var/lib/pier-agent/
├── agent.db
├── downloads/
└── blueprints/<蓝图身份摘要>/
    ├── data/                         # 共享 HOME / PIER_DATA_DIR
    └── apps/<apps[].id>/
        ├── releases/<部署ID>-<SHA256>/
        └── logs/                     # 该应用的 PIER_LOG_DIR
```

任何应用退出（包括退出码 0）都会停止并重启所属蓝图的全部应用。agent 先向各进程组发送 SIGTERM，超时后强制结束，随后对该蓝图用户执行一次残留进程清理，再按应用列表顺序启动新进程。自动重启间隔从 1 秒退避到 60 秒，整组稳定运行 60 秒后重置；其他蓝图不受影响。自动重启保留合法的终端会话，显式部署或停止只关闭目标蓝图的终端。

controller 断线时蓝图继续运行和自动恢复。agent 正常退出会停止全部蓝图；SIGKILL 后由下次启动校验账户、清理遗留进程并从各自本地快照恢复。不要将蓝图专属用户用于其他手工服务，它们也会被纳入该蓝图的进程清理。

在控制台执行“停止蓝图”后，蓝图进入 `stopped`，绑定、账户、数据和历史版本保留。停止不依赖当前 Git 目录仍有该蓝图。agent 在线且蓝图已停止时才可“解除绑定”；解除绑定不执行部署，也不删除账户或数据。以后重新绑定相同路径、相同名称的蓝图会复用原账户及共享数据。部署 `apps: []` 同样停止该蓝图的应用。

账户和目录规则面向全新部署，不自动合并旧的应用数据或迁移 `pier_<身份摘要>`、应用包名账户。发现旧的逐应用部署状态会明确拒绝启动，要求运维先备份并处理旧部署；不会静默接管或忽略旧账户。协议保持 v2，新功能通过 `multi_blueprint_v1` 能力协商；旧 agent 仍可连接和使用原升级通道，但不能接收多蓝图部署任务。

## 代理与边界

定义仓库同步与 app 下载、Git 和构建共用 controller 当前生效的 `build_proxy`，首次在 Web 初始化的“仓库同步与构建代理”中设置，以后在控制器设置页修改或清除并重启生效。app 是否使用代理仍由其 `proxy.enabled` 控制；定义仓库同步直接使用此配置。

HTTP 定义仓库使用 `http_proxy`，HTTPS 定义仓库使用 `https_proxy`，两字段不互相回退；代理地址仅支持 `http://` 和 `https://`，支持用户名和密码。`no_proxy` 按 Git/libcurl 规则指定直连主机。未配置对应代理时明确直连，忽略环境变量及 Git 中的 HTTP 代理设置。代理故障不回退直连，TLS 证书校验保持开启。SSH 与本地定义仓库继续使用 controller 运行账户的 Git/SSH 连接方式。修改代理不会自动同步或使现有目录失效；同步失败保留上次成功的目录。

Docker 拉取镜像仍使用 Docker daemon 的代理设置。agent 连接和部署包下载直接访问配置的 controller，不继承环境中的 HTTP 代理。

当前为单管理员控制台，应用定义仍由 Git 管理；不提供 HTTP 健康检查、自动数据库迁移、零停机升级或多 controller 高可用。启动成功仅表示进程在观察窗口内持续运行，之后异常退出按自动拉起策略处理。

## 验证

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Rust、前端与发布辅助脚本测试由 CI 执行。原生安装包和 systemd 验证由 Packages 工作流执行，在三种系统、两种架构上覆盖安装、配置与身份保留、真实部署、服务重启、修订号 `N → N+1` 自动升级、禁止降级、失败暂停及手动恢复。测试只使用 Actions 中的临时特权容器，不挂载宿主 cgroup 或 Docker socket。

Ubuntu 的迁移测试先安装带旧式数字修订号的测试 DEB，部署 app，再手动安装带 `.ubuntu24.04` 的正式包，验证配置、token 和服务恢复，并继续验证下一修订号自动升级。此夹具复用本次构建的二进制，只模拟旧式包元数据；它与正式产物隔离，不进入 bundle 或 Release。工作流保留打包与 systemd 日志，实际支持验证以对应提交的 Actions 结果为准。

`pier-pkg` 的应用 tar.gz 打包、`inspect(service_dir)` 与 `unpack(package, destination, sha256, architecture)` 仍供运行中的 controller 或库调用方使用，不依赖 GitHub Actions。

原生包初始实现（2026-09-27）验证结果：`cargo fmt --all --check`、Clippy（`-D warnings`）和当时的 34 项默认测试通过。另在以下 Docker 环境中执行了真实服务生命周期和原生包测试，ARM64 使用 QEMU：

| 系统 | 架构 | 服务生命周期 | 安装 / init / 升级 / 恢复 / 卸载 |
| --- | --- | --- | --- |
| Ubuntu 24.04 | amd64 | 通过 | DEB 通过 |
| Ubuntu 24.04 | arm64 | 通过 | DEB 通过 |
| AlmaLinux 8 | x86_64 | 通过 | RPM 通过 |
| AlmaLinux 8 | aarch64 | 通过 | RPM 通过 |

Web 初始化与运行设置改造（2026-09-27）验证：49 项 Rust 默认测试（含文档测试）、格式检查、Clippy（`-D warnings`）通过；前端格式、TypeScript 和内嵌资源重建一致性检查通过。5 项 Chromium HTTPS 测试覆盖首次初始化、运行设置立即生效与保存后待重启、Cookie/CSRF、退出、仓库同步、变量保留与修改、部署冲突重试与进度、接入链接登录与配对、改密、会话失效及手机尺寸页面，且无 CSP 违规或未捕获脚本错误。Rust 回归另覆盖初始化端口冲突无部分提交、监听失败后修复、旧 YAML 单次迁移及重启后设置生效。部署表单的在线 agent 和任务进度使用浏览器测试替身，真实部署由独立系统测试覆盖。

Controller 包测试使用独立 systemd 容器，内部启动测试专用 Docker daemon，不挂载宿主 Docker socket 或 cgroup。Docker 静态二进制仅安装到测试镜像（[官方安装说明](https://docs.docker.com/engine/install/binaries/)）。覆盖专用用户、仅两个启动字段的默认配置、初始化前无 agent 监听、网页初始化立即启用通信、运行设置保存后手动重启生效、端口冲突时 Web 可用与修复、仅手动同步、真实二进制部署，以及 Git 源码 app 在内层 Docker 中执行构建命令后的部署；源码夹具使用复制脚本的最小构建命令，验证权限和挂载，不依赖额外语言工具链。测试还验证升级 PID 不变、Cookie/数据重启恢复、崩溃拉起和卸载保留；升级夹具使用当前发布修订号的下一值，与正式产物分别存储。

### 被动连接的 CI 验证

GitHub Actions 的每个系统/架构包测试均保留主动与被动直连场景，并增加经 SOCKS5 连接的被动场景。被动模式在一次性容器内创建独立 agent 网络命名空间，拒绝所有主动 TCP SYN，仅允许对 controller 连接的响应；覆盖交互注册、实际部署、Web Bash、地址修改、原生自动升级和重启恢复。升级辅助进程也位于此网络命名空间，只写入本地升级日志；agent 主进程重新连接后补报状态。Rust 测试补充密钥、身份、通道会话绑定、重放、超时及断线清理校验。实际验证结论以对应提交的 Actions 结果为准。

GitHub Actions 的原生系统测试在 AlmaLinux 8、AlmaLinux 9、Ubuntu 24.04 的 amd64/arm64 六种组合中运行三种连接场景：agent 主动连接、controller 直连、controller 经 SOCKS5 连接。代理场景禁止 controller 直连 agent，使用仅在代理端解析的目标域名，并检查注册、控制、部署包、升级、终端均通过代理，包含错误密码修正和系统重启恢复。
