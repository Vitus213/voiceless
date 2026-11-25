# Voiceless 全链路重构任务

实施以 `main` 的 `v0.1.3` 为基础，按组内顺序交付。每项先固定可复现的行为，再实现并通过所列验证。Linux Rust 测试在 `nix develop .#cpu` 中执行，涉及 GPU 时用对应 Nix shell 或已安装候选包。`frontend` 的测试入口随 1.1 加入，此后按各项新增浏览器场景。

## 1. 以发布版集成新版控制台

- [x] 1.1 `[Requirement: 统一的导航、状态与可访问操作]` `[Scenario: 从旧链接和窄窗口进入控制台]` 用 `v0.1.3` 的 CLI 和原页面锚点建立浏览器验收。应用 `6290af1` 并合并设备选项冲突，构建嵌入式 TypeScript/Vite 资产，保持原链接可到达等价页面。运行 `cargo test --manifest-path rust/Cargo.toml --test console --test cli_identity` 和 `bun run --cwd frontend test:e2e --grep navigation`。验收 `voiceless web`、390 CSS px 导航、焦点可达和全部有效设备选项。
- [x] 1.2 `[Requirement: 现有批量后端枚举扩展]` `[Scenario: 枚举包含 cloud]` `[Scenario: 本地批量和非法后端值]` `[Requirement: 现有流式后端枚举扩展]` `[Scenario: 执行已有实时配置]` `[Scenario: 枚举包含 cloud-vad]` `[Requirement: 现有最终校准后端枚举扩展]` 用现行配置和旧 Python 枚举建立兼容用例。收敛识别入口校验，旧键值在采集前指向迁移说明，四个实时值、两个批量值和当前默认值继续工作。运行 `cargo test --manifest-path rust/Cargo.toml --test config_contract --test batch_transcription --test cloud_realtime_streaming`，核对实时停止没有调用批量接口。

## 2. 统一会话、设备状态与配置

- [x] 2.1 `[Requirement: 多入口共享听写会话]` `[Scenario: 多个入口同时开始]` 在同一测试运行时同时从 D-Bus 与 Web 请求开始。保留单一进程和采集所有者，把 CLI、快捷键及 Web 操作接入同一会话并回报统一 ID。运行 `cargo test --manifest-path rust/Cargo.toml --lib --test daemon_shortcuts --test ctl_relay --test console`，确认只录一次且冲突有可读结果。
- [x] 2.2 `[Requirement: 权威结果与取消]` `[Scenario: 正常停止与重复请求]` `[Scenario: 用户取消或没有说话]` `[Scenario: 录音设备中途断开]` 用尾帧、重复停止、静音、取消和输入设备丢失建立场景。沿用协调器收尾，发布可观察终止阶段并释放采集和预编辑。运行 `cargo test --manifest-path rust/Cargo.toml --test session_finalize --test session_failures --test final_pipeline --test audio_backpressure`，断言最多一次最终交付及失败后可重新录音。
- [x] 2.3 `[Requirement: 客户端重连之后恢复状态]` `[Scenario: 同一运行实例内重连]` `[Scenario: 服务重启后恢复连接]` 在录音中断开 Web 事件流，再模拟服务重启。实现从快照重同步和续接事件，标记旧会话失效并更新认证。运行 `cargo test --manifest-path rust/Cargo.toml --test console` 与 `bun run --cwd frontend test:e2e --grep reconnect`，检查有效状态恢复且无重复交付。
- [x] 2.4 `[Requirement: 分别报告识别回退和计算设备回退]` `[Scenario: 自动模式选中 GPU 或 CPU]` `[Scenario: 指定设备不匹配]` `[Requirement: 当前运行状态与诊断入口]` `[Scenario: 自动设备选择后查看实际结果]` 先固定请求 `auto`、显式设备失败和独立预览的状态断言。由热模型、实际会话和解码器报告设备、执行和回退，接到 API、CLI、听写与诊断页面。运行 `cargo test --manifest-path rust/Cargo.toml --test offline_transport --test console` 与 `bun run --cwd frontend test:e2e --grep effective-device`，真实硬件见 4.3。
- [x] 2.5 `[Requirement: 识别服务选择遵循用户授权]` `[Scenario: 启动前启用已配置备用服务]` `[Scenario: 无授权或会话中途失败]` 用可观察的音频接收日志测试获准备用、拒绝云端及中途断开。只在主服务未接收音频时使用明确配置的备用服务。运行 `cargo test --manifest-path rust/Cargo.toml --test backend_fallback --test session_failures --test cloud_realtime_streaming`，比对实际后端、错误及对各服务发送的音频数量。
- [x] 2.6 `[Requirement: 并行编辑与变更冲突]` `[Scenario: 两个客户端修改同一节的不同字段]` `[Scenario: 同字段被其他客户端或文件编辑器修改]` 在隔离配置上复现 Vulkan 被线程修改覆盖。实现按字段和配置修订合并，以及同字段冲突恢复草稿。运行 `cargo test --manifest-path rust/Cargo.toml --test console` 与 `bun run --cwd frontend test:e2e --grep configuration-conflict`，确认并行修改都保留且过期草稿不能静默覆盖。
- [x] 2.7 `[Requirement: 严格配置与私密数据保护]` `[Scenario: 保存不合法或写入被中断]` `[Scenario: 导出设置和移除凭据]` 增加非法输入、失败写入、注释保留、仅清除指定密钥和脱敏导出的断言。让 CLI、独立 Web 与 daemon 的写入走同一校验及权限约定。运行 `cargo test --manifest-path rust/Cargo.toml --test config_contract --test console`，确认无半写文件、权限错误或密钥返回。
- [x] 2.8 `[Requirement: 配置保存与运行时生效分离]` `[Scenario: 录音中申请应用新设置]` `[Scenario: 新模型或快捷键加载失败]` 先验证录音中提交、过期加载和错误设备。等待空闲并验证候选后切换生效版本，前端显示等待、成功、失败和可恢复结果。运行 `cargo test --manifest-path rust/Cargo.toml --lib --test daemon_shortcuts --test console` 与 `bun run --cwd frontend test:e2e --grep apply-config`，验证旧会话和最后可用配置保持有效。

