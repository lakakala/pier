# Pier

本仓库现为 Rust workspace：`pier-pkg` 保持为打包库，新增 `pier-controller` 和 `pier-agent` 两个服务。controller 从 Git 读取 app/blueprint，通过 React + Ant Design 控制台和 API 管理部署；agent 使用独立系统用户运行 app，自行守护进程并处理整组回退。controller 和 agent 均提供 DEB/RPM。controller 的 YAML 只保留 Web 监听地址和数据目录；首次访问 `/init` 创建管理员，配置仓库、agent 通信和构建参数，立即生效。后续在控制器设置页修改运行参数并手动重启，定义仓库仅手动同步。管理 API 使用 Cookie 会话。配置和运行说明见 [服务部署文档](docs/services.md)，HTTP 接口见 [controller API 文档](docs/api.md)。

agent/controller 共用根 `Cargo.toml` 的 `[workspace.package].version`。每次推送 `master`，GitHub Actions 自动构建十二个 DEB/RPM 包及 `SHA256SUMS`，验证后创建标签并发布 GitHub Release，无需手动打标签。相同 Cargo 版本依次发布为 `vX.Y.Z`、`vX.Y.Z-r1`、`vX.Y.Z-r2`，对应安装包修订号 `1`、`2`、`3`。详见 [构建与发布说明](docs/services.md#github-actions-构建与发布)。

`pier-agent` 提供 Ubuntu 24.04 的 DEB 包和 AlmaLinux 8/9 的 RPM 包，支持两种架构。安装后执行 `sudo pier-agent init`，按交互向导在 controller 网页授权，完成后自动启用 systemd 服务。agent 在握手时检查 controller 内置的原生包，自动下载并在部署空闲后升级、重启；只升级更高版本，失败后暂停该目标并手动恢复。通信与安装包下载使用 Noise 加密，重启时应用会短暂中断。旧 Ubuntu agent 需手动安装一次带 `.ubuntu24.04` 版本后缀的新包并重启，随后继续自动升级。EL8/EL9 包分别使用 `.el8`/`.el9`；服务安装包及其安装验证全部由 GitHub Actions 生成和执行。

Rust 同步库：从服务目录读取 `pier-pkg.yml`，获取 Git 源码并在 Docker 中编译，或下载上游二进制，渲染配置后生成 `tar.gz`。没有 `pier-pkg` 命令行程序。

每次调用明确传入 **编译镜像和架构**，只生成一个 Linux 包。架构支持 `amd64`、`arm64`；源码模式必须传入镜像名或 digest，二进制下载模式无需 Docker，也不能传入镜像。跨架构使用 **Docker `--platform` + QEMU/binfmt**，容器内使用目标架构的原生 Rust/Go/GCC，不使用 Zig。

## 项目结构

```text
Cargo.toml                 # workspace 配置及两个服务的共享版本
Cargo.lock                 # 四个 crate 共用的依赖锁文件
crates/
├── pier-pkg/               # 打包库，包含自己的 src/ 和 tests/
├── pier-protocol/          # controller 与 agent 的通信协议
├── pier-controller/        # 定义仓库、注册和部署管理
└── pier-agent/             # 服务部署和进程守护
docs/                      # 服务运行说明和 HTTP API
examples/                  # 共享服务定义与配置示例
docker/                    # 编译及测试镜像
scripts/                   # 打包和验证脚本
```

根目录的 Cargo 命令默认选择全部四个成员；单独操作打包库时使用 `-p pier-pkg`。库入口和集成测试已迁至 `crates/pier-pkg/`，库说明见 [pier-pkg README](crates/pier-pkg/README.md)。

## 调用库

在调用方的 `Cargo.toml` 中添加本地依赖：

```toml
[dependencies]
pier-pkg = { path = "../pier/crates/pier-pkg" }
```

```rust,no_run
use pier_pkg::{pack, validate, Architecture, PackOptions, ProxyOptions};
use std::collections::BTreeMap;

# fn main() -> pier_pkg::Result<()> {
let options = PackOptions {
    variables: BTreeMap::from([("DB_HOST".into(), "db.internal".into())]),
    image: Some("pier-builder-rust:almalinux8".into()),
    output_dir: "dist".into(),
    proxy: ProxyOptions {
        http_proxy: Some("http://proxy.internal:7890".into()),
        https_proxy: Some("http://proxy.internal:7890".into()),
        no_proxy: Some("localhost,127.0.0.1,.internal".into()),
    },
    ..PackOptions::new(Architecture::Arm64)
};

// 预检无网络和 Docker 调用，也不写入输出文件。
let report = validate("examples/recipes/rust/1.0.0", &options)?;
println!("预计生成 {}", report.package.path.display());
let artifact = pack("examples/recipes/rust/1.0.0", &options)?;
println!("{} {}", artifact.path.display(), artifact.sha256);
# Ok(())
# }
```

`PackOptions::new(architecture)` 必须显式指定架构，没有默认架构。需要多个包时，由调用方循环调用 `pack()`。二进制模式使用 `PackOptions::new(Architecture::Amd64)`，保持 `image: None`。`overwrite` 默认关闭；使用 `overwrite: true` 原子替换已有普通文件。默认输出目录 `dist/` 相对调用时的工作目录。

`Error` 提供 `stage`、`architecture`、`path`、`message` 和底层错误。失败不发布新包，也不会替换已有包。日志通过 `tracing` 发出；库不初始化全局订阅、不修改调用方环境或工作目录。同步 API 在异步应用中应放到 `spawn_blocking` 等阻塞线程执行。

## 服务目录和 YML

```text
demo/1.0.0/
├── pier-pkg.yml
├── configs/
│   └── app.toml
└── assets/
    └── message.txt
```

每个版本一个目录，不递归寻找其他服务。`schema: 2`；未知字段、重复映射键均报错。镜像和架构只从本次调用参数获取。完整示例位于 `examples/recipes/`；示例来源地址需要替换为实际仓库或发布地址。

源码配置示例：

```yaml
schema: 2
name: demo
version: "1.0.0"
variables:
  DB_HOST: {}               # 必须由调用方提供，即使模板未使用
  PORT: {default: "8080"}
proxy:
  enabled: false
source:
  type: git
  repo: https://example.com/demo.git
  ref: v1.0.0              # branch、tag 或 commit；本地路径相对服务目录
build:
  language: rust
  workdir: .
  env: {}
  commands:
    - cargo build --release --locked
    - cp target/$PIER_RUST_TARGET/release/demo /output/demo
files:
  - from: demo
    to: bin/demo
    executable: true
  - base: recipe
    from: assets/message.txt
    to: share/message.txt
service:
  command: [bin/demo, --config, configs/app.toml]
  working_dir: .
  env:
    RUST_LOG: info
```

`source.type: binary` 时省略 `build`，在 `source` 中配置 `url`、`format`（`raw`、`tar.gz`、`zip`）及可选 `sha256`；文件映射始终放在顶层 `files`。裸文件在产物根目录命名为 `download`，压缩包保留内部目录。例如：

```yaml
source:
  type: binary
  url: "https://example.com/releases/demo-linux-{{ PIER_ARCH }}.tar.gz"
  format: tar.gz
files:
  - from: "demo-{{ PIER_ARCH }}/demo"
    to: bin/demo
    executable: true
```

有不同架构的校验值时，可声明带默认值的 `SHA_AMD64`、`SHA_ARM64`，然后填写 `source.sha256: "{{ SHA_AMD64 if PIER_ARCH == 'amd64' else SHA_ARM64 }}"`；两个默认值均应为实际下载文件的 SHA-256。

`files` 支持文件和目录，`base` 默认 `artifact`（构建的 `/output` 或下载解压目录），`recipe` 表示服务目录。源路径、目标路径和服务路径必须相对根目录，不能含 `..`。映射目标不得重叠；`configs/` 和 `manifest.yml` 是保留路径。显式的 `executable: true` 将所映射文件设为 `0755`，其他文件设为 `0644`。

服务 `command[0]` 必须指向包内可执行文件，路径相对包根目录；`working_dir` 同样相对包根目录，默认 `.`。这些是供调用方启动服务时使用的元数据，库不会安装或启动服务。

## 变量和配置模板

变量同时用于 **YML 的字符串字段**和 `configs/` 的文本内容，使用 `{{ NAME }}`。例如 `version: "{{ VERSION }}"`、`source.ref: "v{{ VERSION }}"`、下载 URL、构建命令、文件映射路径、服务参数和环境值均支持替换。YML 先解析再渲染各字段，因此包含引号、冒号和换行的变量仍然是一个字符串。映射键、布尔值、枚举选择字段（如 `source.type`、`build.language`、`format`）保持字面值。

**内置 `PIER_ARCH`** 自动取本次调用的 `architecture`，值为 `amd64` 或 `arm64`，在 YML 字符串字段和 `configs/` 中均可直接使用，无需声明。禁止在 `variables`、调用方变量或 `build.env` 中覆盖它；未知架构在构造 `Architecture` 时拒绝。

用户变量及默认值使用字符串；变量声明和默认值不做模板求值。调用方值覆盖默认值，空字符串有效；未知变量、全部缺失的必填变量和未定义模板变量在下载前报告。变量不读取环境、不引用其他变量，也不二次求值。需要在构建命令中传入外部值时，优先通过 `build.env` 声明并在 shell 中使用带引号的环境变量引用。

`configs/` 可省略。存在时自动递归渲染全部 UTF-8 文件（包括点文件），保持名称和相对目录，放到包内 `configs/`。文件名不渲染，也不删除 `.j2` 等后缀。二进制资源应放在其他目录并显式映射。

```jinja
# configs/app.toml
listen = "0.0.0.0:{{ PORT }}"
database_host = {{ DB_HOST | tojson }}
```

使用 MiniJinja 原生语法，可用过滤器、条件、循环和仅在 `configs/` 范围内的 include。每个文件都会输出，包括被 include 的文件。严格未定义检查、关闭 HTML 自动转义、保留末尾换行；根据目标配置格式使用适当的序列化过滤器（如 `tojson`），库不猜测文件格式。

## 代理

唯一开关为 YML 中的 `proxy.enabled`，默认 `false`；调用方只提供地址。

- 开启时至少提供一个 HTTP(S) 代理地址；HTTP/HTTPS 分别配置，没有配置的协议直连。`no_proxy` 是逗号分隔的绕过列表，其细节遵循各底层工具。
- 下载请求使用显式代理；Git 和容器注入大小写两组 `HTTP_PROXY`、`HTTPS_PROXY`、`NO_PROXY`，清空未设置项和 `ALL_PROXY/all_proxy`。
- 关闭时忽略调用方代理地址（包括无效地址），禁用下载客户端环境代理，清除 Git 继承代理并覆盖 Git HTTP(S) 代理设置，显式覆盖容器默认代理环境。
- 代理环境仅作用于本次调用，配置不能放进 `build.env`。构建脚本自身仍是受信任代码，可以自行执行网络命令。
- 容器使用默认 bridge 网络；代理地址必须能被宿主及容器访问。源码模式拒绝 `localhost`、回环 IP；仅下载模式可以使用本机代理。
- Git SSH 不转换为 HTTP 代理。Docker 拉取镜像使用 daemon 的代理设置，独立于容器内环境。不要将 `127.0.0.1` 误认为容器外的宿主。

## 构建镜像与 QEMU

基础镜像是 `almalinux:8.10` 和 `ubuntu:24.04`。按语言和发行版提供四个**独立 Dockerfile**，每个文件直接声明基础镜像、系统依赖和工具链安装步骤。没有 Bake 配置或共享安装模板。镜像内使用**目标架构的原生 GCC/系统库**。默认 Rust `1.98.1`、Go `1.27.1`，通过构建参数可更改。自定义服务可以使用自己的兼容镜像；发布时建议固定镜像 digest。

Linux Docker Engine 执行异架构容器需要 QEMU/binfmt。管理员可参照 [Docker 官方文档](https://docs.docker.com/build/building/multi-platform/) 安装，例如在 amd64 上启用 arm64：

```sh
docker run --privileged --rm tonistiigi/binfmt --install arm64
docker run --rm --platform linux/arm64 ubuntu:24.04 uname -m
```

库不会自动注册 binfmt 或运行特权容器。Docker Desktop 的 Linux VM 通常已配置仿真。本库的源码打包入口目前面向本机 Linux Docker daemon；挂载工作目录须能被 daemon 访问。

以下为可选的编译环境示例，可改用自有镜像；`pack()` 不自动构建镜像。镜像须包含 YML 命令所需的工具链、`/bin/sh` 和 `uname`。在项目根目录分别构建，每种镜像包含 amd64/arm64 两个平台：

```sh
docker buildx build --platform linux/amd64,linux/arm64 -f docker/rust-almalinux8.Dockerfile -t pier-builder-rust:almalinux8 --load .
docker buildx build --platform linux/amd64,linux/arm64 -f docker/rust-ubuntu2404.Dockerfile -t pier-builder-rust:ubuntu24.04 --load .
docker buildx build --platform linux/amd64,linux/arm64 -f docker/go-almalinux8.Dockerfile -t pier-builder-go:almalinux8 --load .
docker buildx build --platform linux/amd64,linux/arm64 -f docker/go-ubuntu2404.Dockerfile -t pier-builder-go:ubuntu24.04 --load .
```

此命令需要支持多平台镜像存储的 Docker（如启用 containerd image store）。若构建阶段需要调用方的代理，可按实际环境变量名称加 `--build-arg http_proxy --build-arg https_proxy --build-arg no_proxy`（使用大写变量时改为对应大写名称）。镜像拉取代理仍由 daemon 单独设置。修改 Go 版本时应一并更新 Dockerfile 中的两个官方 SHA-256 构建参数。

镜像名：`pier-builder-rust:almalinux8`、`pier-builder-rust:ubuntu24.04`、`pier-builder-go:almalinux8`、`pier-builder-go:ubuntu24.04`。

运行构建时，库选择 `--platform linux/amd64` 或 `linux/arm64`，校验容器 `uname -m`，以宿主 UID/GID 执行命令。源码挂载 `/src`，产物写 `/output`；命令在同一 `/bin/sh -ec` 中执行。提供 `PIER_ARCH`（如 `arm64`）、`PIER_TARGET`（如 `linux/arm64`）、`PIER_RUST_TARGET`、`PIER_OUTPUT`；Rust 配置 `CARGO_BUILD_TARGET`，Go 配置 `GOOS/GOARCH`、默认 `CGO_ENABLED=0`。这些变量为保留字段，不能通过 `build.env` 覆盖。

Go 使用 CGO 时可配置 `build.env.CGO_ENABLED: "1"`；它在目标架构容器中使用原生编译器。Rust/Go 的额外系统开发库由扩展镜像提供。Git 认证使用宿主已有的 Git/SSH 配置；子模块或 LFS 等额外获取步骤由服务构建命令明确安排。

## 兼容检查与产物

输出 `<name>-<version>-linux-<架构>.tar.gz`，包含程序、资源、已渲染的配置和 `manifest.yml`。清单使用 `schema: 2`，记录 `os: linux`、`architecture`、源码模式的 `image`（调用方传入的镜像名称或 digest）、运行信息、Git commit、文件 SHA-256、权限，以及 ELF 的动态库和 glibc 符号要求。不保存变量声明或打包代理；用于服务参数、环境和配置的变量值会随相应内容打入包内。浮动镜像名称不会自动解析或固定到 digest。

库校验包内 ELF 的 CPU 架构、64 位和小端格式，**不再根据发行版限制 glibc 版本**。AlmaLinux 8、Ubuntu 24.04 的兼容性取决于编译镜像、系统动态库及程序依赖；应选择对应环境的镜像并在目标系统验证。下载的二进制同样接受架构检查。库记录动态库和 glibc 要求，不自动收集系统共享库。

同一名称、版本、架构使用不同镜像仍会生成相同包名。需要保留多个镜像的产物时请指定不同 `output_dir`，默认禁止覆盖。

归档统一顺序、uid/gid、时间和权限，相同已收集文件产生相同归档。Git 分支、远端 URL、浮动镜像或编译器自身的非确定性不在此保证之内。拒绝软/硬链接、特殊文件、路径越界及重名；每个包临时写入后原子发布，失败时保留已有文件。

构建镜像运行受信任的项目代码与构建命令；本库不是不可信构建沙箱。正常返回或失败会清理临时目录和自有容器；调用方应自行管理进程生命周期。

## 测试

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
# 单独运行打包库测试（包含文档测试）
cargo test -p pier-pkg --locked
# 先准备上述镜像与 QEMU；验证两种语言、四种目标的构建/解包/运行
cargo test -p pier-pkg --test docker --locked -- --ignored --nocapture
```

Docker 矩阵测试使用临时本地 Git 仓库，不依赖示例 URL。可用 `PIER_TEST_TARGETS=ubuntu24.04/amd64` 和 `PIER_TEST_LANGUAGES=rust` 缩小本地验证范围。容器测试覆盖用户态与 ABI；目标系统内核兼容性仍须在真实系统或完整虚拟机验证。

2026-09-26 已重新构建四个独立 Dockerfile 的双架构镜像，并用新 API 分批完成以下验证。每种语言使用同一份 YML，仅在每次调用时切换镜像和架构：

| 编译及运行环境 | Rust | Go |
| --- | --- | --- |
| AlmaLinux 8 / amd64 | 编译、打包、运行通过 | 编译、打包、运行通过 |
| AlmaLinux 8 / arm64 | 编译、打包、运行通过 | 编译、打包、运行通过 |
| Ubuntu 24.04 / amd64 | 编译、打包、运行通过 | 编译、打包、运行通过 |
| Ubuntu 24.04 / arm64 | 编译、打包、运行通过 | 编译、打包、运行通过 |

测试还确认包内配置的 `PIER_ARCH`、清单中的镜像/架构和 ELF 依赖记录正确；编译返回 42 时不发布归档。常规单元及集成测试、文档示例、格式检查和 Clippy 均通过。

## 从 schema 1 迁移

此版本对 YML 和库 API 做了不兼容调整；旧配置会返回明确的迁移错误：

1. 将 YML 改为 `schema: 2`，删除 `targets`，把文件映射移至顶层 `files`；二进制下载字段移至 `source`。
2. 将按架构变化的 URL、校验值和路径改为使用 `PIER_ARCH` 的表达式。发行版不同的二进制来源由服务配置或普通用户变量显式表达。
3. 将 `PackOptions::default()` 和 `targets` 列表改为 `PackOptions::new(architecture)`，源码模式通过 `image` 传入镜像。
4. `pack()` 直接返回 `PackageArtifact`，`validate()` 的计划为 `report.package`。架构字段为 `architecture`；`Target`、`Distribution`、`PackReport` 和 `Error.completed` 已移除。
5. 包名和清单不再含发行版；需要区分不同编译镜像的产物时，使用不同输出目录。构建环境的 `PIER_TARGET` 改为 `linux/<架构>`。
