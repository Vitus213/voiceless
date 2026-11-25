# ASR 语音识别基准测试

云端 ASR 候选模型与本地模型的回归测试工具。**测试集与开发集严格分开**，
每次改动后对 `test` 集跑一遍即可检验性能回归。

## 数据组织

```text
corpus/dev.jsonl    开发集（调参/选型用，可反复折腾）
corpus/test.jsonl   测试集（回归用，冻结不动）
audio/<id>.wav      由 qwen3-tts-flash 合成的音频（生成缓存，不入库）
reports/*.json      包含逐条转写或逐次延迟的原始报告
```

语料条目为 JSONL：`{"id", "reference", "voice"}`。`reference` 是送入 TTS 的
原文，即转写的地面真值（ground truth）。开发集 24 条、测试集 12 条，全部为
16 kHz 单声道 PCM16 WAV，覆盖日常口语、数字、专有名词、中英混排、长句、
同音字陷阱、问句等场景；`dev` 用 Cherry/Ethan 双音色，`test` 用 Serena 音色，
避免音色泄露。

## 用法

```bash
# 1. 在仓库根目录补齐冻结语料的 WAV，凭据从环境读取
nix develop --command python scripts/asr_benchmark/generate.py --split test

# 2. 使用已配置的 DashScope 实时服务，逐条比较参考文本
voiceless benchmark asr --split test --backend cloud-realtime --enforce

# 3. 在专用测试输入框中接收上屏结果，运行 100 次完整听写
voiceless benchmark latency --backend cloud-realtime --mode quick --trials 100 --inject --enforce
```

`--inject` 会把结果输入当前焦点窗口。运行延迟门禁前先聚焦专用输入框。公开版 `0.1.3` 的延迟命令默认使用云端实时和 quick 模式，省略 `--backend` 和 `--mode` 即可运行。

## 2026-11-25 云端实时验收

两次均在 Apollo 上使用模型 `qwen3-asr-flash-realtime` 和语料 `corpus-d2a9e20453ce7074`。准确度取 12 条测试音频的平均内容 CER。延迟来自 100 次独立的 quick 听写，每次都执行上屏。两版的质量和延迟门禁均通过。

| Linux x86-64 产物 | 内容 CER | 首字 P95 | 停录到定稿 P50 | 停录到上屏 P99 | 原始报告 |
| --- | ---: | ---: | ---: | ---: | --- |
| 公开版 `0.1.3` | 0.704% | 457 ms | 267 ms | 593 ms | [逐条转写](reports/cloud-realtime-test-2026-11-25T06-56-39.833539774Z.json) · [100 次延迟](reports/latency-cloud-realtime-2026-11-25T07-14-10.184849101Z.json) |
| 私有候选 `0.2.0` | 0.704% | 871 ms | 270 ms | 657 ms | [逐条转写](reports/cloud-realtime-test-2026-11-25T08-33-06.194466877Z.json) · [100 次延迟](reports/latency-cloud-realtime-2026-11-25T08-49-20.44628402Z.json) |

两轮的 ELF SHA-256 分别为 `940a3df4cc69157db441a71267f97d1c74a232b15c20938359f738004b380661` 和 `c56e81f26182d56261fd24af352058f0db2f344430ea080b8fe17ddaf8ddeb2f`。门槛为内容 CER 2%、首字 P95 1 秒、停录到定稿 P50 600 毫秒、停录到上屏 P99 1 秒。每版只进行了一轮云端延迟测试。

## 本地远讲与流式延迟

先准备所选语料的 WAV，安装 `sense-voice-int8` 和 `silero-vad`。测试降噪时还需安装 `gtcrn`。

```bash
python3 scripts/asr_benchmark/replay_local.py --binary ./rust/target/debug/voiceless --split test
python3 scripts/asr_benchmark/replay_local.py --binary ./rust/target/debug/voiceless --split test --denoise
```