## 3. Linux 首次试说与桌面听写

- [x] 3.1 `[Requirement: 回环控制台访问和离线运行]` `[Scenario: 断网访问已安装的控制台]` `[Scenario: 非授权访问或写入]` `[Requirement: 当前运行状态与诊断入口]` `[Scenario: 没有常驻服务时访问控制台]` 从断网、其他本地服务占用、非法 Host/Origin/令牌验证控制台入口。保持回环隔离并呈现独立历史状态。通过 `cargo test --manifest-path rust/Cargo.toml --test console`、仅回环网卡上的 `bun run --cwd frontend test:e2e --grep offline-console`，以及实际 PipeWire 回放，验证 SenseVoice CPU 离线识别、历史和录音保存。
- [x] 3.2 `[Requirement: 按平台能力提供麦克风和浮层]` `[Scenario: 切换或断开默认麦克风]` `[Scenario: 合成器不支持浮层或系统功能]` 在测试中模拟默认设备切换与不可用的桌面能力。通过实际适配读取音源、电平和权限，浮层只订阅会话状态而不抢焦点。运行新增用例 `cargo test --manifest-path rust/Cargo.toml --test desktop_contract microphone`，再在 Niri 上拔插设备并核对回落提示和浮层焦点。
- [x] 3.3 `[Requirement: 模型选择与完整性校验]` `[Scenario: 安装、导入和切换兼容模型]` `[Scenario: 下载中断或模型校验失败]` 扩充模型测试的兼容本地文件、磁盘不足和校验失败用例。打通语言及设备筛选、下载/校验进度、可重试错误与新模型激活。运行 `cargo test --manifest-path rust/Cargo.toml --test model_manager --test console`，确认未验证候选及正在使用模型受到保护。
- [x] 3.4 `[Requirement: 全局听写快捷键与冲突报告]` `[Scenario: 录制并试用支持的手势]` `[Scenario: 快捷键被占用或未授权]` 以重复按键、长按松手、点按锁定、取消和新键冲突做测试。统一已选手势对同一会话的指令，保留系统绑定与既有 ScrollLock 设置，并接通原文直出和重贴上次结果。运行 `cargo test --manifest-path rust/Cargo.toml --test daemon_shortcuts --test ctl_relay`，在目标桌面验证实际快捷键及原文直出和重贴绑定。
- [ ] 3.5 `[Requirement: 首次试说验证实际链路]` `[Scenario: 从干净环境完成本地试说]` `[Scenario: 设备、权限或下载失败后重试]` 用空 XDG 配置和预录 PCM 建立首次试说测试。把设备检查、模型准备、快捷键检测和练习框接到实际会话，并继续接纳已有安装。运行 `bun run --cwd frontend test:e2e --grep onboarding`，再以实际麦克风核对一轮失败恢复、无意外上屏及显式测试上屏。
- [x] 3.6 `[Requirement: 统一的导航、状态与可访问操作]` `[Scenario: 通过键盘试说和定位设置]` 用键盘和可访问树测试页面焦点、状态播报及设置搜索。补齐中文和英文、主题选择及减少动画。运行 `bun run --cwd frontend test:e2e --grep accessibility`，在 Orca 中完成一次试说并验收状态播报和弹窗焦点恢复。
- [x] 3.7 `[Requirement: 在正确的目标交付结果]` `[Scenario: 在编辑器和终端交付]` `[Scenario: 焦点改变或交付结果无法确认]` 测试在识别中切换目标、终端输入、Fcitx5 预编辑及不确定反馈，按原目标能力决定交付或保留结果。运行 `cargo test --manifest-path rust/Cargo.toml --test injection_method --test fcitx5_injection --test wtype_injection` 与 `just bridge-test`，实机确认自动上屏只到预期目标，重贴由用户触发。
- [x] 3.8 `[Requirement: 独立窗口、托盘和服务生命周期]` `[Scenario: 从桌面启动器重复打开应用]` `[Requirement: 多入口共享听写会话]` `[Scenario: 关闭窗口时仍在听写]` 给首次桌面启动及重复打开建立验收。用独立 Tauri 窗口与托盘连接已有运行时，应用启动器能聚焦原窗口。运行 `cargo test --manifest-path rust/Cargo.toml --test desktop_contract activation`，从应用启动器打开两次，然后关窗口并通过快捷键完成一次听写。
- [x] 3.9 `[Requirement: 独立窗口、托盘和服务生命周期]` `[Scenario: 无托盘或活动任务期间退出]` 测试无托盘、登录启动被系统管理和退出时有录音的路径。提供可选择的暂停、退出和登录启动操作。运行 `cargo test --manifest-path rust/Cargo.toml --test desktop_contract lifecycle`，分别在有无托盘的桌面确认窗口、启动器与 CLI 的实际操作。

