# Voiceless 非 Nix 安装与更新调研

## 建议

采用 **同一版原生 Linux 构建，两种安装入口**。Bun 用户执行 `bun add -g <包名>`，独立安装用户从 GitHub Releases 下载并校验相应压缩包，解包后将命令安装到 `~/.local/bin/voiceless`。两种入口都提供 `voiceless update`，由当前安装的管理工具完成更新。独立安装无需 Nix、Rust 工具链、Bun、Node.js 或 `sudo`。首批验证 glibc 标准 CPU 构建，按架构公布实测支持范围。 [GitHub Releases](https://docs.github.com/en/repositories/releasing-projects-on-github/about-releases) · [Bun 全局安装](https://bun.com/docs/pm/cli/add#global)

当前 CLI 有 `model`、`doctor`、`daemon` 和 `ctl`，顶层没有 `update`。现有 CI 在 Ubuntu runner 里用 Nix 运行 Rust 测试和 flake 检查，未生成非 Nix 发布包。本次读取项目的 GitHub Release 列表为空，公有 npm 注册表上 `voiceless` 返回 404。这些查询未确认 npm 名称的可注册性或所有权。本文中的安装和回滚接口是设计建议。 [CLI 源码](../rust/src/main.rs#L32-L70) · [CI 工作流](../.github/workflows/ci.yml) · [项目 Releases](https://github.com/Vitus213/voiceless/releases) · [npm 包元数据](https://registry.npmjs.org/voiceless)

## Bun 全局安装与 bunx

| 场景 | Bun 的行为 |
| --- | --- |
| 安装到用户环境 | `bun add -g <包名>` 或 `bun install -g <包名>` 写入 Bun 的全局项目，默认目录 `~/.bun/install/global`。Bun 根据包的 `bin` 字段在 `~/.bun/bin` 建立命令链接，目录进入 `PATH` 后即可直接调用 `voiceless`。路径可由全局 `.bunfig.toml` 的 `install.globalDir`、`install.globalBinDir` 或 `BUN_INSTALL_GLOBAL_DIR`、`BUN_INSTALL_BIN` 指定。 [Bun 全局安装](https://bun.com/docs/pm/cli/add#global) · [Bun 配置](https://bun.com/docs/runtime/bunfig#install-globaldir) · [Bun v1.4.5 命令链接源码](https://github.com/oven-sh/bun/blob/bun-v1.4.5/src/install/bin.rs#L1323-L1399) |
| 只更新 Voiceless | `bun update -g <包名>` 遵循全局项目已有的版本范围，`bun update -g --latest <包名>` 可跨范围跟随 `latest`，但文档明确说明它不会把已高于 `latest` 的版本降级。`bun add -g --exact <包名>@<版本>` 可明确重装一个仍可取得的版本，并把全局依赖固定到该版本。`bun upgrade` 更新的是 Bun 本身。 [Bun 更新](https://bun.com/docs/pm/cli/update#global) · [Bun `--latest`](https://bun.com/docs/pm/cli/update#latest) · [Bun 精确安装](https://bun.com/docs/pm/cli/add#exact) · [Bun 升级](https://bun.com/docs/installation#upgrading) |
| 一次性运行 | `bunx` 与 `bun x` 等价。省略版本时会优先使用当前环境已安装的命令或包，再用缓存和 npm。按 Bun 文档，缓存中的未限定请求每天检查一次包及其依赖，网络不可达或遇到指定的 HTTP 暂时错误时可警告后运行旧副本。显式 `@latest` 每次尝试查询注册表，显式 `@<版本>` 跳过本地包选择，`--no-install` 只使用可用的现有结果。包名与命令名不同时可用 `bunx -p <包名>@<版本> voiceless …`。 [Bun bunx](https://bun.com/docs/pm/bunx) · [Bun v1.4.5 查找及刷新源码](https://github.com/oven-sh/bun/blob/bun-v1.4.5/src/runtime/cli/bunx_command.rs#L1022-L1185) |
| 缓存和长期入口 | 注册表下载的包默认存于 `~/.bun/install/cache`，可通过 `BUN_INSTALL_CACHE_DIR` 调整。Bun v1.4.5 在 Linux 上优先在该缓存根下用 `.bunx-<uid>` 保存执行树，必要时回退到系统临时目录。bunx 不会为该次运行创建持久的全局命令，缓存路径也不适合作为服务的 `ExecStart`。 [Bun 全局缓存](https://bun.com/docs/pm/global-cache) · [Bun v1.4.5 bunx 缓存路径源码](https://github.com/oven-sh/bun/blob/bun-v1.4.5/src/runtime/cli/bunx_command.rs#L631-L682) |

`omp update` 用当前命令入口、链接目标和全局安装目录判断归属。走 Bun 分支时，它使用检查版本时选定的注册表，委托 Bun 安装确切版本的主包和平台原生组件，然后重新解析命令并验证版本。当前机器的 `omp` 来自 Bun 全局项目中的 `@oh-my-pi/pi-coding-agent`。 [OMP 安装识别](https://github.com/can1357/oh-my-pi/blob/main/packages/coding-agent/src/cli/update-cli.ts#L398-L518) · [OMP 版本验证](https://github.com/can1357/oh-my-pi/blob/main/packages/coding-agent/src/cli/update-cli.ts#L1089-L1100) · [OMP Bun 安装参数](https://github.com/can1357/oh-my-pi/blob/main/packages/coding-agent/src/cli/update-cli.ts#L1294-L1360)

Voiceless 沿用这套用户体验。根据当前运行的可执行文件、命令链接和安装记录确认归属。Bun 分支核验全局命令链接指向该 Bun 全局项目中的已安装包，使用该项目的注册表和目录配置。机器上存在 `bun` 只说明工具可用。独立安装根据本安装记录更新。Nix/Home Manager、发行版包、源码和 `bunx` 临时缓存由各自的入口管理，归属不明时提示当前路径供排查。 [Bun 全局目录配置](https://bun.com/docs/runtime/bunfig#install-globaldir) · [Home Manager 服务与安装](../nix/home-manager.nix#L105-L112)

## 原生二进制交付

| 方案 | 发布与运行特点 |
| --- | --- |
| npm 直接内置 ELF | 在 npm 包中以 `files` 收录二进制、所需共享库和许可证，用 `"bin": {"voiceless": "bin/voiceless"}` 暴露 Linux 可执行文件。Bun 创建链接后由内核执行 ELF，常用命令本身不需要 JS 运行时。单架构包可声明 `os`、`cpu`、`libc`，分别用 `x64`、`arm64` 对应 Rust 目标。或者用一个包附带两套产物，并由 `#!/bin/sh` 入口选择架构，代价是每次下载两套产物。npm 不收录符号链接，原生库若依赖 SONAME 路径，打包时必须提供实际文件及正确的文件名。 [npm 包清单](https://docs.npmjs.com/cli/v11/configuring-npm/package-json/#bin) · [npm 平台字段](https://docs.npmjs.com/cli/v11/configuring-npm/package-json/#os) · [npm 发布文件规则](https://docs.npmjs.com/cli/v11/commands/npm-publish/#files-included-in-package) · [Bun v1.4.5 二进制链接](https://github.com/oven-sh/bun/blob/bun-v1.4.5/src/install/bin.rs#L1020-L1058) |
| npm 入口包加平台子包 | 根包用同版本的 `optionalDependencies` 引用 `linux-x64-gnu`、`linux-arm64-gnu` 等子包。平台子包携带完整运行文件，根包通过启动器找到适配包并转交参数、信号和退出状态。`--omit=optional` 或下载失败可使子包缺失，启动时需指出缺哪个包。用 JS 启动器时，`#!/usr/bin/env bun` 需要 Bun，`#!/usr/bin/env node` 默认需要 Node。Bun 自己的 npm 发行证明了平台子包机制，其根包另有 `postinstall`；Voiceless 应选择无安装钩子的启动路径，因为 Bun 默认跳过未信任的依赖生命周期脚本。 [npm 可选依赖](https://docs.npmjs.com/cli/v11/configuring-npm/package-json/#optionaldependencies) · [Bun 发行打包源码](https://github.com/oven-sh/bun/blob/bun-v1.4.5/packages/bun-release/scripts/upload-npm.ts#L109-L171) · [Bun 生命周期脚本](https://bun.com/docs/pm/lifecycle) · [bunx shebang](https://bun.com/docs/pm/bunx#shebangs) |
| GitHub Release 原生包 | 同一 tag 下为每个支持的 ABI/架构发布带版本的 `tar.gz`，例如 `voiceless-vX.Y.Z-linux-x86_64-gnu.tar.gz`。归档包含原生 CLI、匹配的共享库和再分发声明。GitHub 附件上限为每个文件小于 2 GiB，API 提供资产名称、字节数、下载地址以及可能为 `null` 的 SHA-256 摘要字段。原生安装器负责选择、验证和切换文件，且可直接支持无网络回滚。 [GitHub 资产限制](https://docs.github.com/en/repositories/releasing-projects-on-github/about-releases#storage-and-bandwidth-quotas) · [GitHub 资产 API](https://docs.github.com/en/rest/releases/assets#get-a-release-asset) |

Bun 可以从 GitHub 上的 URL 安装一个符合 npm 包结构的 `.tgz`。这仍是 Bun 管理的 URL 依赖，固定版本 URL 不会自行迁移到另一条 Release 资产 URL。把 Release 二进制留给 npm 包的 `postinstall` 再下载，既受 Bun 脚本信任策略影响，也需要单独验证二次下载的资产。npm 登记的 `dist.integrity` 覆盖 npm tarball，Bun v1.4.5 在支持相应摘要且未跳过验证时会校验 tarball，完整性校验与应用的版本切换保证是两回事。其安装源码有先移开旧包再链接的路径。指定旧版本重装是联网恢复手段，Bun 文档没有给出应用级原子更新、上一个可用版本保留或离线回滚的契约。 [Bun URL tarball 依赖](https://bun.com/docs/pm/cli/add#tarball-dependencies) · [npm 发布完整性](https://docs.npmjs.com/cli/v11/commands/npm-publish/#description) · [Bun tarball 验证源码](https://github.com/oven-sh/bun/blob/bun-v1.4.5/src/install/extract_tarball.rs#L53-L68) · [Bun 流式验证源码](https://github.com/oven-sh/bun/blob/bun-v1.4.5/src/install/TarballStream.rs#L1060-L1084) · [Bun 包替换源码](https://github.com/oven-sh/bun/blob/bun-v1.4.5/src/install/PackageInstall.rs#L2114-L2205)

## Linux 运行要求

- **原生 ABI。** 在明确的 glibc 基线构建，产物需检查 ELF 解释器、`DT_NEEDED`、glibc 和 C++ 运行库版本、CPU 指令集与传递依赖。现有 flake 向包装脚本注入 Nix 路径，Cargo 依赖钉住 Sherpa-ONNX `1.13.3` 的 `shared` 特性，构建脚本链接 `sherpa-onnx-c-api` 和 `onnxruntime`。随包携带许可允许且实际需要的共享库，用 `$ORIGIN` 相对查找并验证传递依赖。`DT_RUNPATH` 只适用于该 ELF 的直接依赖，子库需有各自的搜索路径或由系统提供。具体发行版、最低 glibc 和 CPU 基线待实机验证。musl、CUDA、带 AVX2 的 GGUF 和 NixOS 上不借助 Nix 的启动能力都需分别验证后才对外承诺。 [Cargo 依赖与特性](../rust/Cargo.toml#L12-L48) · [现有 flake 包装](../flake.nix#L149-L220) · [Sherpa 构建脚本](https://docs.rs/crate/sherpa-onnx-sys/1.13.3/source/build.rs) · [Linux 动态加载器](https://man7.org/linux/man-pages/man8/ld.so.8.html)
- **桌面和音频。** 录音要有可用的 PipeWire 会话、麦克风和 `pw-record`。声音提示可用 `pw-play`。文字注入依赖 Wayland 与所选方法所需的 `wtype`、`wl-copy` 或 Fcitx5 bridge，且受 compositor 支持范围制约。常驻 `daemon` 和 `ctl` 需要用户 D-Bus 会话，Portal GlobalShortcuts 取决于对应后端，配置 evdev 快捷键时还需读取目标输入设备的权限。systemd 用户单元是常驻方式之一。 [项目运行要求](../README.zh.md#适配) · [doctor 探测逻辑](../rust/src/doctor.rs#L36-L53) · [声音提示](../rust/src/media.rs#L1-L10) · [daemon 激活来源](../rust/src/daemon.rs#L1-L14) · [Fcitx5 bridge](../rust/src/fcitx5.rs#L1-L23)
- **识别后端。** 云端识别需要网络、TLS 信任链和所选服务的凭据。本地识别沿用 `voiceless model install` 和现有模型目录。首发 CPU 包应明确列出已验证的 Sherpa 后端，标准 CPU 构建没有 GGUF 引擎。配置为 `local-offline` 的用户若选择了 `qwen3-asr-1.7b-gguf` 等 GGUF 模型，或依赖 CUDA provider，只有提供匹配且验证过的构建才适合迁移。保留配置与模型文件本身无法补齐缺失的引擎。 [项目模型与 CPU 变体](../README.zh.md#模型目录) · [本地模型默认值](../rust/src/config.rs#L352-L397) · [GGUF 特性条件](../rust/src/gguf_asr.rs#L1-L12)
- **Bun 安装入口。** 需要 Bun 来安装和更新全局包，`voiceless` 命令由原生可执行文件提供。Bun 官方 Linux glibc 版本要求 glibc 2.17 或更高，x64 要求 SSE4.2，并另发 musl 可执行文件。Voiceless 的 ABI 以自身 ELF 和共享库的实测为准。 [Bun 安装与运行要求](https://bun.com/docs/installation#musl-binaries)

| 交付变体 | 构建和验收条件 |
| --- | --- |
| 标准 CPU（x86_64、aarch64） | 锁定 Rust/Cargo、C 工具链和 `pkg-config`。两种架构分别链接匹配的 Sherpa-ONNX 1.13.3 与 CPU ONNX Runtime。上游提供共享库归档，构建可固定归档摘要。逐架构用发布资产在目标基线上完成一次真实识别。 [Sherpa 上游资产](https://github.com/k2-fsa/sherpa-onnx/releases/expanded_assets/v1.13.3) · [Cargo 特性](../rust/Cargo.toml#L12-L48) |
| CPU GGUF（x86-64-v3） | 需要 CMake 和 C++ 工具链构建 transcribe.cpp。当前 AVX2 变体还启用 FMA、F16C 和 BMI2 等指令，安装器须按完整 CPU 基线选包。GGUF 配置使用 `[local] provider = "cpu"`。 [AVX2 构建](../flake.nix#L204-L220) · [后端配置](../rust/src/config.rs#L429-L452) |
| CUDA GGUF（x86_64） | 构建需要 CUDA 12 工具链和 cuBLAS。运行需要 `cudart`、`cublas`、`cublasLt` 及宿主驱动提供的 `libcuda.so.1`。当前内核面向 SM 8.6。建议 GGUF 使用 CUDA，Sherpa/ORT 继续用 CPU 处理 Paraformer、Silero、GTCRN 和 Sherpa 离线路径。这样发布包只需验证 GGUF 实际使用的 CUDA 库，并核对其再分发条款。 [CUDA 构建](../flake.nix#L65-L145) · [链接依赖](../rust/build.rs#L18-L49) · [Sherpa CPU 路径](../rust/src/local_asr.rs#L104) |
| Fcitx5 桥接 | 用 CMake ≥3.16、C++20、ECM 和匹配宿主 Fcitx5 的 Core 与 D-Bus 开发包构建插件，最低 API 为 5.1。按发行版交付 `.so` 和 addon 描述符，重启 Fcitx5 后以有焦点的输入框验收实际提交。 [桥接构建配置](../fcitx5-bridge/CMakeLists.txt) · [桥接验证脚本](../scripts/fcitx5-bridge-test.sh) |

## `voiceless update` 与回滚约定

`voiceless update` 先识别安装归属，再按该渠道查询版本。发布选择使用当前安装的系统、架构、libc 基线和构建变体，保留标准 CPU、AVX2 GGUF 或 CUDA GGUF 的选择。CPU 指令集和 NVIDIA 驱动用于校验该变体是否可运行。

| 当前安装来源 | 版本查询与执行 |
| --- | --- |
| Bun 全局包 | 查询当前注册表的 `latest`，确认该确切版本有同变体的原生包。用 `bun add -g --exact <包名>@<版本>` 安装，主包与平台包锁到同一版本。校验目标命令的实际路径和 `--version`。恢复指定旧版同样走 Bun，能否离线取决于该归档是否仍在本地缓存。 |
| 独立安装 | 查询 GitHub Releases 稳定 tag，匹配平台与变体，下载校验后切换本机版本。上一版由独立安装器保留，支持离线回滚。 |
| Nix / Home Manager | 提示通过原 flake 输入和部署命令更新。 |

### 独立安装的版本切换

1. **识别所有权与选版本。** 第一次安装用校验过的 Release 压缩包建立用户私有、可写的程序目录，例如 `${XDG_DATA_HOME:-$HOME/.local/share}/voiceless/releases/` 下的版本树，以及 `~/.local/bin/voiceless` 的稳定入口。遇到已有命令或其他管理器的安装，先识别归属并报告 PATH 冲突。更新只接管具有本安装记录且路径、属主和当前版本相符的入口。请求 `Vitus213/voiceless` 的稳定 Release，校验 tag 与应用 SemVer，选唯一匹配当前 ABI、架构和构建变体的资产，只激活更高的稳定版。遇到相同或更旧版本时报告当前状态，遇到资产缺失或网络失败时返回明确诊断。公开 API 可匿名读，默认限额按源 IP 为每小时 60 次，交互更新可报告限流等待时间，无需收集访问令牌。 [XDG 目录约定](https://specifications.freedesktop.org/basedir/latest/#variables) · [GitHub latest 与按 tag 查询](https://docs.github.com/en/rest/releases/releases#get-the-latest-release) · [GitHub API 限流](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api#primary-rate-limit-for-unauthenticated-users)
2. **先校验，再解包。** 在 HTTPS 上从固定仓库读取资产元数据，要求该资产已上传、来源身份匹配并有有效的 SHA-256 与大小。流式下载到专用临时目录，限制大小并校验实际字节数与摘要，限制重定向为 HTTPS。随后按预定义文件布局解包，拒绝越界路径、设备文件和越出包目录的链接，校验版本、架构及必要文件，再在隔离环境调用候选 `--version` 检查基础启动。完整后端能力由发布验证覆盖。建议仓库启用不可变 Release，再把 API 摘要用于客户端的资产字节完整性检查。这个方案的信任锚是经 TLS 访问的 GitHub 仓库。GitHub Release attestation 可独立核验 tag、提交、资产与发布的绑定，仅在确实做过这项验证时报告已验证证明。 [GitHub 资产 API](https://docs.github.com/en/rest/releases/assets#get-a-release-asset) · [不可变 Release 与证明](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases) · [GitHub `verify-asset`](https://docs.github.com/en/code-security/how-tos/secure-your-supply-chain/secure-your-dependencies/verify-release-integrity)
3. **切换一个入口。** 每用户安装加互斥，锁内重新读取当前版本。把已验证的程序及共享库以版本和摘要固定在同一文件系统，生成带有当前及前一版本引用的激活记录，最后用同目录 `rename` 原子切换一条 `current` 链接。`~/.local/bin/voiceless` 经此链接到达可执行文件，回滚目标与生效版本随同一切换生效。提交前同步文件及相关目录，提交后同步链接所在目录，避免把可见的原子切换当作断电耐久性。提交前出错时保留当前入口，若切换后目录同步失败，则报告已观察到的入口及耐久性未知，便于后续恢复。现有模型管理代码有下载校验、暂存和原子指针模式可参照，应给应用更新使用独立的目录和锁。 [Linux `rename(2)`](https://man7.org/linux/man-pages/man2/rename.2.html) · [Linux `fsync(2)`](https://man7.org/linux/man-pages/man2/fsync.2.html) · [现有模型安装约定](../rust/src/models.rs#L19-L47)
4. **离线回滚一个版本。** 建议提供 `voiceless update --rollback`，用同一把锁和同一原子切换路径恢复上一份完整、已校验且兼容的本地程序与库。首次安装没有前一版本，清理时保留当前、前一版本及运行中的 daemon 仍可能使用的载荷。若新 CLI 本身无法启动，使用已保留的已验证旧版包中安装入口执行恢复，恢复入口同样应检查安装归属并走原子切换。回滚范围是程序及其库。配置、模型、历史 SQLite 及录音继续使用原路径，任何新版本的数据格式变化都须在发布前给出与上一版的兼容性。当前历史代码会拒绝高于自身支持版本的数据库，新版本若会写入旧版无法读取的格式，应明确停止承诺跨该边界的回滚。 [配置路径](../rust/src/config.rs#L821-L833) · [XDG 模型、历史及录音路径](../rust/src/models.rs#L1166-L1188) · [历史数据库版本处理](../rust/src/history.rs#L165-L209)
5. **让用户决定何时切换 daemon。** `update` 与 `--rollback` 只切换磁盘命令并报告 CLI 生效版本。已经运行的 daemon 继续服务当前会话，等用户空闲后重启。将来若提供非 Nix 用户单元，`ExecStart` 应指向稳定命令路径。已有的 Home Manager 单元引用 Nix 包的绝对路径，仍由 Home Manager 管理。切换运行方式需要单独确认当前单元、快捷键和后端配置，再让一个实例接管 `dev.voiceless.Daemon`。 [Linux `rename(2)`](https://man7.org/linux/man-pages/man2/rename.2.html) · [daemon 包装入口](../nix/home-manager.nix#L172-L198) · [daemon 与 D-Bus](../rust/src/daemon.rs#L1-L14) · [systemctl 重启](https://www.freedesktop.org/software/systemd/man/latest/systemctl.html#restart%20PATTERN%E2%80%A6)

现有 NixOS 用户首次迁移时，先为当前 CPU/GGUF/CUDA 变体验证解包后的实际启动路径。部分 NixOS 环境需宿主配置 `nix-ld` 或提供兼容的 ELF 装载器。候选版本用 `model check` 复核现有模型。若配置文件由 Home Manager 链接到 Nix store，退出该管理方式时将同样内容迁成用户所有的 `0600` 文件。保留配置路径、模型、历史库和录音，检查 Sway 快捷键与 eww 浮层是否引用旧包绝对路径。安排空闲时切换用户单元并验证 D-Bus、后端热加载或真实识别、焦点上屏和登录启动。`ctl status` 的 `idle` 只是会话快照，自动重启还需 daemon 能排空会话并拒绝新录音。迁移失败时可恢复旧单元和对应配置。 [NixOS ELF 装载机制](https://github.com/nix-community/nix-ld#how-does-nix-ld-work) · [模型检查命令](../rust/src/model_cmd.rs#L95) · [Home Manager 配置及服务](../nix/home-manager.nix#L105-L198) · [引擎健康信息](../rust/src/console/mod.rs#L466)

## 实现及发布边界

- **应用实现。** 在 Rust CLI 增加 `update` 命令和独立安装入口。Bun 安装由 Bun 写入全局目录，独立安装才维护自己的版本目录和命令链接。独立分支复用现有 HTTP、SHA-256、归档依赖及模型安装中可复用的逻辑。迁移现有 daemon、修改含凭据的配置、安装或搬迁模型、安装 Fcitx5 addon 及启停桌面服务均是独立操作。 [CLI 分发入口](../rust/src/main.rs#L32-L70) · [现有 Rust 依赖](../rust/Cargo.toml#L19-L48) · [模型安装实现](../rust/src/models.rs#L19-L47)
- **Release 产物。** 独立于现有 Nix CI 增加按架构构建的发布流程。Cargo 应用版本、tag、资产名和安装记录应一致。检查实际携带的第三方库、许可证及再分发声明，模型继续由模型管理器分发。先生成并验证所有声明支持的平台资产，再附到 draft，最后发布为不可变 Release。发布修复时使用新版本和 tag，并在发布说明中记录兼容性。 [现有 CI](../.github/workflows/ci.yml) · [Sherpa-ONNX 许可证](https://github.com/k2-fsa/sherpa-onnx/blob/v1.13.3/LICENSE) · [GitHub 不可变发布流程](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases#best-practices-for-publishing-immutable-releases)
- **Bun 包。** 先确认维护者控制的包名或 scope。npm 平台包复用同一版本的原生构建，包含相应的共享库和许可证。平台包先发并确认可取，再发布主包并更新 `latest`。用隔离 Bun 全局目录验收首装、更新到确切版本、命令路径及真实本地识别。 [npm 发布](https://docs.npmjs.com/cli/v11/commands/npm-publish/) · [npm dist-tag](https://docs.npmjs.com/cli/v11/commands/npm-dist-tag/) · [Bun 全局安装](https://bun.com/docs/pm/cli/add#global)

## 本机验证

- **直接复制现有产物。** `readelf -l` 显示 x86-64 Nix ELF 的 `PT_INTERP` 指向 `/nix/store/8kvxvr3pmsypxiypq4g8zy13glnfr7nx-glibc-2.42-67/lib/ld-linux-x86-64.so.2`。将同一 ELF 单独挂载到无 Nix store 的 `docker.io/library/golang:1.27.4` amd64 容器运行 `--version`，启动失败，报告 `No such file or directory`。
- **补齐运行库后完成识别。** 在临时目录收集同一 ELF 和 90 个运行库文件（含加载器），解包体积 70,355,017 字节。用启动脚本调用随包加载器并指定 `--library-path`，在相同的 Debian 13 amd64 容器中断网运行。`--version` 返回 `voiceless 0.1.0`，`model list --json` 返回七个目录条目。用只读模型挂载、SenseVoice INT8 和临时 `pw-record` 回放运行 `stream --json --no-inject`，退出码为 0，定稿为 `你好世界欢迎使用语音输入`。实验随包带入了原构建的 glibc。正式包仍需在非 Nix 构建环境确定 glibc 基线，并验证发布资产在目标发行版上的实际识别。实测未接入实体麦克风、图形会话或 Fcitx5。
- **Bun 安装与更新。** Bun `1.4.4+38afee6c8` 从本地 HTTP 注册表安装带 `bin.voiceless` 和 SHA-512 完整性字段的测试 npm 包。用独立的全局目录、命令目录和缓存目录执行 `bun add -g voiceless-local-probe`，再执行 `bun update -g --latest voiceless-local-probe`，两次均使用 `--ignore-scripts`。包从 `0.1.0` 升至 `0.2.0`，两次都经全局命令链接把 `--version` 传给同一份原生 ELF，返回 `voiceless 0.1.0`。把更新后的安装目录只读挂载到同一个无 Nix store 的容器，直接调用 Bun 创建的命令链接，也返回相同版本。实测覆盖 npm 包替换、命令入口和原生启动。Voiceless 的 `update` 命令及回滚仍待实现。
- **现有服务。** 桌面命令来自用户 Nix profile，`systemctl --user show voiceless.service` 确认 `ExecStart` 直接指向 `/nix/store/…/bin/voiceless`。测试期间服务保持运行。服务迁移的验证范围限于当前入口和源码，实际切换须在匹配的发行变体准备好后执行。