脚本用 `ffmpeg` 转换到 16 kHz PCM，以 32 毫秒节奏驱动独立的 `voiceless stream` 进程。
送完最后一帧后，脚本向该次 CLI 发送一次 SIGINT，`stop_final` 从发送信号计到收到定稿。
它按原音、两档模拟远讲和纯噪声输出 JSONL。每行包含采集启动耗时、录音开始到首条非空预览的耗时、
录音开始、预览更新和录音结束之间的最大间隔（无预览时等于录音时长）、停录到定稿的耗时、内容 CER 和最终文本。
样本可用 `--sample` 缩小，条件可用 `--condition` 缩小。比较不同构建时使用同一语料、模型和机器。纯噪声行的 CER 为 `null`，
可用非空文本计数检查误触发。此回放包含独立进程启动和模型校验，测量点为 CLI 转写事件。

用 `--model qwen3-asr-0.6b-int8 --fast-preview` 可在标准 CPU 构建上验证 SenseVoice 预览与 Qwen ONNX 定稿。
用 `--model qwen3-asr-1.7b-gguf --fast-preview` 可对照 GGUF 定稿，需使用支持 GGUF 的构建。
运行前安装所选的定稿模型与 `sense-voice-int8`。
对照 Vulkan 与 CPU 时，对同一个 Vulkan 构建分别传入 `--provider vulkan` 和 `--provider cpu`。

## 度量

- **内容 CER**：忽略标点与空白（NFKC、小写）后的字符编辑距离 / 参考长度 —— 识别准确度。
- **格式 CER**：保留标点、忽略空白 —— 标点结构准确度。
- **英文 WER**：参考文本中 ASCII 单词（含数字）的词错误率，衡量中英混排能力。
- **平均时延 / RTF**：单样本 API 往返秒数；RTF = 时延 / 音频时长。

## 2026-08-13 初测排名（all 集，36 样本，TTS 合成语音）

| 排名 | 模型 | 内容CER | 格式CER | 平均时延(s) | 说明 |
| --- | --- | --- | --- | --- | --- |
| 1 | `qwen3.5-omni-flash` | 0.0089 | 0.0236 | 0.88 | 云端；需固定“只转写”系统提示，问句会触发聊天回复 |
| 2 | `qwen3-omni-flash` | 0.0090 | 0.0347 | 0.86 | 云端；同上 |
| 3 | `qwen3-asr-flash-2026-02-10` | 0.0110 | 0.0209 | 0.64 | 云端；专用 ASR，无聊天漂移，最快，默认推荐 |
| 4 | `qwen3-sherpa`（本地） | 0.0229 | 0.0331 | 1.04 | 离线基线 |
| 5 | `sensevoice`（本地） | 0.1008 | 0.1132 | 0.14 | 离线草稿/流式基线 |

要点：云端模型大幅领先本地；文本次要差异集中在英文 token（TTS 合成英文语音
本身易被各模型误听，含测试口径噪声）。日常新语料跑 `--split test` 对比上表即可。

## 冻结基线（test 集，2026-08-13，12 样本 / 68.7 秒）

| 模型 | 内容CER | 格式CER | 平均时延(s) | RTF |
| --- | --- | --- | --- | --- |
| `qwen3-asr-flash-2026-02-10`（默认） | 0.0115 | 0.0202 | 0.62 | 0.009 |
| `qwen3-omni-flash` | 0.0115 | 0.0446 | 0.73 | 0.011 |
| `qwen3.5-omni-flash` | 0.0115 | 0.0325 | 0.83 | 0.012 |
| `qwen3-sherpa`（本地） | 0.0144 | 0.0293 | 1.21 | 0.018 |
| `sensevoice`（本地） | 0.1016 | 0.1276 | 0.20 | 0.003 |

原报告：`reports/test-2026-08-13.json`（生成命令见上文）。

## 回归判据

同一 `test` 集下，新一轮报告的内容 CER 不得比上一轮劣化超过 0.005
（约等于一条 20 字样本多错 1 字）；格式化 CER 同理。超限即回归。