# pier-pkg

Rust 同步打包库：读取服务目录中的 `pier-pkg.yml`，下载 Git 源码并通过 Docker 编译，或直接下载二进制文件，渲染配置后生成 Linux `tar.gz` 部署包。支持 `amd64` 和 `arm64`，跨架构构建使用 Docker 与 QEMU。

## 调用

在仓库外的调用项目中添加本地依赖，路径按实际位置调整：

```toml
[dependencies]
pier-pkg = { path = "../pier/crates/pier-pkg" }
```

以下示例从仓库根目录运行，使用仓库内的服务定义；先将定义中的上游地址替换为实际仓库，并准备所指定的 Docker 镜像。

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

// 预检不访问网络或 Docker，也不写入输出文件。
let report = validate("examples/recipes/rust/1.0.0", &options)?;
println!("预计生成 {}", report.package.path.display());
let artifact = pack("examples/recipes/rust/1.0.0", &options)?;
println!("{} {}", artifact.path.display(), artifact.sha256);
# Ok(())
# }
```

每次调用必须指定架构；源码模式还必须指定编译镜像，二进制模式不接受镜像。声明的变量无默认值时必须通过 `PackOptions.variables` 提供。默认禁止覆盖已有包，默认输出目录为调用方工作目录下的 `dist/`。

| API | 用途 |
| --- | --- |
| `inspect(service_dir)` | 读取未渲染的 app 元数据与变量声明 |
| `validate(service_dir, &options)` | 检查定义、变量和本次打包参数，返回计划 |
| `pack(service_dir, &options)` | 获取程序、渲染配置并打包，返回产物路径与摘要 |
| `unpack(package, destination, sha256, architecture)` | 验证部署包并解压到不存在的新目录 |

库不初始化日志订阅器，不修改调用方环境或工作目录；异步调用方应在阻塞线程中执行同步接口。完整 YML、镜像构建及兼容说明见[仓库使用文档](../../README.md)。

## 测试

从仓库根目录执行：

```sh
cargo test -p pier-pkg --locked
cargo clippy -p pier-pkg --all-targets --locked -- -D warnings
# 可选：准备构建镜像与 QEMU 后执行 Docker 集成测试。
cargo test -p pier-pkg --test docker --locked -- --ignored --nocapture
```

库的集成测试位于本 crate 的 `tests/`，共享的服务定义、构建镜像和脚本保留在仓库根目录的 `examples/`、`docker/` 和 `scripts/`。
