# wasm-agent

Rust 原生宿主内嵌 Wasmer/WASIX，以长期运行的 `market.wasm` 验证行情接入。

```text
Herdr（当前不改）
└── Pane
    └── wasm-agent（Rust + Tokio）
        └── Wasmer / WASIX
            └── market.wasm
                ├── TCP / TLS / WebSocket
                ├── 行情订阅、解析、重连、阈值判断
                └── JSON Lines 事件 → 宿主事件队列
```

这次 PoC 验证执行层与事件入口。暂未实现 LLM AgentLoop、Slack、下单或 Herdr 集成。模型后续可以消费 `threshold_crossed`，无需逐笔处理报价。

## 构建与演示

宿主工具链由 `rust-toolchain.toml` 固定为 Rust 1.96.1；宿主 Wasmer 7.4.2 / WASIX 0.704.2。模块独立 Cargo 工程，通过 WASIX 官方依赖仓库构建，两个 `Cargo.lock` 都纳入版本控制。

准备：

- [cargo-wasix 0.1.28](https://github.com/wasix-org/cargo-wasix/releases/tag/v0.1.28)，首次构建自动安装 WASIX Rust 工具链。本次验证使用 `v2026-08-06.1+rust-1.97`。
- 支持 wasm 的 Clang / llvm-ar，例如 [WASI SDK](https://github.com/WebAssembly/wasi-sdk/releases)。macOS 自带 Apple Clang 不支持此目标。
- `cargo +wasix --version` 必须足够新。WASIX 工具链不自带 Cargo，默认 stable 太旧时应更新 stable，或将新版 Cargo 关联到 WASIX 工具链的 `bin/cargo`。

```bash
cargo install cargo-wasix --version 0.1.28 --locked
# 如 SDK 不在项目 .tools 中，设置其目录：
export WASI_SDK_PATH=/path/to/wasi-sdk
bash scripts/demo.sh
```

演示启动本地模拟 WebSocket 行情源，WASM 模块订阅 AAPL，产生模拟报价及越过 100 的通知，5 秒后宿主发送停止命令并等待退出。模拟数据均标记 `source: "mock"`，不代表真实市场价格。

也可分开运行：

```bash
bash scripts/build-market.sh
cargo run --locked --example mock_feed
# 另一个终端
MARKET_ABOVE=100 cargo run --locked -- --allow-network --env MARKET_ABOVE --duration 10
```

`--duration 0` 持续运行，Ctrl-C 停止。网络默认关闭；`--allow-network` 是对受信任 PoC 模块的完整网络授权，尚无域名/IP 白名单。不会继承全部宿主环境或挂载宿主目录；只有 `--env NAME` 指定的变量会传入模块。

## Alpaca 行情

模块包含 Alpaca WebSocket 认证及报价订阅路径，默认 IEX feed。需要自行提供有权限的账户凭据；数据覆盖范围由订阅决定。[官方协议](https://docs.alpaca.markets/us/docs/streaming-market-data)

在本机安全设置 `ALPACA_API_KEY` 和 `ALPACA_API_SECRET`，不要写进仓库，然后执行：

```bash
export MARKET_SOURCE=alpaca
export MARKET_SYMBOL=AAPL
cargo run --locked -- --allow-network --duration 30 \
  --env MARKET_SOURCE --env MARKET_SYMBOL \
  --env ALPACA_API_KEY --env ALPACA_API_SECRET
```

可选 `MARKET_URL` 指定 Alpaca feed（仅允许官方 `wss://stream.data.alpaca.markets/` 地址），`MARKET_ABOVE` 配置买卖价中点的上穿阈值。每个连接中仅在从低于阈值变成大于等于阈值时触发；首笔已高于阈值也触发，重连后重新计算状态。不做历史补数、跨重连去重或交易决策。

没有凭据时，默认自动测试只验证本地模拟行情，不声称已验证真实市场订阅。另已用 WASM 连接官方 `/v2/test`，完成 DNS/TLS/WebSocket 握手并收到预期的未认证 401；这不等于已取得行情权限。

## 模块协议 v1

宿主通过环境变量提供配置，通过 stdin 写入控制命令：

```json
{"version":1,"command":"stop"}
```

模块 stdout 仅输出 JSON Lines，stderr 保留诊断用途：

```json
{"version":1,"kind":"quote","data":{"source":"mock","symbol":"AAPL","bid":100.9,"ask":101.1,"mid":101.0,"timestamp":"fixture-1"}}
```

事件类型：`status`、`quote`、`threshold_crossed`。宿主只验证通用信封，业务协议留在模块。单行上限 64 KiB，事件队列容量 32。

停止流程：stdin 停止命令 → 等待 3 秒 → 超时请求 WASIX SIGKILL 并返回错误。取消基于运行时边界，**不能保证中断完全不调用系统接口的恶意死循环**。WASIX 内部 Pipe 也不是端到端有界缓冲；资源配额、强制 CPU 中断与完整背压是后续工作。这是受信任模块的可行性验证，尚非任意不可信代码的生产沙箱。

## 验证

```bash
bash scripts/check.sh
```

测试直接在宿主内运行编译出的 WASM，覆盖行情过滤、阈值事件、断线重连、空闲连接停止和默认拒绝网络。测试无需外部账号，使用本机随机端口。

已在 macOS / Apple Silicon 验证：3 项端到端测试通过，Clippy 无警告，5 秒一键演示正常退出。`cargo-wasix 0.1.28` 的上游兼容清单 URL 目前返回 404，会打印检测警告；构建仍可成功，以最终退出码为准。编译器的 `atomics` 实验特性提示来自 WASIX 工具链。

## Git-flow

- `main`：发布版本。
- `develop`：集成开发。
- `feature/*`：从 develop 创建，完成后合回 develop。
- `release/*` / `hotfix/*`：发布准备 / 紧急修复。

本次 PoC 在 `feature/market-stream-poc` 开发，验证后合入 `develop`。常用命令：

```bash
git flow feature start <name>
# 编码、验证、提交
git flow feature finish <name>
```

PoC 阶段不创建 release，不直接在 main 开发。
