# Voiceless 非 Nix 安装与更新调研

## 建议

原生 Linux 构建同时提供 Bun 平台包和 GitHub Release 压缩包。用 `bun add -g voiceless` 安装 Bun 版，也可校验压缩包后用 `voiceless install` 创建 `~/.local/bin/voiceless`。两者都要求 glibc 2.35，都提供 `voiceless update`。x86-64 包提供 Vulkan GGUF，ARM64 包提供 Sherpa-ONNX CPU 推理。 [GitHub Releases](https://docs.github.com/en/repositories/releasing-projects-on-github/about-releases) · [Bun 全局安装](https://bun.com/docs/pm/cli/add#global)

CLI 已实现 `install`、`update` 和独立安装的 `update --rollback`。版本标签触发自动发布，手动运行用于构建和检查候选包。首次公开发布需确认 npm 包名归属及第三方再分发条件。 [CLI 源码](../rust/src/update.rs) · [发布工作流](../.github/workflows/native-release.yml) · [项目 Releases](https://github.com/Vitus213/voiceless/releases)

### Bun 与 Nix 的 Vulkan 交付

Linux x86-64 的 Bun 平台包和 `voiceless-vulkan` Nix 包使用同一 GGUF 后端。默认 `auto` 选择能初始化的 GPU，否则用 CPU。显式 `vulkan` 要求 Vulkan 设备。Sherpa-ONNX 使用 CPU。

Bun 沿用 `bun add -g voiceless` 和已有的平台包。x86-64 原生包用固定摘要的 Jammy Vulkan SDK 构建，保留 glibc 2.35 和通用 x86-64 基线。随包提供 Vulkan loader，由它发现宿主驱动。独立安装沿用 `cpu` 清单标识，已有安装可以直接升级。 [Jammy SDK 归档](https://packages.lunarg.com/vulkan/1.4.313/) · [Vulkan 驱动发现](https://github.com/KhronosGroup/Vulkan-Loader/blob/v1.4.313/docs/LoaderDriverInterface.md#driver-discovery-on-linux)

Bun 启动器在 NixOS 上把 `/run/opengl-driver/share` 加入 loader 的数据目录搜索路径。系统提供的 Vulkan 驱动继续由 loader 加载，Voiceless 原生 ELF 仍需兼容的 Linux 加载器，例如 `nix-ld`。

发布验收检查 ELF 的版本约束和库解析，并通过 Bun 全局命令完成真实转写。GPU 验收使用两个渠道的产物回放同一段音频，再屏蔽驱动验证 CPU 回退。

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
- **桌面和音频。** 录音需要可用的 PipeWire 会话与麦克风。较新的 `pw-record` 可直接采集，Ubuntu 22.04 自带的旧版则使用 GStreamer。该系统需安装 `pipewire-bin`、`gstreamer1.0-tools` 和 `gstreamer1.0-pipewire`。运行 `voiceless doctor` 可查看本机选用的录音工具。声音提示使用 `pw-play`。文字注入依赖 Wayland 与所选方法所需的 `wtype`、`wl-copy` 或 Fcitx5 bridge，并受合成器能力制约。常驻 `daemon` 和 `ctl` 需要用户 D-Bus 会话，Portal GlobalShortcuts 取决于对应后端，配置 evdev 快捷键时还需读取目标输入设备的权限。systemd 用户单元是常驻方式之一。 [项目运行要求](../README.zh.md#适配) · [录音工具诊断](../rust/src/doctor.rs) · [声音提示](../rust/src/media.rs#L1-L10) · [daemon 激活来源](../rust/src/daemon.rs#L1-L14) · [Fcitx5 bridge](../rust/src/fcitx5.rs#L1-L23)
- **文件解码与 X11。** 文件队列内置未压缩 WAV 解码。导入 FLAC、MP3 或视频需要在 `PATH` 中安装 `ffmpeg`。原生 X11 上，`xdotool` 提供活动窗口标识，`x11-utils` 包里的 `xprop` 读取窗口类别。 [媒体解码](../rust/src/transcription_jobs/media.rs) · [目标窗口识别](../rust/src/desktop/focus.rs)
- **识别后端。** 云端识别需要网络、TLS 信任链和服务凭据。本地识别沿用 `voiceless model install` 和现有模型目录。Bun 和 Nix 的 Vulkan 构建使用相同的 `[local]` 配置。使用 CUDA 的配置迁移到 Vulkan 构建时，改用 `provider = "auto"` 或 `"vulkan"`。 [项目模型与 CPU 变体](../README.zh.md#模型目录)
- **Bun 安装入口。** 需要 Bun 来安装和更新全局包，`voiceless` 命令由原生可执行文件提供。Bun 官方 Linux glibc 版本要求 glibc 2.17 或更高，x64 要求 SSE4.2，并另发 musl 可执行文件。Voiceless 的 ABI 以自身 ELF 和共享库的实测为准。 [Bun 安装与运行要求](https://bun.com/docs/installation#musl-binaries)

| 交付变体 | 构建和验收条件 |
| --- | --- |
| 原生 Linux（x86-64、ARM64） | 两种架构使用 CPU 版 Sherpa-ONNX 1.13.3 与 ONNX Runtime。x86-64 还包含通用 CPU 内核和 Vulkan GGUF。发布验收在 Ubuntu 22.04 上核对依赖并通过 Bun 命令完成真实本地转写。 [Sherpa 上游资产](https://github.com/k2-fsa/sherpa-onnx/releases/expanded_assets/v1.13.3) · [发布校验](../scripts/release/validate.py) |
| CPU GGUF（x86-64-v3） | 需要 CMake 和 C++ 工具链构建 transcribe.cpp。当前 AVX2 变体还启用 FMA、F16C 和 BMI2 等指令，安装器须按完整 CPU 基线选包。GGUF 配置使用 `[local] provider = "cpu"`。 [AVX2 构建](../flake.nix#L204-L220) · [后端配置](../rust/src/config.rs#L429-L452) |
| CUDA GGUF（x86_64） | 构建需要 CUDA 12 工具链和 cuBLAS。运行需要 `cudart`、`cublas`、`cublasLt` 及宿主驱动提供的 `libcuda.so.1`。当前内核面向 SM 8.6。建议 GGUF 使用 CUDA，Sherpa/ORT 继续用 CPU 处理 Paraformer、Silero、GTCRN 和 Sherpa 离线路径。这样发布包只需验证 GGUF 实际使用的 CUDA 库，并核对其再分发条款。 [CUDA 构建](../flake.nix#L65-L145) · [链接依赖](../rust/build.rs#L18-L49) · [Sherpa CPU 路径](../rust/src/local_asr.rs#L104) |
| Fcitx5 桥接 | 用 CMake ≥3.16、C++20、ECM 和匹配宿主 Fcitx5 的 Core 与 D-Bus 开发包构建插件，最低 API 为 5.1。按发行版交付 `.so` 和 addon 描述符，重启 Fcitx5 后以有焦点的输入框验收实际提交。 [桥接构建配置](../fcitx5-bridge/CMakeLists.txt) · [桥接验证脚本](../scripts/fcitx5-bridge-test.sh) |

## `voiceless update` 与回滚约定

`voiceless update` 按安装归属查找版本。Linux 原生包按架构沿用原来的发布入口，Nix 和 Home Manager 通过各自的 flake 更新。

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

- **应用实现。** Bun 负责写入其全局目录。独立安装维护自己的版本目录和稳定入口。安装及更新只管理程序和推理库。桌面服务迁移、Fcitx5 和用户数据都由各自的入口管理。 [CLI 分发入口](../rust/src/main.rs) · [更新实现](../rust/src/update.rs)
- **Release 产物。** 在匹配的 Ubuntu 22.04 主机上用 `python3 scripts/release/native.py <输出目录>` 构建，再用 `python3 -B scripts/release/validate.py <输出目录>` 验证。候选包附带推理库和第三方声明。公开发布时应保持 Cargo 版本、tag 和产物版本一致，并在发布说明里列出经实测的系统和架构。 [候选包工作流](../.github/workflows/native-release.yml) · [GitHub 不可变发布流程](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases#best-practices-for-publishing-immutable-releases)
- **Bun 包。** 先确认维护者控制的包名或 scope。npm 平台包复用同一版本的原生构建，包含相应的共享库和许可证。平台包先发并确认可取，再发布主包并更新 `latest`。用隔离 Bun 全局目录验收首装、更新到确切版本、命令路径及真实本地识别。 [npm 发布](https://docs.npmjs.com/cli/v11/commands/npm-publish/) · [npm dist-tag](https://docs.npmjs.com/cli/v11/commands/npm-dist-tag/) · [Bun 全局安装](https://bun.com/docs/pm/cli/add#global)

## 自动发版

首次发版前确认维护者能发布 `voiceless-linux-x86_64-gnu`、`voiceless-linux-aarch64-gnu` 和 `voiceless`。新包首次创建时，可以在 GitHub 仓库 Actions secrets 中临时设置 `NPM_BOOTSTRAP_TOKEN`，使用允许发布相应包且启用 bypass 2FA 的短期 npm granular access token。已有包可以直接绑定 npm Trusted Publisher。为三个包分别填写用户 `Vitus213`、仓库 `voiceless`、文件 `native-release.yml`，勾选允许 `npm publish`。首次建包完成后可删除引导 token，后续由工作流通过 OIDC 发布。 [npm 首次发布](https://docs.npmjs.com/creating-and-publishing-unscoped-public-packages/#direct-publishing) · [npm Trusted Publishing](https://docs.npmjs.com/trusted-publishers/)

1. 将 `rust/Cargo.toml`、`rust/Cargo.lock` 和 `CHANGELOG.md` 的版本改动合入默认分支。手动运行工作流可以提前看两种架构的候选包及额外验证结果。
2. 在对应提交打 `vX.Y.Z` 标签并推送该标签。标签须和 Cargo 版本一致。
3. 工作流验证两种架构后，将独立包及 `SHA256SUMS` 上传到 GitHub 草稿，依次发布两个 npm 平台包和 `voiceless` 主包。npm 核对成功后才公开 GitHub Release。npm 用户此后通过 `bun install -g voiceless` 安装，现有用户通过 `voiceless update` 升级。

如果发布步骤中断，在原工作流运行中重跑失败的 job，沿用该次验收通过的产物。发布脚本会核对已发布 npm 版本和草稿资产的摘要，并继续缺少的步骤。若版本已存在但内容不同，应检查对应的发布记录。需要修复已公开版本内容时，发布一个新版本。

## 本机验证

- **直接复制现有产物。** `readelf -l` 显示 x86-64 Nix ELF 的 `PT_INTERP` 指向 `/nix/store/8kvxvr3pmsypxiypq4g8zy13glnfr7nx-glibc-2.42-67/lib/ld-linux-x86-64.so.2`。将同一 ELF 单独挂载到无 Nix store 的 `docker.io/library/golang:1.27.4` amd64 容器运行 `--version`，启动失败，报告 `No such file or directory`。
- **补齐运行库后完成识别。** 在临时目录收集同一 ELF 和 90 个运行库文件（含加载器），解包体积 70,355,017 字节。用启动脚本调用随包加载器并指定 `--library-path`，在相同的 Debian 13 amd64 容器中断网运行。`--version` 返回 `voiceless 0.1.0`，`model list --json` 返回七个目录条目。用只读模型挂载、SenseVoice INT8 和临时 `pw-record` 回放运行 `stream --json --no-inject`，退出码为 0，定稿为 `你好世界欢迎使用语音输入`。实验随包带入了原构建的 glibc。正式包仍需在非 Nix 构建环境确定 glibc 基线，并验证发布资产在目标发行版上的实际识别。实测未接入实体麦克风、图形会话或 Fcitx5。
- **发布候选包。** 在 amd64 Ubuntu 22.04 容器用 Rust 1.99.1 构建两版真实程序 `0.1.0` 与 `0.1.1`。候选包验证了 ELF 依赖、独立安装、真实 SenseVoice 转写和 Bun 全局命令。在断网容器内拒绝错误摘要，之后独立安装 `0.1.0`，用已校验的本地归档升到 `0.1.1`，再运行 `voiceless update --rollback` 离线回到 `0.1.0`。测试历史文件保持原样。独立的 Bun 1.4.5 全局目录从本地注册表安装 `0.1.0`，由 `voiceless update` 请求精确版本并验证 `0.1.1`。GitHub 尚无正式发布资产，在线获取新版的完整路径仍需在发布后验证。ARM64 候选包由单独的原生 CI runner 构建和验收，本机未运行该架构。
- **Bun 与 Nix 的 Vulkan 回放。** Ubuntu 22.04 x86-64 候选包通过 ELF ABI、独立安装、Bun 1.4.5 全局安装和 SenseVoice 转写。程序从包内解析 `libvulkan.so.1`。Nix Vulkan 构建通过 169 项沙箱测试。NixOS 上，两种安装产物用同一段 4.69 秒音频回放 Qwen3-ASR 1.7B Q8_0，均默认选中 RTX 4060。屏蔽驱动后均自动回退到 CPU，四次最终文本一致，显式 `vulkan` 均退出 1。PCM SHA-256 为 `5f155fce4d2671593880265f63a0e5604f07ed350ea7a10824846b59740cd54c`。
- **现有服务。** 桌面命令来自用户 Nix profile，`systemctl --user show voiceless.service` 确认 `ExecStart` 直接指向 `/nix/store/…/bin/voiceless`。测试期间服务保持运行。服务迁移的验证范围限于当前入口和源码，实际切换须在匹配的发行变体准备好后执行。

### `0.2.0` 私有候选的 Linux 回放（2026-11-25）

Apollo 上的 x86-64 独立包从公开 `0.1.3` 升级后，保留了旧配置、历史和录音。练习模式结束时，测试终端仍为空。在 quick 模式下，同一终端实际收到与定稿逐字节一致的 45 字节 UTF-8 文本，历史重转写也保存了一个版本。该轮使用 ELF `c56e81f2…`，归档 SHA-256 为 `093e55b15656930c176cdca9004165eacc19a6f4c5468eceb4cc33192fc0fead`。

| 设备与模型 | `0.1.3` 停录到定稿 | 候选停录到定稿 | 采样方式 |
| --- | ---: | ---: | --- |
| Apollo RTX 3070，Vulkan，Qwen3-ASR 1.7B Q8_0 | 3.28 秒 | 3.18 秒 | 每版两次，取平均 |
| Apollo Ryzen 5 5600，屏蔽显卡驱动，同一 GGUF | 25.55 秒 | 29.32 秒 | 每版两次，取平均 |
| M1 上的 ARM64 Linux，CPU，SenseVoice INT8 | 0.17 秒 | 0.20 秒 | 每版一次 |

三组都回放同一段 16 kHz PCM。Apollo 候选在 `auto` 下分别报告 `Vulkan0` 和 CPU 回退原因 `no_gpu_device`。M1 使用 Jammy 构建的 ARM64 归档，候选报告 `sherpa-onnx` 和 `cpu`。本地延迟测量使用新启动的 CLI，ARM64 未测上屏。云端 quick 的准确度和延迟单独统计。 [逐次本地测量、设备及包摘要](../scripts/asr_benchmark/reports/linux-local-replay-2026-11-25.json) · [两版云端门禁](../scripts/asr_benchmark/README.md#2026-11-25-云端实时验收)

上述 Apollo 归档曾把 DashScope 流式 WAV 的占位长度判作损坏。修复后的演练归档（SHA-256 `5ed2822f136de3382143c5834a591ac1a51e5dfb7fff951ac8121890fecbb359`）可导入这类文件，实际截断的 WAV 仍报 `corrupt`。 [同一 TTS 编码器产生的文件头样本](https://github.com/ArcReel/ArcReel/issues/3994)

修正后的 Jammy x86-64 独立包（SHA-256 `b0b3a1adee9baf582a8b26844a4c4d3ef62af2e21fc8cb9b6b8064dd9b591982`）在 RTX 4060 上从公开 `0.1.3` 升级，自动选中 Vulkan。练习向目标终端写入 0 字节，正式听写写入「你好，世界！欢迎使用语音输入。」，45 字节与定稿一致。历史保留原音和重转写版本，同一 WAV 在文件队列中完成，生成一个分段。回滚并恢复快照后，`0.1.3` 读出的配置与旧历史均与升级前一致。

### Linux 会议双轨、断线恢复与权限

使用上述 SHA-256 为 `b0b3a1ad…` 的独立包，在两个仅有回环网络的 Ubuntu x86-64 环境中，启动发行版自带的 PipeWire 和 WirePlumber。向所选麦克风的回环播放短句，向所选输出播放 440 Hz 音，再向第三个设备播放另一段录音。两条保存的 WAV 都匹配各自的信号，停止后分别生成了本地转写任务。

| 环境 | 实际录音程序 | 麦克风 WAV 与短句的归一化相关系数 | 系统 WAV 的主频 | 移除麦克风 / 输出后报告中断 |
| --- | --- | ---: | ---: | --- |
| Ubuntu 22.04，PipeWire 0.3.48 | GStreamer `pipewiresrc` | 0.999 | 440 Hz | 1.06 秒 / 0.98 秒 |
| Ubuntu 24.04，PipeWire 1.0.5 | `pw-record` | 0.989 | 440 Hz | 0.11 秒 / 0.16 秒 |

移除设备后，两套环境都报告了消失的来源名称。选择恢复后，两轨已写入的语音和测试音仍可播放。开始前已拔掉的来源返回 HTTP 409，会议列表为空。

在 Ubuntu 22.04 上，单独用 PipeWire 的访问规则拒绝 `gst-launch-1.0`，同时保留控制台枚举麦克风的权限。服务端记录了被拒进程的 PID。会议报告 `permission-denied` 及所选来源，收到 0 字节，退出了录音进程，并提供授权、改选来源或丢弃的操作。默认麦克风和指定麦克风的 CLI 听写也都报告了权限错误和恢复方法。两套环境另测了整个音频套接字不可访问的情况，API 均返回 HTTP 503，恢复访问后可重新列出来源。

公开的 ARM64 `.deb` 在 Ubuntu 22.04 客户机中也走通了五个会议场景。麦克风短句的相关系数为 0.989，系统轨主频为 440 Hz，两轨各生成一个转写任务。移除麦克风或输出后，分别在 0.60 秒和 0.48 秒内报告中断，恢复后两条 WAV 均保有各自的信号。已移除的来源在开始前返回 409。让 PipeWire 单独拒绝录音进程时，会议标记麦克风 `permission-denied`，提交 0 字节，录音子进程全部退出。 [受测 ARM64 安装包](https://github.com/Vitus213/voiceless/actions/runs/45668781010/artifacts/15050695493)

### Apollo 实体麦克风试说

早期候选在 Apollo 上测了两支实体麦克风。Lian II 恢复可用后，练习窗口仍有焦点，扬声器播放短句与麦克风采样的相关系数为 0.018。切到 DJI Wireless Mic Rx 后，RTX 3070 完成 Vulkan 识别，隔离终端在练习期间保持 0 字节。

使用 Bun 从公开 npm 安装 `voiceless@0.2.0` 后，首次试说在指定失效麦克风时报告错误。改选 DJI 后，Qwen3-ASR 在 RTX 3070 上通过 Vulkan 生成了 24 个字符的定稿。定稿未匹配约定短句，预览时测试终端仍是 0 字节。用户随后确认已亲自试用，接受此次首次试说验收。显式上屏按用户反馈记录，本轮没有真人定稿与目标窗口逐字比对的自动回执。

### `0.2.0` 桌面候选包验收

公开的 `v0.1.3` 提供 Linux 包。以下 `0.2.0` 产物用于候选验收，版本号只在打包用的源码副本中调整。

首次试说提示修正后的提交 `71e202b` 在 Linux 上通过 `just check`，Rust 测试有 404 项通过，浏览器端到端测试 31 项全部通过。另在 Apollo 上用同一次构建的测试程序和已安装模型运行两项默认跳过的测试。流式 Paraformer 正常收尾，Qwen3-ASR 0.6B INT8 在 CPU 上识别出的绕口令包含「广西壮族自治区」「出租车司机」和「南宁市」。

设备选择修复后的提交 `57354a1` 在 Linux 通过 `just check`，37 项浏览器端到端测试全部通过。浏览器用不同构建的模型目录验证旧设备配置能否保留，并覆盖从下拉菜单和模型库切换模型的两条路径。macOS 和 Windows 安装版的试说结果另有记录，包含产物摘要和编辑器实际收到的文本。 [安装版试说记录](../scripts/asr_benchmark/reports/native-desktop-delivery-2026-11-25.json)

| 平台 | 安装 CLI | 安装桌面应用 |
| --- | --- | --- |
| Windows x64 | 运行 `core` 安装包，新开终端即可通过用户 `PATH` 运行 `voiceless`。无需另装 Visual C++ Redistributable。 | 安装 Microsoft Edge WebView2 Evergreen Runtime，再运行 `desktop` 安装包。从开始菜单打开 Voiceless。 |
| Apple Silicon | 从 CLI DMG 运行 `install-cli.command`，命令入口为 `~/.local/bin/voiceless`。 | 将桌面 DMG 中的 `Voiceless.app` 拖入「应用程序」。随包的 ONNX Runtime 要求 macOS 15.5 或更新版本。 |

这两种平台的候选包均已使用随包的 Sherpa-ONNX 运行库在 CPU 上完成离线转写。 [Windows 安装定义](../desktop/packaging/windows.iss) · [macOS 安装脚本](../scripts/release/macos/install-cli.command)

Linux x86-64 的独立包、npm 平台包和桌面 `.deb` 携带同一份 CLI，ELF SHA-256 为 `958e9360e09c68c99853272f0a6eb2c9b1837e8881ac6ce91c6295065b177055`。Ubuntu 22.04.5 构建机运行 `native.py` 和 `validate.py`，独立安装与 Bun 安装均完成 SenseVoice 离线转写，内嵌控制台可访问。 [构建脚本](../scripts/release/native.py) · [产物验收脚本](../scripts/release/validate.py)

在只读的官方 Ubuntu 22.04 根文件系统中，核心包在断网且没有 Node、Bun、GTK 或 WebKit 的条件下，将测试 WAV 转成「你好我是小助手」。`voiceless web --no-open` 提供的 HTML、JS 和 CSS 均返回 HTTP 200。`/api/totals` 带控制台令牌时返回 200，省略令牌时返回 403。

桌面 `.deb`（SHA-256 `c29e7b33a79df492fcf3d3ddee6f50481df1b322ccb2d290045137fb54851769`）经 `apt` 安装后，在 Ubuntu 22.04.5 的 Sway 和 24.04.5 的 Niri 上均完成试说。两套桌面回放「你好我是小助手」时，练习结束后输入框为空，点击“测试上屏”才得到完整短句。由托盘重开窗口时，宿主和守护进程的 PID 均保持一致。

在 Linux x86-64 的 `0.2.0` 独立包中，首次试说已选中测试麦克风和 SenseVoice。将测试进程的单文件大小限制为 128 KiB 后，Silero VAD 下载写入失败，未激活部分文件。解除限制后，在同一页面重试只请求了 Silero VAD。下载的 643854 字节通过目录中固定的 SHA-256 校验，原来的麦克风、识别方式和模型选择都保留了，点击「应用设置」后显示「识别就绪」。后续修正让错误指出模型名称，并在原处显示「重试下载」。 [首次试说浏览器用例](../frontend/e2e/onboarding.spec.ts)

Sway 另测了两个独立窗口。在窗口 A 开始听写、停录前切到窗口 B 时，两边的输入框均未被写入，定稿保留在控制台。Sway 1.7 的无头测试会话保留了一个虚拟键盘，移除最后一个键盘时，单独运行 `wtype` 也出现过漏掉首字的情况。

公开候选工作流也在 Ubuntu 22.04 的 x86-64 和 ARM64 原生 runner 上分别运行了 `native.py` 和 `validate.py`。两种架构的独立包及 Bun 安装均用 SenseVoice 完成离线转写，桌面包各自打开了一个连接本机守护进程的窗口。该次运行共生成两个独立包、三个 npm 包和两个桌面包。 [原生构建与验收](https://github.com/Vitus213/voiceless/actions/runs/45668781010) · [同一次运行的七份产物](https://github.com/Vitus213/voiceless/actions/runs/45668781010/artifacts/15050695493)

从这批产物下载的 x86-64 `.deb`（SHA-256 `49c24799498c233f800f4adc9e5f8061e5270e902083d5323267beb50c76efa5`）在 Sway 和 Niri 上都通过了练习、显式上屏、托盘重开和运行库检查。ARM64 独立包（`b54ca7cc88428c39cc2f7408e448235765b0ae6572ce20e0eb4a8ca645c78e0c`）另在 Apple M1 上的 ARM64 Linux 客户机中离线运行。SenseVoice 从回放音频识别出英文测试句。未安装 Node、Bun、GTK 或 WebKit 的环境可访问内嵌页面、脚本和样式，历史统计接口带令牌返回 200，省略令牌返回 403。

同批 ARM64 `.deb`（SHA-256 `830bc32355d145b1ed2f047c5d9b71768c0ef35910aa834c1d5155ba63cd2ccc`）经 `apt` 安装到 QEMU 的 Ubuntu 22.04.5 客户机，在 Sway 1.7 上用 PipeWire 回放「你好我是小助手」。练习时两个 GTK 输入框均为空，显式上屏和随后两次听写均只写入选定窗口。验收等 GTK 消费全部按键后再比较文字。托盘重开了原窗口，宿主和守护进程的 PID 保持一致，两个安装后的可执行文件均通过运行库检查。

公开的 x86-64 独立包（SHA-256 `1015c2648484828c838f4d41df33e2253156c71bde6bd1e7a67bfaab7cc8e01d`）在断网的原版 Jammy 根文件系统中完成了 CPU 转写与 Web 验收。另在 RTX 4060 的隔离桌面从 `0.1.3` 升级，`auto` 选中 Vulkan。练习留下 0 字节，正式听写与目标终端中的 45 字节逐字相同。历史重转写和文件队列均完成，恢复快照后旧版重新读出了原配置和历史。 [包摘要、终端收到的字节、历史版本和恢复结果](../scripts/asr_benchmark/reports/linux-upgrade-replay-2026-11-25.json)

在个人 Apple M1 的 macOS 26.5 上，从 CLI DMG 安装到独立用户目录，并从桌面 DMG 复制出 `Voiceless.app`。两个安装都完成了本地文件转写。再次启动桌面入口时，已运行的宿主显示原窗口。包内二进制通过 ad-hoc 签名校验。随包 ONNX Runtime 要求 macOS 15.5，实际运行的系统为 26.5。锁屏会话能枚举三个麦克风，未获得麦克风、输入监控和辅助功能授权。菜单栏、实体试说、向前台应用上屏和 Developer ID 签名公证在这台机器上仍待验收，其中一部分后来在托管客机上观察到，见下文。 [macOS 打包入口](../scripts/release/macos/package.py)

这轮 M1 验收的 CLI DMG SHA-256 为 `3f5463c4a126225aef56fc4aedfe609ad80aacabf631de1917496ca55f8be400`，桌面 DMG 为 `f3e55faa0e9497f18bf89de78b11954c138e3b37c4bf00cb19b53bccf31709de`。安装后的 CLI 守护进程用 SenseVoice INT8 将 3.884 秒的流式 WAV 转为「你好世界欢迎使用语音输入」，并成功导出 TXT 与 JSON。这个 WAV 的文件头使用占位长度，保留前半段完整采样仍是有效的 1.941 秒音频。另一个文件的头部已损坏，导入失败后再次提交原件仍然成功。

历史并发修复后的 M1 CLI DMG（SHA-256 `3f7fec0e72ea3e2529c671b73286d191aeb9baf8cf806f8482eab0313b0fbb92`）和桌面 DMG（`fbd2a8eabc63d1a98e56194863b5e0034c64294f08213f23881f19fdd8fb567b`）均在安装后复测。对每份包，8 个进程同时打开全新历史库的 60 轮、32 个进程的 10 轮全部成功。写锁持有 0.8 秒时，读取等待释放后成功。持锁 7.5 秒时，读取在约 5 秒后退出，释放锁后历史仍可读。历史库损坏时控制台退出，未打印可访问地址。两包使用 ad-hoc 签名，Gatekeeper 返回退出码 3。

GitHub 托管的 Apple Silicon 客机运行 macOS 15.8.2。镜像中的 SIP 已关闭，登录会话已解锁，系统把作业的权限记在 `hosted-compute-agent` 名下。重置麦克风决定后，系统弹窗上的「不允许」使首次录音返回 409，会议列表为空。点「允许」后可录音。这里测到的是运行器身份下的系统授权流程。麦克风与系统输出使用测试安装的 BlackHole 回环端点，语音来自预录 WAV。 [环境记录](https://github.com/Vitus213/voiceless/actions/runs/45679945151) · [授权及双轨逐项结果](https://github.com/Vitus213/voiceless/actions/runs/45688022647/artifacts/15059355376)

两次早期完整运行均有失败项。第一次在系统音频授权后重试，安装版将麦克风的 330 Hz 和系统输出的 880 Hz 分别写到两条 WAV，并各生成一个转写任务。第二次验证了菜单栏状态项、登录时隐藏窗口和重开窗口后保持同一 `runtime_id`。两轮都观察到安装版拒绝向受保护的输入框上屏。 [第一轮](https://github.com/Vitus213/voiceless/actions/runs/45688022647) · [第二轮](https://github.com/Vitus213/voiceless/actions/runs/45691666337)

修正焦点查询后，安装版 CLI 向前台 TextEdit 文稿交付 `Voiceless 中文 Ω`，保存后读回的 19 个 UTF-8 字节完全一致。同批桌面 DMG 在自己的窗口内用 SenseVoice INT8 和虚拟麦克风完成试说。练习结束时 TextEdit 文稿为 0 字节，点击「测试上屏」后保存的 101 字节与预览完全一致。参考英文句中的两处 `ask` 有识别错误。应用报告 `unconfirmed`，文本比对来自 TextEdit 保存的文件。 [CLI 上屏](https://github.com/Vitus213/voiceless/actions/runs/45694716105) · [桌面首次试说](https://github.com/Vitus213/voiceless/actions/runs/45699297883)

使用已授权虚拟麦克风录制 330 Hz 测试音时，终止守护进程再重启，会议显示可恢复或丢弃。显式恢复后，262144 字节已提交 PCM 全部保留并进入转写队列。移除所选虚拟麦克风也会中断采集，恢复得到 270336 字节，缺少该设备时再次开始返回 409。 [安装版恢复记录](https://github.com/Vitus213/voiceless/actions/runs/45695626355)

安装带有 `106b45a` 修复的 CLI 后，保持系统音频授权弹窗无人应答。首次双轨请求和重试都在 30.2 秒后返回 HTTP 409，指出所选系统输出仍需授权。两次请求期间，会议目录均无变化，屏幕上只有一个授权弹窗。两次请求之间，已获授权的虚拟麦克风完成了单轨录音并生成转写任务。 [安装版运行记录](https://github.com/Vitus213/voiceless/actions/runs/45707155253) · [验收证据](https://github.com/Vitus213/voiceless/actions/runs/45707155253/artifacts/15066347603)

在另一台托管 Mac 上复用同一份 CLI DMG（SHA-256 `e9e8019d1707a155d0537f29ef39f9bad0375bbfd8a0e1b8171a82fce90fe157`）。测试通过 System Events 点击系统弹窗的「允许」，原请求随即开始录音。两条 WAV 各含 133803 个采样，分别录到授权后的 330 Hz 麦克风和 880 Hz 系统音。移除测试输出驱动并重启 Core Audio 时，两台虚拟设备同时离线，会议记录麦克风中断。显式恢复后两个文件分别保有 157696 和 157867 个采样。重新接入后双轨录音、参考语音转写均成功。 [10 项安装版检查及录音结果](https://github.com/Vitus213/voiceless/actions/runs/45716162420/artifacts/15069413747)

使用运行 `45694716105` 生成的桌面 DMG，向 TextEdit 文稿 A 粘贴 `Voiceless 中文 Ω` 后，保存的 19 个 UTF-8 字节与预期一致，剪贴板恢复了原来的测试文本。另一轮在 A 开始听写，切到文稿 B 再停录，应用报告 `not-delivered` 并保留定稿。保存并关闭两份文稿后，文件与听写前逐字节相同，剪贴板也保留了测试文本。 [桌面安装版实测](https://github.com/Vitus213/voiceless/actions/runs/45710341438) · [文稿摘要与交付状态](https://github.com/Vitus213/voiceless/actions/runs/45710341438/artifacts/15067591385)

在托管 Apple Silicon 上从 CLI DMG 安装后，用浏览器打开内嵌控制台。选择「跟随系统默认」时，页面随 CoreAudio 默认输入切换。显式选定测试聚合麦克风后，系统默认再次改变时，Voiceless 继续使用所选设备。单独移除该设备后，设置页保留原选择，标出「未连接」，有效输入为空。选择「跟随系统默认」并应用后，原来的 `AVIODevice 2` 恢复生效。设备切换期间，Voiceless 和 `coreaudiod` 的进程均持续运行。 [安装版实测](https://github.com/Vitus213/voiceless/actions/runs/45751209895) · [失效与恢复截图](https://github.com/Vitus213/voiceless/actions/runs/45751209895/artifacts/15085263675)

在托管 Apple Silicon 上，较早的 `0.2.0` 候选包在所选系统输出消失后仍显示录制中。安装修复后的 CLI DMG（SHA-256 `3f34e008bcde6c73cc87a28c9e11963e9a3b140a0eeda3b938b88795e72c7834`）后，单独移除选中的 Core Audio 聚合输出，会议进入 `interrupted`，失败来源为 `system`。显式恢复后，麦克风 WAV 有 151211 个采样，检出 330 Hz 测试音。系统 WAV 有 148992 个采样，检出 880 Hz 测试音。两份文件均进入本地转写队列。输出缺失时再次开始返回 HTTP 409，麦克风仍能单独录音。接回同一输出后双轨录音恢复，守护进程的 `runtime_id` 和 `coreaudiod` 的 PID 均保持一致。 [旧包复现](https://github.com/Vitus213/voiceless/actions/runs/45752540316) · [修复包验收记录](https://github.com/Vitus213/voiceless/actions/runs/45755892334/artifacts/15088240562)

`v0.2.0` 对应的提交 `e1dafb27` 生成了两份 Apple Silicon 候选 DMG。在个人 M1（macOS 26.5）上，用 CLI 安装脚本和桌面应用分别安装到隔离目录，卸载磁盘映像后，两份安装均通过 `codesign --verify --strict --deep`。两者都报告 `voiceless 0.2.0`，用本地 SenseVoice 转写同一段公开录音，得到的 83 个字符与 Linux 发布版逐字一致。签名类型均为 ad-hoc。[受测 DMG](https://github.com/Vitus213/voiceless/actions/runs/45800558811/artifacts/15106894409)

两台 Mac 使用的 Voiceless 产物均为 ad-hoc 签名。Developer ID 签名与公证、实体麦克风试说、实体按键触发和实体输出设备拔插仍待实测。 [签名与公证入口](../scripts/release/macos/package.py)

Windows x64 的两份 Inno Setup 安装包在 Linux 上用 MinGW 构建，并在 Wine 11.0 中安装和卸载。安装后的 CLI 校验了带子目录的 SenseVoice 模型，离线队列把上面的流式 WAV 转写为「你好世界欢迎使用语音输入」。双轨分别采到 330 Hz 麦克风和 880 Hz 系统音频，拒绝麦克风时返回 409。核心安装包 SHA-256 为 `6111e7453edd7cc530fde145d4a076a84387483664e05dc42bbf9b62b2a2be40`，桌面安装包为 `14d6fa46375137418b029975e5b82e89251a986ffa9ae0bd108fce074997bb29`。

Windows Server 2022 上使用 MSVC 构建并安装了 CLI 和桌面候选包。两个包各自的 `voiceless.exe` 都用 SenseVoice 在 CPU 上转写了测试音频。桌面入口打开 WebView2 窗口，关闭后再次运行启动器能显示原窗口，守护进程的 `runtime_id` 保持一致。26 项原生测试通过，包括系统快捷键冲突、失效 WASAPI 来源，以及在两个编辑窗口间切换焦点时，只向原目标写入 Unicode。 [原生运行日志](https://github.com/Vitus213/voiceless/actions/runs/45667788670) · [两份 MSVC 安装包](https://github.com/Vitus213/voiceless/actions/runs/45667788670/artifacts/15049053581)

随后在 Windows Server 2022 上，以安装后的 `voiceless.exe transcribe --inject` 转写音频。程序通过剪贴板交付，原生编辑框收到的 83 个字符与 CLI 返回的完整定稿逐字一致。 [安装到上屏的运行记录](https://github.com/Vitus213/voiceless/actions/runs/45669746118) · [对应安装包](https://github.com/Vitus213/voiceless/actions/runs/45669746118/artifacts/15049464041)

关闭桌面窗口后，分别在任务栏通知区域和折叠图标区右击 Voiceless，按系统辅助功能接口给出的名称点击「打开窗口」。两次都重新显示原窗口，窗口句柄和守护进程的 `runtime_id` 均与关闭前一致。 [任务栏图标](https://github.com/Vitus213/voiceless/actions/runs/45685394439) · [折叠图标](https://github.com/Vitus213/voiceless/actions/runs/45685694184)

修正显式配置后的同一批安装包也通过了托盘重开。该版本的 Windows MSVC 历史库测试全部 22 项通过，覆盖旧库迁移、跨年检索及首次打开遇到写锁后等待并切换到 WAL。 [安装版重开和原生历史库测试](https://github.com/Vitus213/voiceless/actions/runs/45686906608)

从开始菜单首次打开 Windows 桌面版，在试说页选择 WASAPI 虚拟麦克风，下载模型并启用本地 CPU 识别。Windows 确认 ScrollLock 已由 Voiceless 注册。软件发送的按键被忽略，超时后页面提示检查绑定或点击「开始试说」。快捷键页可展开 CLI 命令，并说明录音状态显示在 Voiceless 窗口中。实体键盘触发仍待验证。 [受测安装包](https://github.com/Vitus213/voiceless/actions/runs/45688407733/artifacts/15058087523) · [安装后操作记录](https://github.com/Vitus213/voiceless/actions/runs/45690514706)

通过页面按钮试说时，前台编辑框保持原样。修改预览并点击「测试上屏」后，页面显示剪贴板交付有待确认，测试从原生编辑框逐字读回了整份修改稿。另一次听写在停录前切换到第二个窗口，两个输入框均保持原样，控制台保留定稿。 [截图、录音和编辑框结果](https://github.com/Vitus213/voiceless/actions/runs/45690514706/artifacts/15058916490)

在同版 Windows Server 上安装 CLI 后，用两对虚拟音频端点分别输入 330 Hz 和 880 Hz。Voiceless 将两种频率分别录到麦克风和系统回环的 WAV，各生成一个转写任务。通过 Windows 声音控制接口禁用所选麦克风后，会议报告该来源失效；选择恢复后，两条 WAV 都保有原来的信号。重新启用端点后，`voiceless record` 从指定的非默认麦克风识别出参考英文句中的连续词组。关闭桌面应用麦克风权限返回 HTTP 409，会议数保持不变。 [安装后双轨、断开恢复与听写](https://github.com/Vitus213/voiceless/actions/runs/45682688445)

另一个 MSVC 候选测了 WASAPI 本身拒绝打开设备的情况。即使注册表预检查放行，安装包也将 `0x80070005` 报为所选麦克风的权限错误，返回 HTTP 409，会议列表为空。 [权限修复运行记录](https://github.com/Vitus213/voiceless/actions/runs/45681535946) · [对应安装包](https://github.com/Vitus213/voiceless/actions/runs/45681535946/artifacts/15053794610)

含 `--config` 修复的 MSVC 安装版也在同一环境核对了输入选择。测试放入一份含非法后端的默认配置，另用 `--config` 指定有效配置和非默认麦克风。`transcribe` 识别了参考 WAV，`record` 从重连后的所选输入识别出同一句中的连续词组。 [安装后实测](https://github.com/Vitus213/voiceless/actions/runs/45683768636) · [两份安装包](https://github.com/Vitus213/voiceless/actions/runs/45683768636/artifacts/15055867249)

同一份 Windows 安装包还测了会议录制中途终止守护进程。退出前，麦克风和系统轨各确认写入 262144 字节。重启后会议显示可恢复或丢弃，此时两轨均未进入转写队列。选择恢复后，两份 WAV 各包含 131072 个采样，分别保留 330 Hz 与 880 Hz 的信号，并各生成一个转写任务。 [安装版重启恢复记录](https://github.com/Vitus213/voiceless/actions/runs/45685670551)

会议授权修复后的 Windows 安装包也通过了双轨、权限与中断恢复验收。测试按 WASAPI 端点 ID 选择设备，麦克风与系统输出分别录得 330 Hz、880 Hz，拒绝麦克风时返回 409 且会议数保持不变。进程中断前两轨各提交了 262144 字节，重启后等待显式恢复，恢复的两份 WAV 各有 131072 个采样并各生成一个转写任务。 [运行记录](https://github.com/Vitus213/voiceless/actions/runs/45700798152) · [同批安装包](https://github.com/Vitus213/voiceless/actions/runs/45698558308/artifacts/15061804292)

包含 `106b45a` 的 Windows 核心安装包 SHA-256 为 `d4e06be3c6620ffe284d2f4021e281c1c36982a25ebe3aed3d47adef903fbd14`。客机默认设置下，WASAPI 首次打开麦克风返回 `0x80070005`，随后 Windows 保留了拒绝决定。两次双轨请求均返回 HTTP 409，并指出所选麦克风，会议列表和目录始终为空。 [原生安装包](https://github.com/Vitus213/voiceless/actions/runs/45707934020) · [同包权限拒绝与重试](https://github.com/Vitus213/voiceless/actions/runs/45710444582)

同一份安装包在 Windows Server 2022 客机上通过设置页开启麦克风访问后，麦克风轨和系统轨各录得 132640 个采样，分别为 330 Hz 和 880 Hz。禁用所选端点后，两轨均可恢复。另一次终止守护进程后，重启并显式恢复得到两份各含 131072 个采样的 WAV，各生成一个转写任务。关闭设置页开关后，录音再次返回 409。 [原生会议作业](https://github.com/Vitus213/voiceless/actions/runs/45723972884/job/139588997392) · [系统开关与录音证据](https://github.com/Vitus213/voiceless/actions/runs/45723972884/artifacts/15073726861)

加入 `527ca67` 的核心包首次收到 WASAPI `0x80070005` 时，同时显示设备 ID 和 Windows 麦克风隐私设置的位置。Windows 记住拒绝决定后，重试显示同一设置指引一次，两次均未创建会议或音轨。 [安装版验收](https://github.com/Vitus213/voiceless/actions/runs/45712544305) · [受测安装包](https://github.com/Vitus213/voiceless/actions/runs/45712544305/artifacts/15069615629)

在 Wine 11.0 中安装同一份 MSVC 核心包，用独立的 PulseAudio 服务提供合成信号。正常停止后，麦克风 WAV 保存了 133760 个采样，主频为 330 Hz。系统 WAV 保存了 133280 个采样，主频为 880 Hz。录制另一场会议时终止该 Wine 实例，重启后会议显示可恢复或丢弃。显式恢复后，每轨保留 131072 个采样和各自的频率，并各自生成转写任务。 [受测 MSVC 安装包](https://github.com/Vitus213/voiceless/actions/runs/45707934020/artifacts/15067410293)

Windows Server 2022 安装了 `v0.2.0` 发布提交 `e1dafb27` 构建的两份 MSVC 候选包。两份安装均完成 CPU 离线转写。CLI 将 83 个字符完整写入原生编辑框，桌面版从任务栏通知区域的「打开窗口」重开了原窗口。两个原生编辑窗口间切换焦点的 Unicode 交付测试也通过了。核心安装包 SHA-256 为 `5f5aea09ec3379f54348007c057a21518e253137cc157489d0e80cf9b4302a77`，桌面安装包为 `ea9398b98d8864cf65a90993819e68668e67c2f141c94f3c5833ca58a46e0155`。该作业后续的会议步骤未能读到测试设备 ID。[安装与交付日志](https://github.com/Vitus213/voiceless/actions/runs/45800558357/job/139842368510) · [受测安装包](https://github.com/Vitus213/voiceless/actions/runs/45800558357/artifacts/15108740410)

将 WASAPI 测试设备安装移到前一个 Actions 步骤后，`7cc0ece4` 的 Windows 验收全部通过。两条虚拟线缆分别输入 330 Hz 和 880 Hz，麦克风和系统轨各录得 132640 个采样。禁用麦克风后，显式恢复保留了两轨各自的频率。系统拒绝麦克风访问时返回 HTTP 409，会议数保持不变。另一次终止守护进程再恢复，每轨保留 131072 个采样，分别生成转写任务。[原生验收日志](https://github.com/Vitus213/voiceless/actions/runs/45805826895) · [本次受测安装包](https://github.com/Vitus213/voiceless/actions/runs/45805826895/artifacts/15110627351)

#### Windows x64 安装包的范围与实测边界

Windows 安装包只由候选工作流在 GitHub 托管的 `windows-2022` 客机上构建，手动触发或推送 `ci/windows-candidate/**` 分支时运行，结果上传为 Actions 附件。标签触发的发布工作流只构建和发布 Linux，其中没有 Windows 步骤。构建使用 Rust 1.99.1 的 `x86_64-pc-windows-msvc` 目标和默认特性，不带 `transcribe`、`cuda` 或 `vulkan`，再由 Inno Setup 6 生成按用户安装的 `core` 与 `desktop` 两个安装包，两者都没有代码签名。 [候选工作流](../.github/workflows/windows-candidate.yml) · [发布工作流](../.github/workflows/native-release.yml) · [打包脚本](../scripts/release/windows/package.py) · [安装定义](../desktop/packaging/windows.iss)

`core` 的载荷是 `voiceless.exe`、`onnxruntime.dll`、`onnxruntime_providers_shared.dll`、`sherpa-onnx-c-api.dll`、`sherpa-onnx-cxx-api.dll` 和许可证，`desktop` 另带 `voiceless-desktop.exe`，并要求系统已安装 WebView2 Evergreen Runtime。包内没有 GGUF 引擎，也没有 CUDA、Vulkan 或 DirectML 提供程序，可用的计算设备只有 CPU。`voiceless doctor` 报告 `Windows inference engines: x86_64 build; compiled providers: cpu; Sherpa-ONNX uses CPU`。模型接口给 Sherpa-ONNX 模型只列出 `cpu`，给 GGUF 模型不列设备，首次试说页和模型库因此不提供 GGUF。安装版的引擎状态为 `sense-voice-int8 on cpu`，请求设备与实际设备都是 `cpu`。这个包没有任何受支持的 GPU，Windows 上也没有构建或实测过带 GPU 的识别路径。 [载荷所在的构建日志](https://github.com/Vitus213/voiceless/actions/runs/45688407733) · [doctor 断言](../rust/tests/desktop_contract/windows.rs) · [模型设备列表](../rust/src/models/info.rs) · [安装版引擎状态](https://github.com/Vitus213/voiceless/actions/runs/45690514706)

设置页按所选模型列出可用设备。Windows 安装版打开一份已有 `provider = "vulkan"` 的配置时，保留原值并显示该设备不可用。切到 SenseVoice 后只显示「默认」和「CPU」。保存「默认」后配置中没有 `provider` 字段。改选 CPU 并保存后，重新加载页面仍选中 CPU。 [安装版设备切换](https://github.com/Vitus213/voiceless/actions/runs/45701405588) · [截图与配置读回结果](https://github.com/Vitus213/voiceless/actions/runs/45701405588/artifacts/15064256869)

从开始菜单打开 Windows 安装版后，WebView2 的上屏设置只提供「剪贴板」和「复制到剪贴板」。打开页面保留了旧的 `fcitx5` 配置，搜索 `wtype` 没有匹配项。分别选择并保存后，配置写入 `copy` 和 `clipboard`，重新打开后选择正确。 [安装版实测](https://github.com/Vitus213/voiceless/actions/runs/45699415809) · [截图及读回结果](https://github.com/Vitus213/voiceless/actions/runs/45699415809/artifacts/15062391215)

受测源码依次是候选分支上的 `c662d87`、`3142433`、`c419f09`、`f000f26` 和 `2c537a6`，运行日期为 2026-11-25（UTC）。运行 `45667788670` 产出的两份安装包在托盘和双轨验收中复用，下载后的 SHA-256 为核心 `71e19137ad5a7561b9e6daab7695582f884783db0852d61f2e6f3db9a69e39af`、桌面 `139f50e1a18738de573b76fe8f9fcea2ea5b084b71cf89b70a7051a1dca4d1a9`。 [首批 MSVC 安装包](https://github.com/Vitus213/voiceless/actions/runs/45667788670/artifacts/15049053581)

Windows 原生实测都在 GitHub 托管的 Windows Server 2022 Datacenter（10.0.20348）客机上完成。登录会话里有 `explorer.exe`，开始时没有音频端点，显卡只有 `Microsoft Hyper-V Video`，会话以高完整性（`0x3000`）运行。麦克风和系统回环来自厂商的 Virtual Audio Cable 4.71 评估驱动，两条虚拟线缆各有播放和录制端点。驱动只用于测试。测试通过声音控制面板使用的 Core Audio 端点可见性接口模拟麦克风断开。早期授权用例在客机的 `ConsentStore` 中设置允许和拒绝，后续补测了系统按原有设置拒绝录音的情况。 [客机环境](https://github.com/Vitus213/voiceless/actions/runs/45690514706) · [端点搭建](../scripts/release/windows/lines.ps1) · [端点开关](../scripts/release/windows/endpoint.ps1) · [完整性级别](https://github.com/Vitus213/voiceless/actions/runs/45682965073)

首次试说的页面由测试经 WebView2 的远程调试端口操作，安装后的桌面程序和守护进程没有改动。语音是模型包附带的参考英文句，由测试播放进虚拟线缆。「测试上屏」后页面显示 `Delivery unconfirmed. Check the target field`，经控制台 API 开始的一次听写把定稿追加到有焦点的编辑框，交付状态同为 `unconfirmed`。托盘菜单经 MSAA 读出「打开窗口」「暂停听写」「重贴上次结果」「登录时启动」和「退出」，实测只调用了「打开窗口」。代码签名和 SmartScreen 提示仍待实测。 [操作记录](https://github.com/Vitus213/voiceless/actions/runs/45690514706) · [试说脚本](https://github.com/Vitus213/voiceless/blob/e64d97c0c16b0eb6901c22cdf25f1b890af70049/scripts/release/windows/practice.py) · [托盘菜单](https://github.com/Vitus213/voiceless/actions/runs/45685694184) · [安装验收脚本](../scripts/release/windows/accept.py)

在 Windows Server 2022 上，安装 CLI 后通过 Explorer 打开新的 PowerShell。在安装目录之外，`where.exe voiceless` 和 `Get-Command voiceless` 都只找到安装的程序，按名称运行 `voiceless --version` 输出 `voiceless 0.2.0`。卸载结束后，新终端找不到该命令。这轮用户 `PATH` 原值不以分号结尾，卸载后值和类型恢复，原有目录中的命令仍能运行。安装目录和工作目录均含空格，16 项检查全部通过。 [安装与卸载记录](https://github.com/Vitus213/voiceless/actions/runs/45708657354)

新建标准账户的 `PATH` 以 `%USERPROFILE%\AppData\Local\Microsoft\WindowsApps;` 结尾。早期 CLI 安装包卸载后丢失了最后的分号。用 `90fde25` 修复重新构建后，安装期间保留了末尾分号，新终端可以按名称运行 CLI。卸载后，`PATH` 与安装前逐字相同，类型仍为 `REG_EXPAND_SZ`，新终端已找不到 CLI。 [原始失败](https://github.com/Vitus213/voiceless/actions/runs/45723972884/job/139588997136) · [修复后的实测](https://github.com/Vitus213/voiceless/actions/runs/45745562089/job/139660622258) · [两份受测安装包](https://github.com/Vitus213/voiceless/actions/runs/45743904617/artifacts/15083406038)

在没有 WebView2 的 Windows Server Core 2022 容器中，桌面安装器以退出码 7 拒绝安装，日志提示先安装 Microsoft Edge WebView2 Evergreen Runtime。目标目录没有留下可执行文件，当前账户也没有新增卸载项。 [安装记录](https://github.com/Vitus213/voiceless/actions/runs/45746709811)

同一环境的旧核心包安装成功，但运行 `voiceless --version` 时返回 `0xC0000135`。只补入 `VCRUNTIME140.dll` 后，原程序就能启动并列出 7 个模型。`1a0c697` 将 MSVC 运行库静态链接到发布程序。重新构建的核心包在断网容器中报告 `voiceless 0.2.0`，列出 7 个模型，`voiceless web` 返回 HTTP 200，随后卸载成功。在带 WebView2 的 Windows 桌面，两份安装包都完成了 SenseVoice 离线转写，CLI 向编辑器送达 83 个字符，桌面窗口从托盘重开。 [单个 DLL 对照实验](https://github.com/Vitus213/voiceless/actions/runs/45747272309) · [修复后的安装实测](https://github.com/Vitus213/voiceless/actions/runs/45747948970) · [两份安装包](https://github.com/Vitus213/voiceless/actions/runs/45747948970/artifacts/15084691333)

快捷键在 Windows 上验证到了注册这一步。守护进程的低级键盘钩子按设计忽略带 `LLKHF_INJECTED` 标志的事件，软件发送的按键不算用户按键。验收用 `SendInput` 发出 ScrollLock，测试自己的钩子看到的事件都带该标志，守护进程保持 `idle`，页面提示检查绑定或点击「开始试说」。同一组合再次调用 `RegisterHotKey` 返回 1409，同键加其他修饰键和另一个键都成功，说明 Windows 已把该组合登记给 Voiceless。上屏只发送一次 Ctrl+V，并且只发给听写开始时记录、仍是前台且完整性不高于本进程的窗口，交付因此报告为未确认。键盘移除只在状态机单测中验证，安装版没有收到过真实的设备移除通知。 [按键与注册记录](https://github.com/Vitus213/voiceless/actions/runs/45690514706) · [钩子与上屏实现](../rust/src/desktop/windows.rs) · [键盘移除单测](../rust/src/desktop/windows/contract.rs)

在安装版的快捷键设置中，将「点按切换」从 `Ctrl+Shift+F6` 改为另一个进程已占用的 `Ctrl+Shift+F7`。页面报告 Windows 错误 1409，配置文件保留新选择，运行中的配置和 F6 注册保持原样。释放 F7 后点击「重试应用」，同一守护进程注册了 F7，其他进程可以重新注册 F6。 [原生 Windows 验收](https://github.com/Vitus213/voiceless/actions/runs/45750351599) · [两个页面截图及配置状态](https://github.com/Vitus213/voiceless/actions/runs/45750351599/artifacts/15085183090)

在标准账户（中等完整性 `0x2000`）下，两份安装包可安装和卸载。CLI 向同级编辑框粘贴了 83 个字符并恢复剪贴板，面对高完整性窗口时拒绝交付，编辑框及剪贴板内容保持原样。重复启动桌面入口可重开窗口。同一会话的任务栏属于另一个账户，标准账户自己的托盘重开仍待验证。 [标准账户实测](https://github.com/Vitus213/voiceless/actions/runs/45723972884/job/139588997136)

实体按键的开始、停止与取消，实体麦克风试说，以及键盘或 USB 麦克风拔出后的恢复仍待实机验证。安装及听写验收使用 Windows Server 2022，Windows 10 和 11 x64 客户版待测。当前 Windows 安装包仅提供 CPU 识别。
