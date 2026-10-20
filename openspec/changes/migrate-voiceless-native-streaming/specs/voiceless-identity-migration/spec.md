# Spec Delta

## ADDED Requirements

### Requirement: Voiceless 唯一对外身份

安装后的产品 SHALL 以 `Voiceless` 作为应用身份并以 `voiceless` 作为唯一 CLI 名称。Nix 包、Home Manager 选项、桌面元数据、D-Bus/Portal 标识、帮助文本和文档 MUST 使用 Voiceless；旧 `type4me-linux` 命令、Python 分发名、Nix 属性、Home Manager 选项和 D-Bus 标识 MUST 不作为兼容入口保留。

#### Scenario: 新 CLI 和系统集成可识别

- **WHEN** 用户安装包并调用 `voiceless --help`
- **THEN** 命令 MUST 成功启动并显示 Voiceless 身份，且 Nix、Home Manager、桌面和 D-Bus/Portal 元数据 MUST 使用 Voiceless 标识

#### Scenario: 旧入口不再可用

- **WHEN** 用户在切换后调用 `type4me-linux` 或尝试导入原 Python 包
- **THEN** 安装产物 MUST 不提供指向 Voiceless 的命令别名、Python 兼容包或重导出

### Requirement: 干净的持久化状态边界

Voiceless SHALL 仅在 `voiceless` 命名的 XDG 配置、数据、缓存、状态和运行时目录中读取或写入配置、模型、词汇和历史。Voiceless MUST 不自动读取、移动、删除或导入旧 `type4me-linux` 目录；迁移说明 MUST 明确旧目录保留和用户手动迁移的边界。

#### Scenario: 仅存在旧状态目录时首次启动

- **WHEN** 用户环境中只存在旧 `type4me-linux` XDG 状态而不存在 Voiceless 状态
- **THEN** Voiceless MUST 创建并使用自己的目录，且旧目录内容 MUST 保持未修改

#### Scenario: 用户需要迁移既有数据

- **WHEN** 用户查阅 Voiceless 迁移说明
- **THEN** 说明 MUST 明确旧配置、模型、词汇和历史不会自动导入，并给出手动备份或迁移前提

## MODIFIED Requirements

无。

## REMOVED Requirements

无。

## RENAMED Requirements

无。
