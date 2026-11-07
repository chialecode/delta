# 官方 Rust ccxt 隔离实测

记录：2026-11-08；实测：2026-11-07；执行者：Agent B；作用域：M1 v2.6 的扩展 POC-D06。此证据替代原阶段报告中误用 `ccxt 0.1.3` 并称官方项目停更的结论。

官方来源：[crates.io ccxt 4.5.91](https://crates.io/crates/ccxt/4.5.91)、[ccxt 官方仓库](https://github.com/ccxt/ccxt)。官方 Rust crate 是活跃项目，MIT 许可；不应以早年的同名 crate 判断其维护状态。

隔离工程位于忽略目录 `.local/poc/ccxt-4.5.91/`，不是产品依赖。直接依赖精确锁定 `ccxt =4.5.91`，禁用默认 features，仅启用 `okx` / `kraken`；解析器锁定 `ccxt-base` / `ccxt-common 4.5.92`，共 252 个包。Windows MSVC / Rust 1.99.1 的首次 dev 构建成功，实测 46,799 ms。

默认 Windows main 线程栈运行失败，退出码 `0xc00000fd`（STACK_OVERFLOW）。加入 `SetErrorMode` 保持静音，再改为 64 MiB 标准库 worker 栈和 boxed future 后，OKX `fetch_ohlcv("BTC/USDT", "1d", limit=2)` 成功返回 2 根，累计 7,981 ms；随后 Kraken 同请求成功返回 2 根，累计 12,031 ms；进程退出 0。这是独立 POC 的真实公共网络请求，不能替代产品原生连接器的在线测试。

官方 `typed.rs` 将 OHLCV 转为 `[f64; 6]`。该接口不能作为本项目 canonical Decimal 的无损行情入口。基于已观察到的精度与线程栈边界，M1 保留原生 REST / ZIP 连接器；通用 ccxt adapter 的无损原始报文路径、栈隔离及受控版本升级登记在 [LOOP-03](../decisions/open-questions.md)。`verify-stage` 对本 POC 标记 `recorded` 并引用本证据，不写成当轮自动执行通过。