## 4. Linux 发布和升级验收

- [x] 4.1 `[Requirement: 平台交付与运行依赖可验证]` `[Scenario: Linux 发布包离线运行]` `[Scenario: Linux 桌面或跨平台安装]` 先验证 x86-64 与 ARM64 支持范围和动态库，再组装核心 CLI/Web 与独立桌面包。在 Jammy 运行 `python3 -B scripts/release/native.py "$CANDIDATES"` 后运行 `python3 -B scripts/release/validate.py "$CANDIDATES"`。验收无 Node/Bun/WebView 的核心运行路径，以及实际 Linux 桌面安装、启动窗口、托盘和完整试说。 [构建、实机回放与桌面验收](../../../docs/non-nix-distribution.md#020-桌面候选包验收)
- [x] 4.2 `[Requirement: 安装渠道归属和回滚]` `[Scenario: 从 Bun 或 Nix 现有安装升级]` `[Scenario: 更新验证失败或用户回滚]` 为三种安装布局、活动会话、损坏候选和数据备份写升级回归。实现原渠道更新、版本提示、空闲切换及可用回滚。以隔离的 `0.1.3` 安装运行 `voiceless update`、`voiceless update --rollback`（独立包）及 Nix 的原更新入口，核对运行版本和恢复数据。
- [x] 4.3 `[Requirement: 听写延迟和质量可独立复核]` `[Scenario: 本地和云端分别实测]` `[Scenario: 缺少实测环境]` `[Requirement: 发布验证与数据迁移证据]` `[Scenario: 对现有 Linux 部署执行升级验收]` 确定 `0.1.3` 同设备、同音频基线。对候选包复用 `voiceless benchmark asr --split test --backend cloud-realtime --enforce` 和 `voiceless benchmark latency --backend cloud-realtime --mode quick --trials 100 --inject --enforce`。使用现有回放工具分别验证 Apollo Vulkan、无驱动 CPU 和 ARM64 CPU，并演练旧设置与历史升级，报告版本、实际设备、上屏和缺失条件。 [实测记录](../../../docs/non-nix-distribution.md#020-私有候选的-linux-回放2026-11-25)

## 5. 输出流程、术语与历史

- [x] 5.1 `[Requirement: 按目标选择可预览的输出流程]` `[Scenario: 从不同应用听写同一句话]` `[Scenario: 原文直出或处理步骤失败]` 以同一句术语和路径输入验证应用规则、手动覆盖、失败和脱离远程服务。将原有模式映射到可排序步骤并提供逐步对比。运行 `cargo test --manifest-path rust/Cargo.toml --test final_pipeline --test modes_repository --test processing_provider` 与 `bun run --cwd frontend test:e2e --grep output-pipeline`。
- [x] 5.2 `[Requirement: 术语管理与纠错回流]` `[Scenario: 导入术语并从历史纠错补充词库]` `[Scenario: 针对模型查看词条支持]` 用含重名、冲突和中英混合词条的样本覆盖导入与作用阶段。让词库预览和历史纠错使用同一套作用范围规则。运行 `cargo test --manifest-path rust/Cargo.toml --test text_cleanup --test console` 与 `bun run --cwd frontend test:e2e --grep vocabulary`，观察项目范围隔离和词条归属。
- [x] 5.3 `[Requirement: 自动化扩展由用户选择]` `[Scenario: 显式配置远程处理或脚本]` 用禁用、启用、无权限与失败的处理器验证隔离。按显式授权执行 OpenCode、远程处理和限权脚本，为应用及网站上下文显示来源。运行新增用例 `cargo test --manifest-path rust/Cargo.toml --test output_extensions permissions`，以可观测调用确认数据只送到已允许运行的步骤。
- [x] 5.4 `[Requirement: 自动化扩展由用户选择]` `[Scenario: 使用 CLI 或 OpenCode 预览结果]` 验证脚本超时、未经授权请求和无副作用预览。通过 `voiceless mode preview --json` 和 OpenCode 接入同一预览请求，返回每项处理结果与失败原因。运行 `cargo test --manifest-path rust/Cargo.toml --test output_extensions --test cli_workflows preview`，以真实 CLI/API 回执证明试跑不上屏。
- [x] 5.5 `[Requirement: 全量历史搜索与兼容读取]` `[Scenario: 搜索旧记录和跨年记录]` `[Scenario: 分页期间有新录音完成]` 构造含旧格式跨年记录和同一时间戳的多页库。加入服务端搜索、筛选、稳定分页及同查询导出。运行 `cargo test --manifest-path rust/Cargo.toml --test history_store --test console` 与 `bun run --cwd frontend test:e2e --grep history-search`，检查结果不重不漏。
- [x] 5.6 `[Requirement: 回放、重转写与版本恢复]` `[Scenario: 用另一模型重转写并恢复]` `[Scenario: 录音缺失或重转写失败]` 验证原音回放、用户确认替换和恢复。把重转写及纠错回到现有历史条目并标明来源，保持无音频条目的文本操作。运行 `cargo test --manifest-path rust/Cargo.toml --test history_store --test console` 与 `bun run --cwd frontend test:e2e --grep history-revisions`。
- [x] 5.7 `[Requirement: 文本、录音和失败恢复的保留策略]` `[Scenario: 先清理录音再清理文本]` `[Scenario: 导出选择和恢复失败会话]` 用受控时间验证各自保留期限、取消不写成功记录、仅文本导出及失败录音重开。实现保留预览、分步清理和显式导出，文档说明迁移前后的数据权限。运行 `cargo test --manifest-path rust/Cargo.toml --test audio_retention --test history_store --test session_failures` 和 `bun run --cwd frontend test:e2e --grep history-retention`。

## 6. 后台转写和资源竞争

- [x] 6.1 `[Requirement: 批量导入与可验证的转写结果]` `[Scenario: 导入音视频和导出字幕]` `[Scenario: 文件、编解码器或云端传输不可用]` 用混合格式、损坏文件和未许可的云端任务测试部分成功。接上媒体解码、批量导入和支持模型的真实分段时间线，提供 TXT、结构化 JSON、SRT 和 VTT。运行新增用例 `cargo test --manifest-path rust/Cargo.toml --test transcription_jobs import` 与 `bun run --cwd frontend test:e2e --grep media-import`。
- [x] 6.2 `[Requirement: 作业可暂停、取消和恢复]` `[Scenario: 退出界面或重启后恢复长任务]` `[Scenario: 暂停、重试或取消转写]` 以进程重启、暂停中断和取消竞争为场景。持久化已确认的段落，连接重试和重新选择源文件，保证同一段结果只出现一次。运行 `cargo test --manifest-path rust/Cargo.toml --test transcription_jobs recover` 与 `bun run --cwd frontend test:e2e --grep job-recovery`。
- [x] 6.3 `[Requirement: 后台任务让出实时听写资源]` `[Scenario: 长文件处理期间开始听写]` 在长任务占用解码器时回放即时听写，覆盖历史重转写和基准的相同调度入口。提供可观测让出与恢复状态。运行 `cargo test --manifest-path rust/Cargo.toml --test transcription_jobs scheduling`，分别在 Vulkan 和 CPU 上比较竞争前后的延迟和完整结果。

## 7. macOS 和 Windows 运行时及发行

- [ ] 7.1 `[Requirement: 平台交付与运行依赖可验证]` `[Scenario: Linux 桌面或跨平台安装]` `[Requirement: 按平台能力提供麦克风和浮层]` `[Scenario: 切换或断开默认麦克风]` `[Scenario: 合成器不支持浮层或系统功能]` 在 Apple Silicon macOS 上以设备断开、拒绝权限及首次试说做测试。完成 CoreAudio、快捷键、辅助功能授权、菜单栏与上屏适配，签名并对可安装产物验证。运行 `cargo test --manifest-path rust/Cargo.toml --test desktop_contract macos`，从发布候选安装后在原生应用中验收实际麦克风和交付。
- [ ] 7.2 `[Requirement: 平台交付与运行依赖可验证]` `[Scenario: Linux 桌面或跨平台安装]` `[Requirement: 全局听写快捷键与冲突报告]` `[Scenario: 快捷键被占用或未授权]` `[Requirement: 在正确的目标交付结果]` `[Scenario: 焦点改变或交付结果无法确认]` 在 Windows x64 建立权限拒绝、热插拔和焦点变化用例。完成 WASAPI、快捷键和输入适配，产出可安装的桌面和 CLI 包。运行 `cargo test --manifest-path rust/Cargo.toml --test desktop_contract windows`，实际安装验证 CPU 及受支持的 GPU、系统托盘、重开窗口和正确交付。

## 8. 会议和带语音的屏幕反馈

- [ ] 8.1 `[Requirement: 独立的会议采集和恢复]` `[Scenario: 选择会议输入并完成录制]` `[Scenario: 授权拒绝、来源失效或进程中断]` 用合成双轨与进程重启测试来源选择、录音状态和恢复。接入可用的麦克风和系统回环，结束后交给后台队列。运行新增用例 `cargo test --manifest-path rust/Cargo.toml --test capture_workflows meeting_capture`，在发布矩阵中实测成功与权限拒绝。
- [x] 8.2 `[Requirement: 可校对的会议整理]` `[Scenario: 校对会议并选择整理方式]` 用多说话人音频验证带时间段的查看和人工订正。加入本地说话人区分、摘要策略与可校对的要点和导出。运行 `cargo test --manifest-path rust/Cargo.toml --test capture_workflows meeting_review` 与 `bun run --cwd frontend test:e2e --grep meeting-review`，分别核对离线和显式授权的远程输出。
- [x] 8.3 `[Requirement: 带语音的屏幕标注]` `[Scenario: 录制并复核带语音的屏幕反馈]` `[Scenario: 平台不支持屏幕采集或用户取消]` 测试授权、采集区域、批注、转写预览和取消后零发送。实现从系统捕获来源到保存、复制和明确送出的完整交互。运行 `cargo test --manifest-path rust/Cargo.toml --test capture_workflows annotation` 与 `bun run --cwd frontend test:e2e --grep annotation`，在支持的系统验证实际被录制范围和输出内容。

## 9. 整体升级和回归验收

- [ ] 9.1 `[Requirement: 发布验证与数据迁移证据]` `[Scenario: 对现有 Linux 部署执行升级验收]` `[Requirement: 听写延迟和质量可独立复核]` `[Scenario: 本地和云端分别实测]` `[Scenario: 缺少实测环境]` 在已有 Linux 安装上从候选包完成「升级 → 试说 → 自动选设备 → 上屏 → 历史重转写 → 文件排队 → 数据回滚」，再在 macOS 和 Windows 完成各自安装与上屏路径。运行 `just check`、`bun run --cwd frontend test:e2e` 和 4.3 的实测门禁，存放逐平台证据。对已承诺的能力，未验证结果不计入通过。
