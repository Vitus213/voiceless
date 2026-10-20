# Spec Delta

## ADDED Requirements

### Requirement: 原生 Rust CLI 分发

Voiceless SHALL 作为原生 Rust 可执行文件分发，并在没有 Python 解释器或 Python 包环境的运行环境中提供核心 CLI。安装包 MUST 提供 `stream`、`transcribe`、`record`、`model`、`doctor`、`mode`、`history` 和 `daemon` 子命令；本 change 完成后 MUST 不再提供 Adwaita/GTK 桌面应用入口。

#### Scenario: 无 Python 环境运行核心 CLI

- **WHEN** 用户在安装了 Voiceless 运行时依赖但没有 Python 解释器和项目 Python 包的环境中调用 `voiceless --help` 与 `voiceless doctor`
- **THEN** 两个命令 MUST 由同一原生可执行文件运行，且帮助中 MUST 列出全部核心子命令

#### Scenario: GUI 入口被移除

- **WHEN** 用户调用旧桌面入口或查阅安装产物
- **THEN** 安装产物 MUST 不提供 GTK/Adwaita 桌面进程、旧桌面命令或把 GUI 描述为当前能力

### Requirement: 严格配置与后端选择

Voiceless SHALL 从 `voiceless` XDG 配置目录读取严格 TOML。未知节、未知键、非法枚举、越界数值和与所选后端不相容的配置 MUST 在启动会话前产生可诊断错误。云端密钥 MUST 从权限不宽于 `0600` 的配置文件读取，且 MUST 不出现在普通输出、JSON 事件、历史或日志中。

#### Scenario: 未知配置在采集前失败

- **WHEN** 配置包含未知键或非法实时后端
- **THEN** 命令 MUST 在启动音频采集或网络连接前以非零状态退出，并指出无效字段

#### Scenario: 配置文件权限过宽

- **WHEN** 配置包含云端密钥且文件权限允许同组或其他用户读取
- **THEN** 云端会话 MUST 在网络连接前失败并给出收紧权限的诊断，且输出 MUST 不包含密钥值

### Requirement: 核心数据工作流迁移

Voiceless SHALL 保留经过完整性校验的模型安装/检查/移除、用户模式、历史记录和 headless 快捷键 daemon 工作流。显式模型检查 MUST 重新验证文件；损坏模型 MUST 不可用于新会话。`quick` 模式 MUST 绕过外部文本处理；其他模式的处理失败 MUST 保留原识别文本并产生 warning。

#### Scenario: 显式检查发现模型损坏

- **WHEN** 已安装模型的有效载荷在安装后被修改并执行 `voiceless model check <id>`
- **THEN** 检查 MUST 失败，后续依赖该模型的会话 MUST 不使用此前缓存的路径

#### Scenario: quick 模式不调用文本处理服务

- **WHEN** 用户使用 `--mode quick` 完成一个成功识别会话
- **THEN** 最终输出 MUST 直接来自识别结果，且 MUST 不发起外部文本处理请求

#### Scenario: 自定义模式处理失败

- **WHEN** 非 quick 模式的文本处理请求失败
- **THEN** Voiceless MUST 发布 warning、保留识别文本并按既有一次输出语义完成会话

### Requirement: Headless 快捷键会话控制

`voiceless daemon` SHALL 通过受支持的全局快捷键入口控制同一会话的开始和正常停止。首次激活 MUST 开始采集；再次激活 MUST 触发与 CLI 第一次 `SIGINT` 相同的正常停止语义；重复或竞态激活 MUST 不创建并发会话或重复注入。

#### Scenario: 第二次快捷键激活正常停止

- **WHEN** daemon 已有活动会话且收到同一快捷键的第二次激活
- **THEN** daemon MUST 停止采集、冲刷该会话并最多注入一次最终文本

#### Scenario: 会话结束前重复开始

- **WHEN** 活动会话尚未完成且收到额外开始请求
- **THEN** daemon MUST 不创建第二个采集或识别会话，并 MUST 给出可诊断状态

## MODIFIED Requirements

无。

## REMOVED Requirements

无。

## RENAMED Requirements

无。
