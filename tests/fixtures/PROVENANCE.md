# 测试夹具来源与可复现说明（Fixtures Provenance）

更新：2026-11-08，Agent B。本目录保存 M1 确定性回放夹具，包括第三方日历数据、按公开接口结构摘录/构造的行情 JSON，以及合成账单。没有个人真实账户、密钥或私人样本。合成并不表示来自真实账户后再脱敏；`ACT-03` 真实平台样本仍未测。

## 1. 交易日历夹具（`tests/fixtures/calendar/`）

| 文件 | 覆盖区间 | 来源说明 |
| --- | --- | --- |
| `xnys_1990_2023.csv` | `1990-01-02` ～ `2023-01-13` | 来源：[exchange_calendars 的 tests/resources/xnys.csv](https://github.com/gerrymanoim/exchange_calendars/blob/master/tests/resources/xnys.csv)，8,324 个交易日含开收盘 UTC 时间戳。保留上游 [Apache-2.0 许可](https://github.com/gerrymanoim/exchange_calendars/blob/master/LICENSE)；使用其数据文件，不引入 Python 运行期。 |
| `xnys_2023_2027.csv` | `2023-01-03` ～ `2027-12-31` | 按 [NYSE 官方假日与交易时间](https://www.nyse.com/markets/hours-calendars) 规则生成的对照表，含 2025-01-09 国家哀悼日。属于规则生成数据，不是交易所逐日原始导出；与实现逐日比较，不能把这一部分称为独立线上录制。 |

两份文件用于 S-06 / POC-D02 的开闭市、UTC 开收盘、半日市与 DST 对照；重叠区間必须一致。`XNAS` 同范围在当前实现中采用已声明相同的美股规则，并对照同一基准，未引入独立 NASDAQ 历史逐日档案。文件内容和内置特殊表由门禁源码指纹绑定。

## 2. 行情、公司行动与汇率夹具（`tests/fixtures/market_data/`）

以下 JSON 没有完整的采集时间、HTTP 响应头与原始响应哈希链，因此只作为公开接口结构与指定金额的测试夹具，不能一律称为完整真实线上录制，也不能用于证明实际历史成交量。A 的公共实测事实见 [2026-11-07 调研](../../docs/evidence/2026-11-07-stack-and-data-research.md)，本轮真实网络结果独立见 [阶段报告](../../docs/evidence/2026-11-08-m1-stage-report.md#3-实际验证)。

| 文件 | 所模拟/摘录的公开结构 | 关键验证点 |
| --- | --- | --- |
| `yahoo_aapl_2020_split.json` | Yahoo Chart v8 (`events=div,split`) | `AAPL` `2020-08-28` 拆股调整后收盘 `124.80750274658203` 与 `2020-08-31` `4:1` 拆股事件，S-08 还原 `raw` 收盘 `499.23`、成交量 `/ 4`、2020-08-07 分红 `0.205 * 4 = 0.82`。 |
| `eastmoney_aapl_2020_raw.json` | 东方财富 `push2his` (`secid=105.AAPL`, `fqt=0`) | `AAPL` `2020-08-28` 原始未复权收盘 `499.23`，与 Yahoo 还原值 0 bps 偏差。 |
| `tiingo_aapl_2020_raw.json` | Tiingo Daily (`AAPL/prices`) | `AAPL` 原始未复权 OHLCV 与 `splitFactor=4.0`、`divCash=0.82`。 |
| `alpaca_aapl_corporate_actions.json` | Alpaca `/v1/corporate-actions` | `AAPL` `forward_splits` (`4:1`) 与 `cash_dividends` (`0.82`) 交叉验证。 |
| `yahoo_nvda_2024_split.json` | Yahoo Chart v8 (`NVDA`) | `NVDA` `2024-06-07` 拆股前收盘 `120.888 * 10 = 1208.88` 与 `2024-06-10` `10:1` 拆股。 |
| `eastmoney_nvda_2024_raw.json` | 东方财富 `push2his` (`secid=105.NVDA`, `fqt=0`) | `NVDA` `2024-06-07` 原始未复权收盘 `1208.88`。 |
| `yahoo_spy_dividend.json` | Yahoo Chart v8 (`SPY`) | `SPY` 现金分红事件与 ETF 日线。 |
| `okx_btc_usdt_candles.json` | OKX `/api/v5/market/history-candles` (`bar=1Dutc`) | 含 `confirm="1"` 已完结日线与 `confirm="0"` 未完结当日 K 线（`is_final=false`）。 |
| `binance_btc_usdt_klines.json` | Binance `/api/v3/klines` (`interval=1d`) | `BTCUSDT` UTC 日线与成交额字段。 |
| `coinbase_usdt_usd_candles.json` | Coinbase `/products/USDT-USD/candles` | `USDT/USD` 夹具值 `0.9998` / `0.9993`，验证 S-07 不硬编码 `1.0`；这些数值不作为线上历史报价证据。 |
| `bitstamp_usdt_usd_ohlc.json` | Bitstamp `/api/v2/ohlc/usdtusd/` | `USDT/USD` 校验源（与 Coinbase 偏差 `< 50 bps`）。 |
| `bitstamp_usdc_usd_ohlc.json` | Bitstamp `/api/v2/ohlc/usdcusd/` | `USDC/USD` 主源（`1.0001` / `0.9999`）。 |
| `kraken_usdc_usd_ohlc.json` | Kraken `/0/public/OHLC?pair=USDCUSD` | `USDC/USD` 校验源。 |
| `frankfurter_usd_fx.json` | Frankfurter `/v1/2026-01-02..2026-01-06` | 只含发布日字段的构造报价，用未出现的周末日期验证 S-07 最多 4 日 `carried_forward` 与同版本对称反算；不声明这些数值为 ECB 实际历史报价。 |
| `ledger_fx_synthetic.json` | 内部 `FxRateRecord`，`source=synthetic-ledger-test` | 完全合成的账本入账汇率，补 USDT、OKB、BNB 和 HKD 的事件 FX；不来自线上供应商，不可用于真实资金估值。 |

数据供应商自己的条款仍适用，接口开放访问不表示获得再分发许可。Tiingo/Alpaca JSON 只验证离线结构与金融语义；没有使用私有 Key 运行真实服务。

## 3. 合成账单样本（`tests/fixtures/statements/`）

所有 CSV/XML 都由开发者声明的模板构造，账户和业务 ID 为测试字符串；平台预设名不表示兼容该平台的全部真实原生格式。Futu 模板默认纽约时区，OKX/币安为 UTC；模板 `fee` 表示非负支出，真实原生 API 的有符号费字段在独立 parser 中转换。

| 文件 | 预设与格式 | 覆盖场景 |
| --- | --- | --- |
| `futu_cn_sample.csv` / `futu_en_sample.csv` | `futu`（中英文表头） | USD 入金、买入、部分卖出与费用列；这两个文件没有拆股或现金分红行。 |
| `okx_billing_sample.csv` | `okx` | USDT 入金、OKB 买入、BTC-USDT 现货买卖及第三资产 OKB 手续费；OKB 余额由买入产生。 |
| `generic_full_lifecycle.csv` | `generic` | 多币种入金、买入、拆股、部分卖出、现金分红、跨账户转账完整生命周期。 |
| `ibkr_flex_sample.xml` | `ibkr`（Flex XML） | `<Trades>`、`<CashTransactions>`、`<CorporateActions>` 结构化导入。 |
| `binance_trade_history.csv` | `binance` | USDT 入金、BNB 买入、BTCUSDT 现货成交及独立 BNB 费用字段；文件中没有带单位的数量字符串。 |
| `tiger_sample.csv` / `longbridge_sample.csv` / `schwab_sample.csv` / `eastmoney_sample.csv` / `ibkr_activity_sample.csv` | M1-EXT 扩展预设 | 老虎、长桥、嘉信、东方财富、IBKR Activity CSV 表头校验与预览入库。 |
| `sample_opening_cash.csv` | `generic` | 完全合成、显式的一笔 USD 期初入金，供只含成交的模板测试使用；不代表真实余额，宿主按调用方账户绑定。 |

## 4. 代码内构造夹具

`crates/delta-app/src/regression.rs` 另行构造独立 Futu/OKX/币安资金流水、去重与更正、多账户 Flex、OKX/币安原生结构余额/成交/已完成充提数组、Binance ZIP/CHECKSUM、分页/429/取消和第三资产费用。它们是可审查的纯合成测试数据；`FixtureAccountProvider` 的 recorded 接口名只说明可回放报文，不证明这些构造数组来自真实账户。

回环 HTTP 服务只绑定 `127.0.0.1:0`；六类指标、现金 FIFO、账户在途与外币报告使用独立手算期望及未来哨兵。离线测试通过、公共 online 探针通过和真实私有账户兼容性分别报告。
